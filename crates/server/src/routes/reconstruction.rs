use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use futures::stream::{self, StreamExt, TryStreamExt};

use openxet_cas_types::chunk::ChunkHeader;
use openxet_cas_types::reconstruction::{
    ByteRange, CASReconstructionFetchInfo, CASReconstructionTerm, ChunkRange,
    QueryReconstructionResponse,
};
use openxet_cas_types::shard::Shard;

use crate::auth::RequireRead;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::index::XorbLayout;
use crate::storage::{ChunkIndex, FileIndex, StorageBackend, validate_hash};

/// Max concurrent per-xorb layout lookups + presigns when building fetch info.
/// Each unique xorb needs an independent index read (and, for legacy entries, a
/// possible object-store fetch) plus a presign; running them serially made a
/// file that spans many xorbs pay those round-trips back to back.
const RECON_FETCH_CONCURRENCY: usize = 16;

/// Parse an HTTP Range header of the form "bytes=start-end" (inclusive end).
fn parse_range_header(headers: &HeaderMap) -> Result<Option<(u64, u64)>, AppError> {
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

/// Compute the same `(offset_start, offset_end)` byte positions as
/// [`compute_chunk_byte_offsets`], but from a recorded [`XorbLayout`] instead of
/// the xorb bytes — no object-store fetch. Each chunk occupies its 8-byte header
/// plus its compressed data, so offsets are the running sum of those spans.
///
/// Returns `None` when the layout predates recorded compressed sizes (any chunk
/// still has the `0` sentinel), so the caller falls back to reading the xorb.
fn chunk_byte_offsets_from_layout(layout: &XorbLayout) -> Option<Vec<(u64, u64)>> {
    if layout.chunks.iter().any(|c| c.compressed_size == 0) {
        return None;
    }

    let mut offsets = Vec::with_capacity(layout.chunks.len());
    let mut pos = 0u64;
    for chunk in &layout.chunks {
        let chunk_start = pos;
        let chunk_end = pos + ChunkHeader::SIZE as u64 + chunk.compressed_size as u64;
        offsets.push((chunk_start, chunk_end));
        pos = chunk_end;
    }

    Some(offsets)
}

pub async fn get_reconstruction(
    State(state): State<AppState>,
    RequireRead(_claims): RequireRead,
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

    // xet-core fetches fetch_info URLs with no Authorization header, so each URL
    // must be self-authenticating. Cloud backends satisfy this with a presigned
    // URL straight to object storage (this server stays out of the data path).
    // The local filesystem can't presign, so it falls back to the server's own
    // public xorb route. Prefer the configured public URL for that fallback;
    // otherwise use the request `Host`, then the bound address.
    let base_url = state
        .config
        .server
        .public_url
        .clone()
        .or_else(|| {
            headers
                .get(axum::http::header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(|host| format!("http://{host}"))
        })
        .unwrap_or_else(|| state.config.base_url());

    let url_ttl = Duration::from_secs(state.config.auth.shard_key_ttl_seconds);

    // De-duplicate to the first term referencing each xorb — its chunk range is
    // what that xorb's fetch info covers — then build every entry concurrently.
    let mut seen = HashSet::new();
    let unique_terms: Vec<CASReconstructionTerm> = terms
        .iter()
        .filter(|t| seen.insert(t.hash.clone()))
        .cloned()
        .collect();

    let fetch_info: HashMap<String, Vec<CASReconstructionFetchInfo>> =
        stream::iter(unique_terms)
            .map(|term| {
                let state = &state;
                let base_url = &base_url;
                async move {
                    // Prefer the recorded layout: byte offsets come from index
                    // metadata with no object-store fetch. Only fall back to
                    // downloading the whole xorb for entries stored before
                    // layouts recorded compressed sizes.
                    let chunk_offsets =
                        match state.chunk_index.get_xorb_layout(&term.hash).await? {
                            Some(layout) => match chunk_byte_offsets_from_layout(&layout) {
                                Some(offsets) => offsets,
                                None => compute_chunk_byte_offsets(
                                    &state.storage.get_xorb(&term.hash).await?,
                                ),
                            },
                            None => compute_chunk_byte_offsets(
                                &state.storage.get_xorb(&term.hash).await?,
                            ),
                        };

                    // Build fetch info covering the chunks this term needs
                    let start_idx = term.range.start;
                    let end_idx = term.range.end.min(chunk_offsets.len());

                    if start_idx >= chunk_offsets.len() {
                        return Ok::<_, AppError>(None);
                    }

                    let byte_start = chunk_offsets[start_idx].0;
                    let byte_end = chunk_offsets[end_idx - 1].1 - 1; // inclusive for HTTP Range

                    let url = match state
                        .storage
                        .presigned_xorb_url(&term.hash, url_ttl)
                        .await?
                    {
                        Some(presigned) => presigned,
                        None => format!("{base_url}/v1/xorbs/default/{}", term.hash),
                    };

                    Ok(Some((
                        term.hash.clone(),
                        vec![CASReconstructionFetchInfo {
                            range: term.range,
                            url,
                            url_range: ByteRange {
                                start: byte_start,
                                end: byte_end,
                            },
                        }],
                    )))
                }
            })
            .buffer_unordered(RECON_FETCH_CONCURRENCY)
            .try_filter_map(|entry| async move { Ok(entry) })
            .try_collect()
            .await?;

    Ok(Json(QueryReconstructionResponse {
        offset_into_first_range,
        terms,
        fetch_info,
    }))
}
