use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;

use openxet_cas_types::chunk::ChunkHeader;
use openxet_cas_types::reconstruction::{
    ByteRange, CASReconstructionFetchInfo, CASReconstructionTerm, ChunkRange,
    QueryReconstructionResponse,
};
use openxet_cas_types::shard::Shard;

use crate::auth::RequireRead;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::{FileIndex, StorageBackend, validate_hash};

/// Parse an HTTP Range header of the form "bytes=start-end" (inclusive end).
pub fn parse_range_header(headers: &HeaderMap) -> Result<Option<(u64, u64)>, AppError> {
    let Some(range_val) = headers.get("range") else {
        return Ok(None);
    };

    let range_str = range_val
        .to_str()
        .map_err(|_| AppError::BadRequest("invalid range header encoding".to_string()))?;

    let range_str = range_str
        .strip_prefix("bytes=")
        .ok_or_else(|| AppError::BadRequest("range header must start with 'bytes='".to_string()))?;

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

    Ok(Some((start, end)))
}

/// Compute byte offsets for each chunk within a serialized xorb by walking chunk headers.
/// Returns a list of (offset_start, offset_end) byte positions in the xorb binary for each chunk.
fn compute_chunk_byte_offsets(xorb_data: &[u8]) -> Vec<(u64, u64)> {
    let mut offsets = Vec::new();
    let mut pos = 0usize;

    while pos + ChunkHeader::SIZE <= xorb_data.len() {
        let header_bytes: [u8; 8] = xorb_data[pos..pos + 8].try_into().unwrap();
        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };

        let chunk_start = pos as u64;
        let chunk_end = (pos + ChunkHeader::SIZE + header.compressed_size as usize) as u64;
        offsets.push((chunk_start, chunk_end));

        pos = chunk_end as usize;
    }

    offsets
}

pub async fn get_reconstruction(
    State(state): State<AppState>,
    _auth: RequireRead,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<QueryReconstructionResponse>, AppError> {
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

    // Find the file info block for this file_id
    let file_block = shard
        .file_info_blocks
        .iter()
        .find(|b| b.header.file_hash.to_hex() == file_id)
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "file {file_id} not found in shard {shard_hash}"
            ))
        })?;

    // Build the initial terms list
    let mut terms: Vec<CASReconstructionTerm> = file_block
        .entries
        .iter()
        .map(|entry| CASReconstructionTerm {
            hash: entry.cas_hash.to_hex(),
            unpacked_length: entry.unpacked_segment_bytes as u64,
            range: ChunkRange {
                start: entry.chunk_index_start as usize,
                end: entry.chunk_index_end as usize,
            },
        })
        .collect();

    let mut offset_into_first_range: u64 = 0;

    // Handle range requests
    if let Some((range_start, range_end)) = parse_range_header(&headers)? {
        // Compute cumulative byte offsets per term
        let total_size: u64 = terms.iter().map(|t| t.unpacked_length).sum();

        if range_start >= total_size {
            return Err(AppError::RangeNotSatisfiable);
        }

        let range_end = range_end.min(total_size - 1); // clamp

        // Find which terms overlap the requested range
        let mut byte_offset: u64 = 0;
        let mut trimmed_terms = Vec::new();

        for term in &terms {
            let term_start = byte_offset;
            let term_end = byte_offset + term.unpacked_length; // exclusive

            if term_start <= range_end && term_end > range_start {
                // This term overlaps the range
                if trimmed_terms.is_empty() {
                    offset_into_first_range = range_start.saturating_sub(term_start);
                }
                trimmed_terms.push(term.clone());
            }

            byte_offset = term_end;
        }

        terms = trimmed_terms;
    }

    // Build fetch_info: for each unique xorb, compute byte offsets.
    // Derive the base URL from the request's Host header so that URLs
    // are reachable by the client (the configured host/port may be 0.0.0.0:0).
    let base_url = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|host| format!("http://{host}"))
        .unwrap_or_else(|| state.config.base_url());
    let mut fetch_info: HashMap<String, Vec<CASReconstructionFetchInfo>> = HashMap::new();

    for term in &terms {
        if fetch_info.contains_key(&term.hash) {
            continue;
        }

        let xorb_data = state.storage.get_xorb(&term.hash).await?;
        let chunk_offsets = compute_chunk_byte_offsets(&xorb_data);

        // Build fetch info covering the chunks this term needs
        let start_idx = term.range.start;
        let end_idx = term.range.end.min(chunk_offsets.len());

        if start_idx < chunk_offsets.len() {
            let byte_start = chunk_offsets[start_idx].0;
            let byte_end = chunk_offsets[end_idx - 1].1 - 1; // inclusive for HTTP Range

            fetch_info.insert(
                term.hash.clone(),
                vec![CASReconstructionFetchInfo {
                    range: term.range,
                    url: format!("{base_url}/v1/xorbs/default/{}", term.hash),
                    url_range: ByteRange {
                        start: byte_start,
                        end: byte_end,
                    },
                }],
            );
        }
    }

    Ok(Json(QueryReconstructionResponse {
        offset_into_first_range,
        terms,
        fetch_info,
    }))
}
