use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::Serialize;

use openxet_cas_types::chunk::ChunkHeader;
use openxet_cas_types::xorb::{MAX_XORB_SIZE, compute_xorb_hash, deserialize_xorb};
use openxet_hashing::{MerkleHash, compute_chunk_hash};

use crate::auth::RequireWrite;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::index::{ChunkLocation, XorbChunk, XorbLayout};
use crate::storage::{ChunkIndex, StorageBackend, validate_hash};

#[derive(Debug, Serialize)]
pub struct XorbUploadResponse {
    pub was_inserted: bool,
}

/// Compressed data size (excluding the 8-byte header) of each chunk, in xorb
/// order. Parses only chunk headers — no decompression. `body` is already
/// validated by [`deserialize_xorb`] before this is called, so the walk mirrors
/// that framing and never runs past the end.
fn chunk_compressed_sizes(xorb_data: &[u8]) -> Vec<u32> {
    let mut sizes = Vec::new();
    let mut pos = 0usize;

    while pos + ChunkHeader::SIZE <= xorb_data.len() {
        let header_bytes: [u8; 8] = xorb_data[pos..pos + 8].try_into().unwrap();
        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };
        sizes.push(header.compressed_size);
        pos += ChunkHeader::SIZE + header.compressed_size as usize;
    }

    sizes
}

pub async fn post_xorb(
    State(state): State<AppState>,
    _auth: RequireWrite,
    Path(hash): Path<String>,
    body: Bytes,
) -> Result<Json<XorbUploadResponse>, AppError> {
    validate_hash(&hash)?;

    if body.len() > MAX_XORB_SIZE {
        return Err(AppError::PayloadTooLarge);
    }

    // Idempotent: if xorb already exists, return early
    if state.storage.xorb_exists(&hash).await? {
        return Ok(Json(XorbUploadResponse {
            was_inserted: false,
        }));
    }

    // Parse and validate xorb
    let chunks = deserialize_xorb(&body)?;

    // A xorb must contain at least one chunk; an empty chunk list would panic
    // the merkle-root computation below.
    if chunks.is_empty() {
        return Err(AppError::BadRequest("xorb contains no chunks".to_string()));
    }

    // Compute chunk hashes and verify xorb hash
    let chunk_hashes_and_sizes: Vec<(MerkleHash, usize)> = chunks
        .iter()
        .map(|c| (compute_chunk_hash(&c.data), c.data.len()))
        .collect();

    let computed_hash = compute_xorb_hash(&chunk_hashes_and_sizes);
    if computed_hash.to_hex() != hash {
        return Err(AppError::BadRequest(format!(
            "xorb hash mismatch: URL={hash}, computed={}",
            computed_hash.to_hex()
        )));
    }

    // On-disk size and per-chunk compressed sizes must be read before `body` is
    // moved into storage. Compressed sizes come from a header-only walk (no
    // decompression) and feed the layout so reconstruction can compute byte
    // ranges without re-downloading the xorb.
    let num_bytes_on_disk = body.len() as u32;
    let compressed_sizes = chunk_compressed_sizes(&body);

    // Store the xorb
    state.storage.put_xorb(&hash, body).await?;

    // Index all chunks in one batched write.
    let entries: Vec<(String, ChunkLocation)> = chunk_hashes_and_sizes
        .iter()
        .enumerate()
        .map(|(i, (chunk_hash, _))| {
            (
                chunk_hash.to_hex(),
                ChunkLocation {
                    xorb_hash: hash.clone(),
                    chunk_index: i as u32,
                },
            )
        })
        .collect();
    state.chunk_index.put_batch(entries).await?;

    // Record the xorb layout so dedup responses can be built from the index
    // instead of re-downloading and decompressing the xorb from storage.
    let layout = XorbLayout {
        num_bytes_on_disk,
        chunks: chunk_hashes_and_sizes
            .iter()
            .zip(&compressed_sizes)
            .map(|((chunk_hash, size), compressed_size)| XorbChunk {
                chunk_hash: chunk_hash.to_hex(),
                unpacked_size: *size as u32,
                compressed_size: *compressed_size,
            })
            .collect(),
    };
    state.chunk_index.put_xorb_layout(&hash, layout).await?;

    Ok(Json(XorbUploadResponse { was_inserted: true }))
}

/// GET /xorbs/default/{hash} — download xorb data with optional Range header.
///
/// xet-core's download flow fetches xorb data from the URLs in the
/// reconstruction response's fetch_info entries **without** an Authorization
/// header (it treats them like presigned URLs). This route is therefore the
/// unauthenticated fallback used only when the storage backend can't presign
/// (local filesystem); cloud backends hand out presigned URLs that bypass this
/// route entirely, so on those backends this route serves nothing and 404s.
pub async fn get_xorb(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    // On presign-capable backends clients fetch directly from object storage;
    // this route is only the filesystem fallback, so refuse to serve otherwise.
    if state.storage.supports_presigned_urls() {
        return Err(AppError::NotFound(format!(
            "xorb download not served by this backend: {hash}"
        )));
    }

    validate_hash(&hash)?;

    if let Some(range_val) = headers.get("range") {
        let range_str = range_val
            .to_str()
            .map_err(|_| AppError::BadRequest("invalid range header".to_string()))?;

        let range_str = range_str
            .strip_prefix("bytes=")
            .ok_or_else(|| AppError::BadRequest("range must start with 'bytes='".to_string()))?;

        let (start_str, end_str) = range_str
            .split_once('-')
            .ok_or_else(|| AppError::BadRequest("invalid range format".to_string()))?;

        let start: u64 = start_str
            .parse()
            .map_err(|_| AppError::BadRequest("invalid range start".to_string()))?;
        let end: u64 = end_str
            .parse()
            .map_err(|_| AppError::BadRequest("invalid range end".to_string()))?;

        if start > end {
            return Err(AppError::RangeNotSatisfiable);
        }

        // get_xorb_range uses exclusive end
        let data = state.storage.get_xorb_range(&hash, start, end + 1).await?;

        Ok((
            axum::http::StatusCode::PARTIAL_CONTENT,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        ))
    } else {
        let data = state.storage.get_xorb(&hash).await?;
        Ok((
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        ))
    }
}
