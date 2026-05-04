use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use bytes::Bytes;
use serde::Serialize;

use openxet_cas_types::chunk::{ChunkHeader, CompressionType};
use openxet_cas_types::reconstruction::{
    ByteRange, CASReconstructionFetchInfo, CASReconstructionTerm, ChunkRange,
    QueryReconstructionResponse,
};
use openxet_cas_types::shard::{
    CASChunkSequenceEntry, CASChunkSequenceHeader, CASInfoBlock, FileDataSequenceEntry,
    FileDataSequenceHeader, FileInfoBlock, FileVerificationEntry, MDB_FILE_FLAG_WITH_VERIFICATION,
    Shard, ShardHeader,
};
use openxet_cas_types::xorb::{XORB_SOFT_LIMIT, compute_xorb_hash, serialize_single_chunk};
use openxet_chunking::chunk_data;
use openxet_hashing::{
    MerkleHash, compute_chunk_hash, compute_file_hash, compute_verification_hash,
};

use crate::error::AppError;
use crate::state::AppState;
use crate::storage::{ChunkIndex, FileIndex, StorageBackend};

pub fn management_router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(get_stats))
        .route("/files", get(get_files))
        .route("/files/{hash}", get(get_file_detail))
        .route("/files/{hash}/content", get(get_file_content))
        .route("/xorbs", get(get_xorbs))
        .route("/upload", axum::routing::post(post_upload))
}

// ─── GET /api/stats ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct StatsResponse {
    pub files_count: usize,
    pub xorbs_count: usize,
    pub shards_count: usize,
    pub total_size_bytes: u64,
}

async fn get_stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, AppError> {
    let files = state.file_index.list_all().await?;
    let xorbs = state.storage.list_xorbs().await?;
    let shards = state.storage.list_shards().await?;

    let total_size: u64 =
        xorbs.iter().map(|(_, s)| s).sum::<u64>() + shards.iter().map(|(_, s)| s).sum::<u64>();

    Ok(Json(StatsResponse {
        files_count: files.len(),
        xorbs_count: xorbs.len(),
        shards_count: shards.len(),
        total_size_bytes: total_size,
    }))
}

// ─── GET /api/files ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FileEntry {
    pub hash: String,
    pub shard_hash: String,
}

async fn get_files(State(state): State<AppState>) -> Result<Json<Vec<FileEntry>>, AppError> {
    let files = state.file_index.list_all().await?;

    let entries: Vec<FileEntry> = files
        .into_iter()
        .map(|(hash, shard_hash)| FileEntry { hash, shard_hash })
        .collect();

    Ok(Json(entries))
}

// ─── GET /api/files/:hash ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct FileDetailResponse {
    pub hash: String,
    pub total_size: u64,
    pub reconstruction: QueryReconstructionResponse,
}

async fn get_file_detail(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<FileDetailResponse>, AppError> {
    use crate::storage::validate_hash;
    use std::collections::HashMap;

    validate_hash(&hash)?;

    let shard_hash = state
        .file_index
        .get(&hash)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("file not found: {hash}")))?;

    let shard_data = state.storage.get_shard(&shard_hash).await?;
    let shard = Shard::from_bytes(&shard_data)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt shard {shard_hash}: {e}")))?;

    let file_block = shard
        .file_info_blocks
        .iter()
        .find(|b| b.header.file_hash.to_hex() == hash)
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "file {hash} not found in shard {shard_hash}"
            ))
        })?;

    let terms: Vec<CASReconstructionTerm> = file_block
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

    let total_size: u64 = terms.iter().map(|t| t.unpacked_length).sum();

    let base_url = state.config.base_url();
    let mut fetch_info: HashMap<String, Vec<CASReconstructionFetchInfo>> = HashMap::new();

    for term in &terms {
        if fetch_info.contains_key(&term.hash) {
            continue;
        }

        let xorb_data = state.storage.get_xorb(&term.hash).await?;
        let chunk_offsets = compute_chunk_byte_offsets(&xorb_data);

        let start_idx = term.range.start;
        let end_idx = term.range.end.min(chunk_offsets.len());

        if start_idx < chunk_offsets.len() {
            let byte_start = chunk_offsets[start_idx].0;
            let byte_end = chunk_offsets[end_idx - 1].1 - 1;

            fetch_info.insert(
                term.hash.clone(),
                vec![CASReconstructionFetchInfo {
                    range: term.range,
                    url: format!("{base_url}/xorbs/default/{}", term.hash),
                    url_range: ByteRange {
                        start: byte_start,
                        end: byte_end,
                    },
                }],
            );
        }
    }

    Ok(Json(FileDetailResponse {
        hash: hash.clone(),
        total_size,
        reconstruction: QueryReconstructionResponse {
            offset_into_first_range: 0,
            terms,
            fetch_info,
        },
    }))
}

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

// ─── GET /api/files/:hash/content ─────────────────────────────────────────────

async fn get_file_content(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    use crate::storage::validate_hash;

    validate_hash(&hash)?;

    let shard_hash = state
        .file_index
        .get(&hash)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("file not found: {hash}")))?;

    let shard_data = state.storage.get_shard(&shard_hash).await?;
    let shard = Shard::from_bytes(&shard_data)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt shard {shard_hash}: {e}")))?;

    let file_block = shard
        .file_info_blocks
        .iter()
        .find(|b| b.header.file_hash.to_hex() == hash)
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!(
                "file {hash} not found in shard {shard_hash}"
            ))
        })?;

    // Reconstruct the file by reading and decompressing chunks from each xorb
    let mut file_bytes: Vec<u8> = Vec::new();

    for entry in &file_block.entries {
        let xorb_hash = entry.cas_hash.to_hex();
        let xorb_data = state.storage.get_xorb(&xorb_hash).await?;

        let chunks = openxet_cas_types::xorb::deserialize_xorb_range(
            &xorb_data,
            entry.chunk_index_start as usize,
            entry.chunk_index_end as usize,
        )
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to read xorb {xorb_hash}: {e}")))?;

        for chunk in chunks {
            file_bytes.extend_from_slice(&chunk.data);
        }
    }

    let total_size = file_bytes.len() as u64;

    // Handle Range header for partial content requests
    if let Some(range_val) = headers.get(header::RANGE) {
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
        let end: u64 = if end_str.is_empty() {
            total_size - 1
        } else {
            end_str
                .parse()
                .map_err(|_| AppError::BadRequest("invalid range end".to_string()))?
        };

        if start > end || start >= total_size {
            return Err(AppError::RangeNotSatisfiable);
        }

        let end = end.min(total_size - 1);
        let slice = file_bytes[start as usize..=end as usize].to_vec();
        let content_range = format!("bytes {start}-{end}/{total_size}");

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

// ─── GET /api/xorbs ──────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct XorbEntry {
    pub hash: String,
    pub size: u64,
    pub chunk_count: usize,
}

fn count_xorb_chunks(xorb_data: &[u8]) -> usize {
    let mut count = 0;
    let mut pos = 0usize;

    while pos + ChunkHeader::SIZE <= xorb_data.len() {
        let header_bytes: [u8; 8] = xorb_data[pos..pos + 8].try_into().unwrap();
        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };
        pos += ChunkHeader::SIZE + header.compressed_size as usize;
        count += 1;
    }

    count
}

async fn get_xorbs(State(state): State<AppState>) -> Result<Json<Vec<XorbEntry>>, AppError> {
    let xorbs = state.storage.list_xorbs().await?;

    let mut entries = Vec::with_capacity(xorbs.len());
    for (hash, size) in xorbs {
        let chunk_count = match state.storage.get_xorb(&hash).await {
            Ok(data) => count_xorb_chunks(&data),
            Err(_) => 0,
        };

        entries.push(XorbEntry {
            hash,
            size,
            chunk_count,
        });
    }

    Ok(Json(entries))
}

// ─── POST /api/upload ────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct UploadResponse {
    pub file_hash: String,
    pub xorb_hashes: Vec<String>,
    pub shard_hash: String,
    pub file_size: u64,
    pub chunk_count: usize,
    pub xorb_count: usize,
}

async fn post_upload(
    State(state): State<AppState>,
    file_data: Bytes,
) -> Result<Json<UploadResponse>, AppError> {
    if file_data.is_empty() {
        return Err(AppError::BadRequest("empty upload body".to_string()));
    }

    let result = process_file_upload(&state, &file_data).await?;
    Ok(Json(result))
}

/// Shared pipeline: CDC chunk → hash → split into xorbs → shard → index.
///
/// Used by both the single-shot `POST /api/upload` endpoint and the
/// session-based multipart `complete` endpoint.
///
/// Large files are split across multiple xorbs, each staying under the
/// 64 MiB size limit.
pub(crate) async fn process_file_upload(
    state: &AppState,
    file_data: &[u8],
) -> Result<UploadResponse, AppError> {
    let file_size = file_data.len() as u64;

    // 1. Chunk the file using CDC
    let chunk_infos = chunk_data(file_data);
    let chunk_count = chunk_infos.len();

    // 2. Extract chunk data slices and compute hashes
    let chunk_slices: Vec<&[u8]> = chunk_infos
        .iter()
        .map(|ci| &file_data[ci.offset..ci.offset + ci.length])
        .collect();

    let chunk_hashes: Vec<MerkleHash> = chunk_slices
        .iter()
        .map(|data| compute_chunk_hash(data))
        .collect();

    let chunk_hashes_and_sizes: Vec<(MerkleHash, usize)> = chunk_hashes
        .iter()
        .zip(chunk_slices.iter())
        .map(|(h, d)| (*h, d.len()))
        .collect();

    // 3. Compute file hash over ALL chunks (independent of xorb splitting)
    let file_hash = compute_file_hash(&chunk_hashes_and_sizes);
    let file_hash_hex = file_hash.to_hex();

    // 4. Split chunks into xorb groups, serializing incrementally
    struct XorbGroup {
        global_start: usize,
        global_end: usize,
        xorb_bytes: Vec<u8>,
    }

    let mut xorb_groups: Vec<XorbGroup> = Vec::new();
    let mut current_buffer: Vec<u8> = Vec::new();
    let mut group_start: usize = 0;

    for (i, chunk_data) in chunk_slices.iter().enumerate() {
        let serialized = serialize_single_chunk(chunk_data, CompressionType::Lz4)?;

        if !current_buffer.is_empty() && current_buffer.len() + serialized.len() > XORB_SOFT_LIMIT {
            xorb_groups.push(XorbGroup {
                global_start: group_start,
                global_end: i,
                xorb_bytes: std::mem::take(&mut current_buffer),
            });
            group_start = i;
        }

        current_buffer.extend_from_slice(&serialized);
    }

    if !current_buffer.is_empty() {
        xorb_groups.push(XorbGroup {
            global_start: group_start,
            global_end: chunk_slices.len(),
            xorb_bytes: std::mem::take(&mut current_buffer),
        });
    }

    // 5. Store each xorb, index chunks, build shard metadata
    let mut file_data_entries: Vec<FileDataSequenceEntry> = Vec::new();
    let mut cas_info_blocks: Vec<CASInfoBlock> = Vec::new();
    let mut verification_entries: Vec<FileVerificationEntry> = Vec::new();
    let mut xorb_hashes_hex: Vec<String> = Vec::new();

    for group in &xorb_groups {
        let local_count = group.global_end - group.global_start;
        let group_hashes_and_sizes = &chunk_hashes_and_sizes[group.global_start..group.global_end];

        // Compute xorb hash from this group's chunk hashes+sizes
        let xorb_hash = compute_xorb_hash(group_hashes_and_sizes);
        let xorb_hash_hex = xorb_hash.to_hex();

        // Store xorb
        state
            .storage
            .put_xorb(&xorb_hash_hex, Bytes::from(group.xorb_bytes.clone()))
            .await?;

        // Index each chunk with its LOCAL index within this xorb
        for local_i in 0..local_count {
            let global_i = group.global_start + local_i;
            state
                .chunk_index
                .put(
                    &chunk_hashes[global_i].to_hex(),
                    crate::storage::ChunkLocation {
                        xorb_hash: xorb_hash_hex.clone(),
                        chunk_index: local_i as u32,
                    },
                )
                .await?;
        }

        // Total uncompressed bytes in this group
        let group_unpacked: u32 = (group.global_start..group.global_end)
            .map(|i| chunk_slices[i].len() as u32)
            .sum();

        // FileDataSequenceEntry for this xorb
        file_data_entries.push(FileDataSequenceEntry {
            cas_hash: xorb_hash,
            cas_flags: 0,
            unpacked_segment_bytes: group_unpacked,
            chunk_index_start: 0,
            chunk_index_end: local_count as u32,
        });

        // Verification hash for this group's chunks
        let group_chunk_hashes: Vec<MerkleHash> = (group.global_start..group.global_end)
            .map(|i| chunk_hashes[i])
            .collect();
        verification_entries.push(FileVerificationEntry {
            range_hash: compute_verification_hash(&group_chunk_hashes),
        });

        // CASInfoBlock for this xorb
        let mut byte_offset = 0u32;
        let cas_entries: Vec<CASChunkSequenceEntry> = (group.global_start..group.global_end)
            .map(|i| {
                let size = chunk_slices[i].len() as u32;
                let entry = CASChunkSequenceEntry {
                    chunk_hash: chunk_hashes[i],
                    chunk_byte_range_start: byte_offset,
                    unpacked_segment_bytes: size,
                };
                byte_offset += size;
                entry
            })
            .collect();

        cas_info_blocks.push(CASInfoBlock {
            header: CASChunkSequenceHeader {
                cas_hash: xorb_hash,
                cas_flags: 0,
                num_entries: local_count as u32,
                num_bytes_in_cas: group_unpacked,
                num_bytes_on_disk: group.xorb_bytes.len() as u32,
            },
            entries: cas_entries,
        });

        xorb_hashes_hex.push(xorb_hash_hex);
    }

    // 6. Build and store shard
    let xorb_count = xorb_groups.len();

    let file_info_block = FileInfoBlock {
        header: FileDataSequenceHeader {
            file_hash,
            file_flags: MDB_FILE_FLAG_WITH_VERIFICATION,
            num_entries: file_data_entries.len() as u32,
        },
        entries: file_data_entries,
        verification_entries,
        metadata_ext: None,
    };

    let shard = Shard {
        header: ShardHeader::new(0), // upload shards: footer_size = 0
        file_info_blocks: vec![file_info_block],
        cas_info_blocks,
        footer: None,
    };

    let shard_bytes = shard
        .to_bytes()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to serialize shard: {e}")))?;

    let shard_hash_bytes = blake3::hash(&shard_bytes);
    let shard_hash = MerkleHash::from_bytes(*shard_hash_bytes.as_bytes());
    let shard_hash_hex = shard_hash.to_hex();

    state
        .storage
        .put_shard(&shard_hash_hex, Bytes::from(shard_bytes))
        .await?;

    // Index the file
    state
        .file_index
        .put(&file_hash_hex, &shard_hash_hex)
        .await?;

    Ok(UploadResponse {
        file_hash: file_hash_hex,
        xorb_hashes: xorb_hashes_hex,
        shard_hash: shard_hash_hex,
        file_size,
        chunk_count,
        xorb_count,
    })
}
