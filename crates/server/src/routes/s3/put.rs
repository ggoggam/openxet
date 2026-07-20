//! PutObject / CopyObject / DeleteObject: the S3 gateway write path (Phase 2).
//!
//! This is where the interop shim earns the *other* half of its cost. Phase 1
//! could only serve files that the Xet client had already chunked; here the
//! server does that chunking itself: it content-defines chunks over the
//! incoming object bytes (the same gear-hash CDC the client uses), packs them
//! into xorbs, records a shard describing the file's reconstruction, and
//! registers the `(bucket, key)` name — all reusing the exact structures the
//! read path and the native `/v1/xorbs` + `/v1/shards` endpoints already speak,
//! so an object written here dedups against and reconstructs identically to one
//! written by the client.
//!
//! Scope is single-shot: the whole object arrives in one request body, streamed
//! so peak memory stays near one xorb (~64 MiB) rather than the whole file.
//! Multipart upload is a later phase.
//!
//! Ownership note: PutObject/CopyObject record an ownership claim on the file so
//! it survives GC. DeleteObject removes only the `(bucket, key)` name — it does
//! *not* release the claim, because claims are keyed by `(owner, file_hash)` and
//! one owner may name the same content under several keys, so releasing on
//! delete could orphan content another key still points at. The cost is that a
//! deleted (or overwritten) object's bytes stay owned until precise per-name
//! refcounting lands in a later phase; this is conservative (never deletes
//! still-named content) rather than leaky-unsafe.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use md5::{Digest, Md5};

use xet_core_structures::merklehash::{MerkleHash, compute_data_hash, file_hash};
use xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks;
use xet_core_structures::metadata_shard::file_structs::{
    FileDataSequenceEntry, FileDataSequenceHeader, FileVerificationEntry, MDBFileInfo,
};
use xet_core_structures::metadata_shard::shard_in_memory::MDBInMemoryShard;
use xet_core_structures::metadata_shard::{MDBShardFileHeader, MDBShardInfo};
use xet_core_structures::xorb_object::constants::{MAX_XORB_BYTES, MAX_XORB_CHUNKS};
use xet_core_structures::xorb_object::{Chunk, RawXorbData, SerializedXorbObject};
use xet_data::deduplication::Chunker;

use crate::error::AppError;
use crate::routes::xorb::ingest_xorb;
use crate::state::AppState;
use crate::storage::{FileIndex, OwnershipClaim, S3Object, StorageBackend};

use super::error::{S3Error, from_app_error, xml_escape};
use super::format_iso8601;
use super::sigv4::S3Auth;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- aws-chunked (streaming SigV4) de-framing --------------------------------

/// Longest chunk-header line we will buffer before declaring the body
/// malformed. Real headers are `<hex-size>[;chunk-signature=<64 hex>]` — well
/// under this — so the cap only bounds a hostile/garbled stream.
const MAX_CHUNK_HEADER_LEN: usize = 1024;

/// State machine that strips AWS `aws-chunked` transfer framing from a request
/// body. When a signed `aws s3` client uploads, the payload is wrapped as a
/// series of `<hex-size>[;chunk-signature=…]\r\n<data>\r\n` frames ending in a
/// zero-length chunk (plus optional trailers). Without stripping this, the
/// framing bytes would be chunked as if they were object content.
///
/// Per-chunk signatures are *not* verified: the request's header signature has
/// already authenticated the caller, and this gateway is an interop shim, not a
/// tamper-evident channel (documented trade-off, same spirit as trusting
/// `x-amz-content-sha256` verbatim in [`super::sigv4`]).
struct AwsChunkedDecoder {
    enabled: bool,
    buf: BytesMut,
    state: DecoderState,
    remaining: usize,
}

#[derive(PartialEq)]
enum DecoderState {
    Header,
    Data,
    Crlf,
    Done,
}

impl AwsChunkedDecoder {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            buf: BytesMut::new(),
            state: DecoderState::Header,
            remaining: 0,
        }
    }

    /// Feed one input frame, appending any decoded payload bytes to `out`.
    fn feed(&mut self, input: &[u8], out: &mut Vec<Bytes>) -> Result<(), AppError> {
        if !self.enabled {
            if !input.is_empty() {
                out.push(Bytes::copy_from_slice(input));
            }
            return Ok(());
        }
        self.buf.extend_from_slice(input);

        loop {
            match self.state {
                DecoderState::Done => break,
                DecoderState::Header => {
                    let Some(pos) = find_crlf(&self.buf) else {
                        if self.buf.len() > MAX_CHUNK_HEADER_LEN {
                            return Err(AppError::BadRequest(
                                "aws-chunked header line too long".into(),
                            ));
                        }
                        break; // need more input
                    };
                    let line = self.buf.split_to(pos);
                    self.buf.advance(2); // consume the CRLF
                    let size = parse_chunk_size(&line)?;
                    if size == 0 {
                        // Final chunk; ignore any trailer headers that follow.
                        self.state = DecoderState::Done;
                        break;
                    }
                    self.remaining = size;
                    self.state = DecoderState::Data;
                }
                DecoderState::Data => {
                    if self.buf.is_empty() {
                        break;
                    }
                    let take = self.remaining.min(self.buf.len());
                    out.push(self.buf.split_to(take).freeze());
                    self.remaining -= take;
                    if self.remaining == 0 {
                        self.state = DecoderState::Crlf;
                    }
                }
                DecoderState::Crlf => {
                    if self.buf.len() < 2 {
                        break;
                    }
                    if &self.buf[..2] != b"\r\n" {
                        return Err(AppError::BadRequest(
                            "malformed aws-chunked chunk terminator".into(),
                        ));
                    }
                    self.buf.advance(2);
                    self.state = DecoderState::Header;
                }
            }
        }
        Ok(())
    }

    /// Assert the framed body ended cleanly (saw its zero-length chunk).
    fn finish(&self) -> Result<(), AppError> {
        if self.enabled && self.state != DecoderState::Done {
            return Err(AppError::BadRequest(
                "aws-chunked body ended before its final chunk".into(),
            ));
        }
        Ok(())
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Parse the size out of a chunk header line: hex digits up to an optional
/// `;chunk-signature=…` extension.
fn parse_chunk_size(line: &[u8]) -> Result<usize, AppError> {
    let field = line.split(|&b| b == b';').next().unwrap_or(line);
    let s = std::str::from_utf8(field)
        .map_err(|_| AppError::BadRequest("non-utf8 aws-chunked header".into()))?
        .trim();
    usize::from_str_radix(s, 16)
        .map_err(|_| AppError::BadRequest("invalid aws-chunked chunk size".into()))
}

// ---- server-side chunk → xorb → shard assembly -------------------------------

/// Accumulates content-defined chunks into xorbs and a file reconstruction as
/// object bytes stream in, cutting a xorb whenever the next chunk would exceed
/// the xorb byte or chunk-count cap so peak memory stays near one xorb.
struct ObjectBuilder<'a> {
    state: &'a AppState,
    /// (chunk_hash, unpacked_size) for every chunk in file order — the input to
    /// the file hash.
    file_chunks: Vec<(MerkleHash, u64)>,
    /// One reconstruction term per emitted xorb (no cross-file dedup in this
    /// phase, so each xorb the writer produces is one contiguous term).
    segments: Vec<FileDataSequenceEntry>,
    verifications: Vec<FileVerificationEntry>,
    /// Chunks staged for the xorb currently being filled.
    cur: Vec<Chunk>,
    cur_bytes: usize,
}

impl<'a> ObjectBuilder<'a> {
    fn new(state: &'a AppState) -> Self {
        Self {
            state,
            file_chunks: Vec::new(),
            segments: Vec::new(),
            verifications: Vec::new(),
            cur: Vec::new(),
            cur_bytes: 0,
        }
    }

    /// Add one content-defined chunk, flushing the staged xorb first if this
    /// chunk would push it past a cap.
    async fn absorb(&mut self, chunk: Chunk) -> Result<(), AppError> {
        let len = chunk.data.len();
        if !self.cur.is_empty()
            && (self.cur_bytes + len > *MAX_XORB_BYTES || self.cur.len() + 1 > *MAX_XORB_CHUNKS)
        {
            self.flush().await?;
        }
        self.file_chunks.push((chunk.hash, len as u64));
        self.cur_bytes += len;
        self.cur.push(chunk);
        Ok(())
    }

    /// Serialize the staged chunks into a xorb, ingest it (store + index), and
    /// append the reconstruction term it covers.
    async fn flush(&mut self) -> Result<(), AppError> {
        if self.cur.is_empty() {
            return Ok(());
        }
        let chunks = std::mem::take(&mut self.cur);
        self.cur_bytes = 0;

        // A term maps to one xorb, capped at MAX_XORB_BYTES (64 MiB), so the
        // unpacked byte count and chunk count both fit in u32 (the field types).
        let unpacked: u32 = chunks.iter().map(|c| c.data.len() as u32).sum();
        let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|c| c.hash).collect();
        let num_chunks = chunks.len();

        // Single file's worth of chunks, so a single file boundary at 0.
        let raw = RawXorbData::from_chunks(&chunks, vec![0]);
        let serialized = SerializedXorbObject::from_xorb(raw, true)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing xorb: {e}")))?;
        let xorb_hash = serialized.hash;
        let xorb_hash_hex = xorb_hash.hex();

        ingest_xorb(
            self.state,
            &xorb_hash_hex,
            Bytes::from(serialized.serialized_data),
        )
        .await?;

        self.segments.push(FileDataSequenceEntry::new(
            xorb_hash,
            unpacked,
            0u32,
            num_chunks as u32,
        ));
        self.verifications
            .push(FileVerificationEntry::new(range_hash_from_chunks(
                &chunk_hashes,
            )));
        Ok(())
    }

    /// Finalize: flush the last xorb, then build and store the file's shard in
    /// the upload wire format (footer stripped), returning `(file_hash, shard_hash)`.
    async fn finalize(mut self) -> Result<(String, String), AppError> {
        self.flush().await?;

        let file_mh = file_hash(&self.file_chunks);
        let file_hash_hex = file_mh.hex();

        let header = FileDataSequenceHeader::new(file_mh, self.segments.len(), true, false);
        let file_info = MDBFileInfo {
            metadata: header,
            segments: self.segments,
            verification: self.verifications,
            metadata_ext: None,
        };

        let mut mem = MDBInMemoryShard::default();
        mem.add_file_reconstruction_info(file_info)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("assembling shard: {e}")))?;

        let shard_bytes = serialize_upload_shard(&mem)?;
        let shard_hash_hex = compute_data_hash(&shard_bytes).hex();
        self.state
            .storage
            .put_shard(&shard_hash_hex, Bytes::from(shard_bytes))
            .await?;

        Ok((file_hash_hex, shard_hash_hex))
    }
}

/// Serialize an in-memory shard into the *upload* wire format the server stores
/// and the reader expects: header + file-info + xorb-info sections, footer and
/// lookup tables stripped, and the header's `footer_size` rewritten to 0. This
/// mirrors xet-core's own `read_shard_to_bytes_remove_footer`: the footer
/// begins at `file_lookup_offset`, so truncate there.
fn serialize_upload_shard(mem: &MDBInMemoryShard) -> Result<Vec<u8>, AppError> {
    let mut full = Vec::new();
    let info = MDBShardInfo::serialize_from(&mut full, mem, None)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing shard: {e}")))?;

    let split = info.metadata.file_lookup_offset as usize;
    if split > full.len() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "shard footer offset {split} beyond serialized length {}",
            full.len()
        )));
    }
    full.truncate(split);

    // Rewrite the header in place with footer_size = 0.
    let header = MDBShardFileHeader {
        footer_size: 0,
        ..Default::default()
    };
    let mut hbuf = Vec::new();
    header
        .serialize(&mut hbuf)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing shard header: {e}")))?;
    if hbuf.len() > full.len() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "shard shorter than its header"
        )));
    }
    full[..hbuf.len()].copy_from_slice(&hbuf);
    Ok(full)
}

/// Stream the request body (de-framing aws-chunked if needed), chunk it, build
/// its xorbs and shard, and register the file. Returns
/// `(file_hash, size, md5_hex)`. `md5_hex` is the object's S3 ETag.
async fn ingest_object(
    state: &AppState,
    owner: &str,
    aws_chunked: bool,
    body: Body,
) -> Result<(String, u64, String), AppError> {
    let mut stream = body.into_data_stream();
    let mut decoder = AwsChunkedDecoder::new(aws_chunked);
    let mut chunker = Chunker::default();
    let mut hasher = Md5::new();
    let mut builder = ObjectBuilder::new(state);
    let mut total: u64 = 0;

    while let Some(frame) = stream.next().await {
        let frame =
            frame.map_err(|e| AppError::BadRequest(format!("error reading request body: {e}")))?;
        let mut decoded = Vec::new();
        decoder.feed(&frame, &mut decoded)?;
        for data in decoded {
            hasher.update(&data);
            total += data.len() as u64;
            for chunk in chunker.next_block_bytes(&data, false) {
                builder.absorb(chunk).await?;
            }
        }
    }
    decoder.finish()?;
    if let Some(chunk) = chunker.finish() {
        builder.absorb(chunk).await?;
    }

    let md5_hex = hex::encode(hasher.finalize());

    // Empty object: no chunks, no xorbs, no shard. Register a nameless-content
    // marker; the read path short-circuits on size 0 without reconstruction.
    if total == 0 {
        return Ok((file_hash(&[]).hex(), 0, md5_hex));
    }

    let (file_hash_hex, shard_hash_hex) = builder.finalize().await?;

    // Register file → shard and claim ownership so the file survives GC.
    state
        .file_index
        .put(&file_hash_hex, &shard_hash_hex)
        .await?;
    state
        .file_index
        .claim(
            owner,
            &file_hash_hex,
            OwnershipClaim {
                logical_bytes: total,
                created_at_unix: now_unix(),
            },
        )
        .await?;

    Ok((file_hash_hex, total, md5_hex))
}

// ---- handlers ----------------------------------------------------------------

/// Does the request body arrive in aws-chunked transfer framing? Signed
/// `aws s3` uploads set `x-amz-content-sha256: STREAMING-…`; some tools instead
/// (or additionally) set `Content-Encoding: aws-chunked`.
fn is_aws_chunked(headers: &HeaderMap) -> bool {
    let streaming = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("STREAMING-"))
        .unwrap_or(false);
    let encoded = headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|e| e.trim() == "aws-chunked"))
        .unwrap_or(false);
    streaming || encoded
}

/// PUT /{prefix}/{bucket}/{key} — store an object (or, when `x-amz-copy-source`
/// is present, copy an existing one).
pub async fn put_object(
    State(state): State<AppState>,
    auth: S3Auth,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, S3Error> {
    if let Some(src) = headers.get("x-amz-copy-source") {
        let src = src
            .to_str()
            .map_err(|_| {
                S3Error::new(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "invalid x-amz-copy-source",
                )
            })?
            .to_string();
        return copy_object(&state, &auth, bucket, key, &src).await;
    }

    if bucket.is_empty() || key.is_empty() {
        return Err(S3Error::new(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "bucket and key are required",
        ));
    }

    let aws_chunked = is_aws_chunked(&headers);
    let (file_hash, size, etag) = ingest_object(&state, &auth.owner_id, aws_chunked, body)
        .await
        .map_err(|e| from_app_error(e, &key))?;

    let obj = S3Object {
        bucket,
        key,
        file_hash,
        size,
        etag: etag.clone(),
        owner_id: auth.owner_id.clone(),
        last_modified: now_unix(),
    };
    state
        .s3_index
        .put_object(&obj)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, format!("\"{etag}\""))
        .body(Body::empty())
        .map_err(|e| S3Error::internal(e.to_string()))
}

/// Parse an `x-amz-copy-source` value (`/bucket/key` or `bucket/key`, key
/// percent-encoded, optional `?versionId=…`) into `(bucket, key)`.
fn parse_copy_source(raw: &str) -> Option<(String, String)> {
    let path = raw.split('?').next().unwrap_or(raw);
    let path = path.strip_prefix('/').unwrap_or(path);
    let (bucket, key) = path.split_once('/')?;
    if bucket.is_empty() || key.is_empty() {
        return None;
    }
    let key = percent_decode(key);
    Some((bucket.to_string(), key))
}

/// Minimal percent-decoding for a copy-source key (S3 encodes `/` in keys as
/// `%2F`, spaces as `%20`, etc.). Invalid escapes are passed through literally.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Server-side copy: map a new name onto the source object's existing content
/// (metadata-only; content is shared, so this is free dedup) and claim
/// ownership of that content for the copier.
async fn copy_object(
    state: &AppState,
    auth: &S3Auth,
    dst_bucket: String,
    dst_key: String,
    copy_source: &str,
) -> Result<Response, S3Error> {
    let (src_bucket, src_key) = parse_copy_source(copy_source).ok_or_else(|| {
        S3Error::new(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "invalid x-amz-copy-source",
        )
    })?;

    let src = state
        .s3_index
        .get_object(&src_bucket, &src_key)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?
        .ok_or_else(|| S3Error::no_such_key(&src_key))?;

    let now = now_unix();
    let dst = S3Object {
        bucket: dst_bucket,
        key: dst_key,
        file_hash: src.file_hash.clone(),
        size: src.size,
        etag: src.etag.clone(),
        owner_id: auth.owner_id.clone(),
        last_modified: now,
    };
    state
        .s3_index
        .put_object(&dst)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?;

    // Claim the shared content for the copier (skip empty objects, which have
    // no reconstructable file behind them).
    if src.size > 0 {
        state
            .file_index
            .claim(
                &auth.owner_id,
                &src.file_hash,
                OwnershipClaim {
                    logical_bytes: src.size,
                    created_at_unix: now,
                },
            )
            .await
            .map_err(|e| S3Error::internal(e.to_string()))?;
    }

    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <CopyObjectResult><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag></CopyObjectResult>",
        format_iso8601(now),
        xml_escape(&dst.etag),
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .header(header::ETAG, format!("\"{}\"", dst.etag))
        .body(Body::from(body))
        .map_err(|e| S3Error::internal(e.to_string()))
}

/// DELETE /{prefix}/{bucket}/{key} — remove the object name. Idempotent: S3
/// returns 204 whether or not the key existed. See the module note on why the
/// underlying file's ownership claim is intentionally left in place.
pub async fn delete_object(
    State(state): State<AppState>,
    _auth: S3Auth,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, S3Error> {
    state
        .s3_index
        .delete_object(&bucket, &key)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?;

    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .map_err(|e| S3Error::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(enabled: bool, framed: &[&[u8]]) -> Result<Vec<u8>, AppError> {
        let mut dec = AwsChunkedDecoder::new(enabled);
        let mut out = Vec::new();
        for f in framed {
            let mut decoded = Vec::new();
            dec.feed(f, &mut decoded)?;
            for d in decoded {
                out.extend_from_slice(&d);
            }
        }
        dec.finish()?;
        Ok(out)
    }

    #[test]
    fn disabled_passthrough() {
        let out = decode_all(false, &[b"hello ", b"world"]).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn signed_chunked_single_frame() {
        // "hello world" (0xb bytes) then a zero terminator, with chunk-signatures.
        let body = b"b;chunk-signature=abc123\r\nhello world\r\n0;chunk-signature=def456\r\n\r\n";
        let out = decode_all(true, &[body]).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn unsigned_trailer_chunked() {
        let body = b"5\r\nhello\r\n6\r\n world\r\n0\r\nx-amz-checksum-crc32:abcd\r\n\r\n";
        let out = decode_all(true, &[body]).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn chunked_split_across_frames() {
        // Same body as signed_chunked_single_frame, delivered one byte at a time.
        let body: &[u8] =
            b"b;chunk-signature=abc123\r\nhello world\r\n0;chunk-signature=def456\r\n\r\n";
        let mut dec = AwsChunkedDecoder::new(true);
        let mut out = Vec::new();
        for b in body {
            let mut decoded = Vec::new();
            dec.feed(&[*b], &mut decoded).unwrap();
            for d in decoded {
                out.extend_from_slice(&d);
            }
        }
        dec.finish().unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn incomplete_chunked_body_errors() {
        // No final zero-length chunk.
        let mut dec = AwsChunkedDecoder::new(true);
        let mut decoded = Vec::new();
        dec.feed(b"5\r\nhello\r\n", &mut decoded).unwrap();
        assert!(dec.finish().is_err());
    }

    #[test]
    fn parse_copy_source_variants() {
        assert_eq!(
            parse_copy_source("/bucket/path/to/key"),
            Some(("bucket".into(), "path/to/key".into()))
        );
        assert_eq!(
            parse_copy_source("bucket/key%20name"),
            Some(("bucket".into(), "key name".into()))
        );
        assert_eq!(
            parse_copy_source("bucket/a%2Fb?versionId=1"),
            Some(("bucket".into(), "a/b".into()))
        );
        assert_eq!(parse_copy_source("/bucket"), None);
    }
}
