//! GetObject / HeadObject: resolve `(bucket, key)` to a file and stream its
//! reassembled bytes.
//!
//! This is where the interop shim pays its cost: the store is content-addressed
//! and chunk-compressed, so serving raw object bytes means fetching the needed
//! xorb frames, decompressing them, and streaming the result *through* the
//! server (decompression is server CPU — the accepted trade for S3
//! compatibility). Reuses the reconstruction internals (`file_terms`,
//! `chunk_byte_offsets_from_layout`) so there is one source of truth for how a
//! file maps onto xorb chunk ranges.

use std::io::Cursor;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use openxet_cas_types::reconstruction::CASReconstructionTerm;
use xet_core_structures::xorb_object::deserialize_chunks;

use crate::error::AppError;
use crate::routes::reconstruction::{chunk_byte_offsets_from_layout, file_terms};
use crate::routes::xorb_meta::{chunk_byte_offsets, xorb_info_from_stored};
use crate::state::AppState;
use crate::storage::{ChunkIndex, S3Object, StorageBackend};

use super::error::{S3Error, from_app_error};
use super::format_http_date;
use super::sigv4::S3Auth;

/// Max concurrent per-term fetch+decompress operations while streaming an
/// object. `buffered` preserves order, so output is still sequential; this only
/// overlaps the network/CPU of adjacent terms. Each in-flight term holds its
/// decompressed bytes in memory, so keep this modest.
const REASSEMBLE_CONCURRENCY: usize = 4;

/// One overlapping term plus the byte slice of it to emit: `[skip, end)` within
/// the term's decompressed bytes.
struct Segment {
    term: CASReconstructionTerm,
    skip: usize,
    end: usize,
}

/// Parse an S3 `Range` header into an inclusive `(start, end)` within a file of
/// `total` bytes. Supports `bytes=a-b`, `bytes=a-`, and `bytes=-suffix`.
/// Returns `Ok(None)` when there is no Range header.
fn parse_s3_range(headers: &HeaderMap, total: u64) -> Result<Option<(u64, u64)>, S3Error> {
    let Some(raw) = headers.get(header::RANGE) else {
        return Ok(None);
    };
    let raw = raw.to_str().map_err(|_| S3Error::invalid_range())?;
    let spec = raw
        .strip_prefix("bytes=")
        .ok_or_else(S3Error::invalid_range)?;
    // Only single ranges are supported; a comma means multi-range.
    if spec.contains(',') {
        return Err(S3Error::invalid_range());
    }
    let (start_s, end_s) = spec.split_once('-').ok_or_else(S3Error::invalid_range)?;

    let (start, end) = if start_s.is_empty() {
        // Suffix range: last N bytes.
        let suffix: u64 = end_s.parse().map_err(|_| S3Error::invalid_range())?;
        if suffix == 0 || total == 0 {
            return Err(S3Error::invalid_range());
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let start: u64 = start_s.parse().map_err(|_| S3Error::invalid_range())?;
        let end = if end_s.is_empty() {
            total.saturating_sub(1)
        } else {
            end_s.parse::<u64>().map_err(|_| S3Error::invalid_range())?
        };
        (start, end.min(total.saturating_sub(1)))
    };

    if total == 0 || start > end || start >= total {
        return Err(S3Error::invalid_range());
    }
    Ok(Some((start, end)))
}

/// Physical `[byte_start, byte_end)` span of a term's chunk range within its
/// serialized xorb. Prefers the recorded layout (no fetch); falls back to
/// parsing the whole xorb for pre-layout entries, exactly as reconstruction does.
async fn term_physical_span(
    state: &AppState,
    term: &CASReconstructionTerm,
) -> Result<(u64, u64), AppError> {
    let offsets = match state.chunk_index.get_xorb_layout(&term.hash).await? {
        Some(layout) => match chunk_byte_offsets_from_layout(&layout) {
            Some(offsets) => offsets,
            None => {
                let data = state.storage.get_xorb(&term.hash).await?;
                chunk_byte_offsets(&xorb_info_from_stored(&data)?)
            }
        },
        None => {
            let data = state.storage.get_xorb(&term.hash).await?;
            chunk_byte_offsets(&xorb_info_from_stored(&data)?)
        }
    };

    let start = term.range.start;
    let end = term.range.end;
    if start >= end || end > offsets.len() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "term chunk range {start}..{end} out of bounds for xorb {} ({} chunks)",
            term.hash,
            offsets.len()
        )));
    }
    Ok((offsets[start].0, offsets[end - 1].1))
}

/// Fetch and decompress a term's chunk range into its raw (unpacked) bytes.
async fn term_bytes(state: &AppState, term: &CASReconstructionTerm) -> Result<Bytes, AppError> {
    let (byte_start, byte_end) = term_physical_span(state, term).await?;
    let frames = state
        .storage
        .get_xorb_range(&term.hash, byte_start, byte_end)
        .await?;
    let (bytes, _boundaries) = deserialize_chunks(&mut Cursor::new(&frames[..]))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decode xorb {}: {e}", term.hash)))?;
    Ok(Bytes::from(bytes))
}

/// Select the terms overlapping `[req_start, req_end]` (inclusive) and, for
/// each, the byte slice to emit.
fn plan_segments(terms: &[CASReconstructionTerm], req_start: u64, req_end: u64) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut cumulative = 0u64;
    for term in terms {
        let term_start = cumulative;
        let term_end = cumulative + term.unpacked_length; // exclusive
        cumulative = term_end;

        if term_end <= req_start || term_start > req_end {
            continue;
        }
        let skip = req_start.saturating_sub(term_start);
        let end = ((req_end + 1).min(term_end) - term_start) as usize;
        segments.push(Segment {
            term: term.clone(),
            skip: skip as usize,
            end,
        });
    }
    segments
}

/// Common metadata headers for GetObject/HeadObject responses.
fn object_headers(obj: &S3Object) -> Vec<(header::HeaderName, HeaderValue)> {
    let mut headers = vec![
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        ),
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
    ];
    if let Ok(v) = HeaderValue::from_str(&format!("\"{}\"", obj.etag)) {
        headers.push((header::ETAG, v));
    }
    if let Ok(v) = HeaderValue::from_str(&format_http_date(obj.last_modified)) {
        headers.push((header::LAST_MODIFIED, v));
    }
    headers
}

/// GET /{prefix}/{bucket}/{key} — stream the object's (optionally ranged) bytes.
pub async fn get_object(
    State(state): State<AppState>,
    _auth: S3Auth,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    let obj = state
        .s3_index
        .get_object(&bucket, &key)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?
        .ok_or_else(|| S3Error::no_such_key(&key))?;

    // Empty objects have no reconstructable file behind them (PutObject skips
    // xorb/shard creation for zero bytes), so short-circuit before file_terms.
    if obj.size == 0 {
        let mut builder = Response::builder().status(StatusCode::OK);
        for (name, value) in object_headers(&obj) {
            builder = builder.header(name, value);
        }
        return builder
            .header(header::CONTENT_LENGTH, 0)
            .body(Body::empty())
            .map_err(|e| S3Error::internal(e.to_string()));
    }

    let terms = file_terms(&state, &obj.file_hash)
        .await
        .map_err(|e| from_app_error(e, &key))?;

    let range = parse_s3_range(&headers, obj.size)?;
    let (req_start, req_end, status) = match range {
        Some((s, e)) => (s, e, StatusCode::PARTIAL_CONTENT),
        None => (0, obj.size.saturating_sub(1), StatusCode::OK),
    };
    let content_length = if obj.size == 0 {
        0
    } else {
        req_end - req_start + 1
    };

    let segments = plan_segments(&terms, req_start, req_end);

    // Stream term-by-term (bounded concurrency, order preserved) so we never
    // hold the whole object in memory.
    let state_stream = state.clone();
    let body_stream = stream::iter(segments.into_iter().map(move |seg| {
        let state = state_stream.clone();
        async move {
            let bytes = term_bytes(&state, &seg.term).await?;
            // Guard against a term shorter than planned (corrupt data).
            let end = seg.end.min(bytes.len());
            let skip = seg.skip.min(end);
            Ok::<Bytes, AppError>(bytes.slice(skip..end))
        }
    }))
    .buffered(REASSEMBLE_CONCURRENCY)
    .map(|r| r.map_err(std::io::Error::other));

    let mut builder = Response::builder().status(status);
    for (name, value) in object_headers(&obj) {
        builder = builder.header(name, value);
    }
    builder = builder.header(header::CONTENT_LENGTH, content_length);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {req_start}-{req_end}/{}", obj.size),
        );
    }
    builder
        .body(Body::from_stream(body_stream))
        .map_err(|e| S3Error::internal(e.to_string()))
}

/// HEAD /{prefix}/{bucket}/{key} — object metadata, no body.
pub async fn head_object(
    State(state): State<AppState>,
    _auth: S3Auth,
    Path((bucket, key)): Path<(String, String)>,
) -> Result<Response, S3Error> {
    let obj = state
        .s3_index
        .get_object(&bucket, &key)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?
        .ok_or_else(|| S3Error::no_such_key(&key))?;

    let mut builder = Response::builder().status(StatusCode::OK);
    for (name, value) in object_headers(&obj) {
        builder = builder.header(name, value);
    }
    builder = builder.header(header::CONTENT_LENGTH, obj.size);
    builder
        .body(Body::empty())
        .map_err(|e| S3Error::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(len: u64, start: usize, end: usize) -> CASReconstructionTerm {
        CASReconstructionTerm {
            hash: "00".repeat(32),
            unpacked_length: len,
            range: openxet_cas_types::reconstruction::ChunkRange { start, end },
        }
    }

    #[test]
    fn plan_segments_full_file() {
        let terms = vec![term(10, 0, 1), term(10, 0, 1)];
        let segs = plan_segments(&terms, 0, 19);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].skip, segs[0].end), (0, 10));
        assert_eq!((segs[1].skip, segs[1].end), (0, 10));
    }

    #[test]
    fn plan_segments_mid_range() {
        // bytes 5..=14 span the tail of term 0 and the head of term 1.
        let terms = vec![term(10, 0, 1), term(10, 0, 1)];
        let segs = plan_segments(&terms, 5, 14);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].skip, segs[0].end), (5, 10));
        assert_eq!((segs[1].skip, segs[1].end), (0, 5));
    }

    #[test]
    fn range_suffix_and_open() {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_static("bytes=-5"));
        assert_eq!(parse_s3_range(&h, 100).unwrap(), Some((95, 99)));

        h.insert(header::RANGE, HeaderValue::from_static("bytes=10-"));
        assert_eq!(parse_s3_range(&h, 100).unwrap(), Some((10, 99)));

        h.insert(header::RANGE, HeaderValue::from_static("bytes=10-19"));
        assert_eq!(parse_s3_range(&h, 100).unwrap(), Some((10, 19)));
    }

    #[test]
    fn range_unsatisfiable() {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_static("bytes=200-300"));
        assert!(parse_s3_range(&h, 100).is_err());
    }
}
