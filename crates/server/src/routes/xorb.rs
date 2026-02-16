use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::Serialize;

use openxet_cas_types::xorb::{MAX_XORB_SIZE, compute_xorb_hash, deserialize_xorb};
use openxet_hashing::{MerkleHash, compute_chunk_hash};

use crate::auth::{RequireRead, RequireWrite};
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::index::ChunkLocation;
use crate::storage::{ChunkIndex, StorageBackend, validate_hash};

#[derive(Debug, Serialize)]
pub struct XorbUploadResponse {
    pub was_inserted: bool,
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

    // Store the xorb
    state.storage.put_xorb(&hash, body).await?;

    // Index each chunk
    for (i, (chunk_hash, _)) in chunk_hashes_and_sizes.iter().enumerate() {
        state
            .chunk_index
            .put(
                &chunk_hash.to_hex(),
                ChunkLocation {
                    xorb_hash: hash.clone(),
                    chunk_index: i as u32,
                },
            )
            .await?;
    }

    Ok(Json(XorbUploadResponse { was_inserted: true }))
}

/// GET /xorbs/default/{hash} — download xorb data with optional Range header.
///
/// xet-core's download flow fetches xorb data from the URLs provided in
/// the reconstruction response's fetch_info entries.
pub async fn get_xorb(
    State(state): State<AppState>,
    _auth: RequireRead,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
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
