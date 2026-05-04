use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;

use openxet_cas_types::shard::Shard;
use openxet_cas_types::xorb::deserialize_xorb_range;

use crate::auth::RequireRead;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::{FileIndex, StorageBackend, validate_hash};

use super::reconstruction::parse_range_header;

/// GET /v1/content/{file_id} — stream decompressed file bytes with optional Range support.
pub async fn get_content(
    State(state): State<AppState>,
    _auth: RequireRead,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    validate_hash(&file_id)?;

    // Look up file → shard
    let shard_hash = state
        .file_index
        .get(&file_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("file not found: {file_id}")))?;

    // Read and parse the shard
    let shard_data = state.storage.get_shard(&shard_hash).await?;
    let shard = Shard::from_bytes(&shard_data)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt shard {shard_hash}: {e}")))?;

    // Find the file info block
    let file_block = shard
        .file_info_blocks
        .iter()
        .find(|b| b.header.file_hash.to_hex() == file_id)
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "file {file_id} not found in shard {shard_hash}"
            ))
        })?;

    // Reassemble the full file by reading and decompressing each xorb's chunks
    let mut file_bytes: Vec<u8> = Vec::new();
    for entry in &file_block.entries {
        let xorb_hash = entry.cas_hash.to_hex();
        let xorb_data = state.storage.get_xorb(&xorb_hash).await?;
        let chunks = deserialize_xorb_range(
            &xorb_data,
            entry.chunk_index_start as usize,
            entry.chunk_index_end as usize,
        )
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!(
                "failed to decompress xorb {xorb_hash}: {e}"
            ))
        })?;

        for chunk in &chunks {
            file_bytes.extend_from_slice(&chunk.data);
        }
    }

    let total_size = file_bytes.len() as u64;

    // Handle Range header
    if let Some((range_start, range_end)) = parse_range_header(&headers)? {
        if range_start >= total_size {
            return Err(AppError::RangeNotSatisfiable);
        }

        let range_end = range_end.min(total_size - 1);
        let slice = file_bytes[range_start as usize..=range_end as usize].to_vec();
        let content_range = format!("bytes {range_start}-{range_end}/{total_size}");

        Ok((
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_LENGTH, slice.len().to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_RANGE, content_range),
            ],
            slice,
        ))
    } else {
        Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (header::CONTENT_LENGTH, total_size.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_RANGE, String::new()),
            ],
            file_bytes,
        ))
    }
}
