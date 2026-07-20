use std::collections::HashMap;
use std::io::Cursor;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use futures::stream::{self, StreamExt, TryStreamExt};

use openxet_cas_types::reconstruction::{
    ByteRange, CASReconstructionFetchInfo, CASReconstructionTerm, ChunkRange,
    QueryReconstructionResponse, QueryReconstructionResponseV2, XorbMultiRangeFetch,
    XorbRangeDescriptor,
};
use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;
use xet_core_structures::xorb_object::XORB_CHUNK_HEADER_LENGTH;

use crate::auth::RequireRead;
use crate::error::AppError;
use crate::routes::xorb_meta::{chunk_byte_offsets, xorb_info_from_stored};
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

/// Compute the same `(offset_start, offset_end)` physical byte positions as
/// [`chunk_byte_offsets`], but from a recorded [`XorbLayout`] instead of the
/// xorb bytes — no object-store fetch. Each chunk occupies its 8-byte header
/// plus its compressed data, so offsets are the running sum of those spans.
///
/// Returns `None` when the layout predates recorded compressed sizes (any chunk
/// still has the `0` sentinel), so the caller falls back to reading the xorb.
pub(crate) fn chunk_byte_offsets_from_layout(layout: &XorbLayout) -> Option<Vec<(u64, u64)>> {
    if layout.chunks.iter().any(|c| c.compressed_size == 0) {
        return None;
    }

    let mut offsets = Vec::with_capacity(layout.chunks.len());
    let mut pos = 0u64;
    for chunk in &layout.chunks {
        let chunk_start = pos;
        let chunk_end = pos + XORB_CHUNK_HEADER_LENGTH as u64 + chunk.compressed_size as u64;
        offsets.push((chunk_start, chunk_end));
        pos = chunk_end;
    }

    Some(offsets)
}

/// Resolve `file_id` to its full, untrimmed reconstruction terms in file order,
/// by looking up its shard and parsing the file's entry list. Shared by the
/// reconstruction endpoints (which then trim to a Range) and the S3 gateway
/// (which reassembles the bytes server-side).
pub(crate) async fn file_terms(
    state: &AppState,
    file_id: &str,
) -> Result<Vec<CASReconstructionTerm>, AppError> {
    validate_hash(file_id)?;

    // Look up file → shard
    let shard_hash = state
        .file_index
        .get(file_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("file not found: {file_id}")))?;

    // Read and parse the shard (stored shards are upload-format: no footer)
    let shard_data = state.storage.get_shard(&shard_hash).await?;
    let shard = MDBMinimalShard::from_reader(&mut Cursor::new(&shard_data[..]), true, false)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt shard {shard_hash}: {e}")))?;

    // Find the file info block for this file_id
    let file_view = (0..shard.num_files())
        .filter_map(|i| shard.file(i))
        .find(|fv| fv.file_hash().hex() == file_id)
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "file {file_id} not found in shard {shard_hash}"
            ))
        })?;

    // Build the terms list
    Ok((0..file_view.num_entries())
        .map(|i| {
            let entry = file_view.entry(i);
            CASReconstructionTerm {
                hash: entry.xorb_hash.hex(),
                unpacked_length: entry.unpacked_segment_bytes as u64,
                range: ChunkRange {
                    start: entry.chunk_index_start as usize,
                    end: entry.chunk_index_end as usize,
                },
            }
        })
        .collect())
}

/// Resolve `file_id` to its reconstruction terms, trimmed to the request's
/// Range header when present. Returns the byte offset into the first term
/// along with the (possibly trimmed) terms.
async fn reconstruction_terms(
    state: &AppState,
    file_id: &str,
    headers: &HeaderMap,
) -> Result<(u64, Vec<CASReconstructionTerm>), AppError> {
    let mut terms = file_terms(state, file_id).await?;

    let mut offset_into_first_range: u64 = 0;

    // Handle range requests
    if let Some((range_start, range_end)) = parse_range_header(headers)? {
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

    Ok((offset_into_first_range, terms))
}

/// Merge every term's chunk range per xorb into a minimal set of
/// non-overlapping, non-adjacent ranges. A file that references the same xorb
/// from multiple terms (deduplicated content) must get fetch coverage for each
/// term's range, not just the first one seen.
fn merged_chunk_ranges_per_xorb(terms: &[CASReconstructionTerm]) -> Vec<(String, Vec<ChunkRange>)> {
    let mut order: Vec<String> = Vec::new();
    let mut per_xorb: HashMap<String, Vec<ChunkRange>> = HashMap::new();

    for term in terms {
        per_xorb
            .entry(term.hash.clone())
            .or_insert_with(|| {
                order.push(term.hash.clone());
                Vec::new()
            })
            .push(term.range);
    }

    order
        .into_iter()
        .map(|hash| {
            let mut ranges = per_xorb.remove(&hash).unwrap_or_default();
            ranges.sort_by_key(|r| (r.start, r.end));

            let mut merged: Vec<ChunkRange> = Vec::new();
            for range in ranges {
                match merged.last_mut() {
                    // Overlapping or adjacent ranges collapse into one span.
                    Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                    _ => merged.push(range),
                }
            }

            (hash, merged)
        })
        .collect()
}

/// Resolved fetch information for one xorb: a self-authenticating URL plus the
/// physical byte span of each merged chunk range.
struct XorbFetchEntry {
    hash: String,
    url: String,
    ranges: Vec<(ChunkRange, ByteRange)>,
}

/// Build one [`XorbFetchEntry`] per unique xorb referenced by `terms`,
/// resolving chunk byte offsets and presigning concurrently.
async fn build_xorb_fetch_entries(
    state: &AppState,
    terms: &[CASReconstructionTerm],
    headers: &HeaderMap,
) -> Result<Vec<XorbFetchEntry>, AppError> {
    // xet-core fetches fetch URLs with no Authorization header, so each URL
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

    stream::iter(merged_chunk_ranges_per_xorb(terms))
        .map(|(hash, chunk_ranges)| {
            let state = &state;
            let base_url = &base_url;
            async move {
                // Prefer the recorded layout: byte offsets come from index
                // metadata with no object-store fetch. Only fall back to
                // downloading the whole xorb for entries stored before
                // layouts recorded compressed sizes.
                let chunk_offsets = match state.chunk_index.get_xorb_layout(&hash).await? {
                    Some(layout) => match chunk_byte_offsets_from_layout(&layout) {
                        Some(offsets) => offsets,
                        None => {
                            let xorb_data = state.storage.get_xorb(&hash).await?;
                            chunk_byte_offsets(&xorb_info_from_stored(&xorb_data)?)
                        }
                    },
                    None => {
                        let xorb_data = state.storage.get_xorb(&hash).await?;
                        chunk_byte_offsets(&xorb_info_from_stored(&xorb_data)?)
                    }
                };

                let ranges: Vec<(ChunkRange, ByteRange)> = chunk_ranges
                    .into_iter()
                    .filter_map(|range| {
                        let start_idx = range.start;
                        let end_idx = range.end.min(chunk_offsets.len());

                        if start_idx >= chunk_offsets.len() {
                            return None;
                        }

                        let byte_start = chunk_offsets[start_idx].0;
                        let byte_end = chunk_offsets[end_idx - 1].1 - 1; // inclusive for HTTP Range

                        Some((
                            range,
                            ByteRange {
                                start: byte_start,
                                end: byte_end,
                            },
                        ))
                    })
                    .collect();

                if ranges.is_empty() {
                    return Ok::<_, AppError>(None);
                }

                let url = match state.storage.presigned_xorb_url(&hash, url_ttl).await? {
                    Some(presigned) => presigned,
                    None => format!("{base_url}/v1/xorbs/default/{hash}"),
                };

                Ok(Some(XorbFetchEntry { hash, url, ranges }))
            }
        })
        .buffer_unordered(RECON_FETCH_CONCURRENCY)
        .try_filter_map(|entry| async move { Ok(entry) })
        .try_collect()
        .await
}

pub async fn get_reconstruction(
    State(state): State<AppState>,
    RequireRead(_claims): RequireRead,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<QueryReconstructionResponse>, AppError> {
    let (offset_into_first_range, terms) = reconstruction_terms(&state, &file_id, &headers).await?;
    let entries = build_xorb_fetch_entries(&state, &terms, &headers).await?;

    let fetch_info: HashMap<String, Vec<CASReconstructionFetchInfo>> = entries
        .into_iter()
        .map(|XorbFetchEntry { hash, url, ranges }| {
            let infos = ranges
                .into_iter()
                .map(|(chunks, bytes)| CASReconstructionFetchInfo {
                    range: chunks,
                    url: url.clone(),
                    url_range: bytes,
                })
                .collect();
            (hash, infos)
        })
        .collect();

    Ok(Json(QueryReconstructionResponse {
        offset_into_first_range,
        terms,
        fetch_info,
    }))
}

pub async fn get_reconstruction_v2(
    State(state): State<AppState>,
    RequireRead(_claims): RequireRead,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<QueryReconstructionResponseV2>, AppError> {
    let (offset_into_first_range, terms) = reconstruction_terms(&state, &file_id, &headers).await?;
    let entries = build_xorb_fetch_entries(&state, &terms, &headers).await?;

    // One single-range fetch entry per merged chunk range: xet-core sends all
    // of an entry's ranges in one Range header, and a multi-range header needs
    // a multipart/byteranges response — which S3-style presigned URLs and the
    // /v1/xorbs fallback route can't produce. Single-range entries keep every
    // fetch on the ordinary 206 path.
    let xorbs: HashMap<String, Vec<XorbMultiRangeFetch>> = entries
        .into_iter()
        .map(|XorbFetchEntry { hash, url, ranges }| {
            let fetches = ranges
                .into_iter()
                .map(|(chunks, bytes)| XorbMultiRangeFetch {
                    url: url.clone(),
                    ranges: vec![XorbRangeDescriptor { chunks, bytes }],
                })
                .collect();
            (hash, fetches)
        })
        .collect();

    Ok(Json(QueryReconstructionResponseV2 {
        offset_into_first_range,
        terms,
        xorbs,
    }))
}
