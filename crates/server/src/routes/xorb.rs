use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::Serialize;

use xet_core_structures::merklehash::MerkleHash;
use xet_core_structures::xorb_object::constants::MAX_XORB_BYTES;

use crate::auth::RequireWrite;
use crate::error::AppError;
use crate::routes::xorb_meta::{layout_from_info, validated_xorb_info};
use crate::state::AppState;
use crate::storage::index::ChunkLocation;
use crate::storage::{ChunkIndex, StorageBackend, validate_hash};

/// Serialized uploads may exceed the raw 64 MiB xorb content cap slightly:
/// per-chunk headers and the trailing metadata footer add overhead on top of
/// the (compressed) chunk payloads.
pub(crate) fn max_serialized_xorb_size() -> usize {
    *MAX_XORB_BYTES + 4 * 1024 * 1024
}

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

    if body.len() > max_serialized_xorb_size() {
        return Err(AppError::PayloadTooLarge);
    }

    // Idempotent: if xorb already exists, return early
    if state.storage.xorb_exists(&hash).await? {
        return Ok(Json(XorbUploadResponse {
            was_inserted: false,
        }));
    }

    let expected = MerkleHash::from_hex(&hash)
        .map_err(|e| AppError::BadRequest(format!("invalid xorb hash: {e}")))?;

    // Recomputes every chunk hash from the (decompressed) data and checks the
    // aggregate against the URL hash — the core CAS invariant.
    let info = validated_xorb_info(&body, &expected)?;

    if info.num_chunks == 0 {
        return Err(AppError::BadRequest("xorb contains no chunks".to_string()));
    }

    // Record the layout before `body` moves into storage, so dedup and
    // reconstruction responses can be built from the index instead of
    // re-reading and re-parsing the xorb.
    let layout = layout_from_info(&info, body.len() as u32);

    // Store the xorb
    state.storage.put_xorb(&hash, body).await?;

    // Index all chunks in one batched write.
    let entries: Vec<(String, ChunkLocation)> = info
        .chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, chunk_hash)| {
            (
                chunk_hash.hex(),
                ChunkLocation {
                    xorb_hash: hash.clone(),
                    chunk_index: i as u32,
                },
            )
        })
        .collect();
    state.chunk_index.put_batch(entries).await?;

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
