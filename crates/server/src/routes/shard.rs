use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use bytes::Bytes;
use serde::Serialize;

use xet_core_structures::merklehash::{MerkleHash, compute_data_hash, file_hash};
use xet_core_structures::metadata_shard::MDBShardFileHeader;
use xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks;
use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;
use xet_core_structures::xorb_object::XorbObjectInfoV1;

use crate::auth::RequireWrite;
use crate::error::AppError;
use crate::routes::xorb_meta::xorb_info_from_stored;
use crate::state::AppState;
use crate::storage::index::ChunkLocation;
use crate::storage::{ChunkIndex, FileIndex, OwnershipClaim, StorageBackend, validate_hash};

/// Maximum shard size: 64 MiB.
pub(crate) const MAX_SHARD_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct ShardUploadResponse {
    pub result: u8,
}

pub async fn post_shard(
    State(state): State<AppState>,
    RequireWrite(claims): RequireWrite,
    body: Bytes,
) -> Result<Json<ShardUploadResponse>, AppError> {
    if body.len() > MAX_SHARD_SIZE {
        return Err(AppError::PayloadTooLarge);
    }

    // Uploaded shards MUST NOT have a footer (xet-core strips it before
    // uploading; the footer is only present in shards the server hands out).
    let header = MDBShardFileHeader::deserialize(&mut Cursor::new(&body[..]))
        .map_err(|e| AppError::BadRequest(format!("invalid shard: {e}")))?;
    if header.footer_size != 0 {
        return Err(AppError::BadRequest(
            "upload shards must have footer_size=0".to_string(),
        ));
    }

    let shard = MDBMinimalShard::from_reader(&mut Cursor::new(&body[..]), true, true)
        .map_err(|e| AppError::BadRequest(format!("invalid shard: {e}")))?;

    // Validate every file block: referenced xorbs must exist, verification
    // hashes (when present) must match, and — critically — the declared
    // file_hash must equal the hash recomputed from the actual chunk content.
    // Without that last check a writer could register arbitrary bytes under any
    // file hash and poison later reconstructions (the core CAS invariant).
    // Chunk hashes come from the stored xorbs' metadata footers, which were
    // themselves verified against the chunk data at xorb upload time.
    let mut xorb_meta_cache: HashMap<String, Arc<XorbObjectInfoV1>> = HashMap::new();

    // (file_hash, logical_bytes) per validated file, for ownership accounting.
    let mut file_sizes: Vec<(String, u64)> = Vec::new();

    for file_idx in 0..shard.num_files() {
        let file_view = shard.file(file_idx).expect("index in range");
        let num_entries = file_view.num_entries();
        if num_entries == 0 {
            return Err(AppError::BadRequest(
                "file info block has no entries".to_string(),
            ));
        }

        // (chunk_hash, size) for the whole file, in term order, to rebuild the
        // file hash.
        let mut file_chunks: Vec<(MerkleHash, u64)> = Vec::new();

        for term_idx in 0..num_entries {
            let entry = file_view.entry(term_idx);
            let xorb_hash_hex = entry.xorb_hash.hex();
            validate_hash(&xorb_hash_hex)?;

            let chunk_start = entry.chunk_index_start as usize;
            let chunk_end = entry.chunk_index_end as usize;
            if chunk_end <= chunk_start {
                return Err(AppError::BadRequest(format!(
                    "file {} term {term_idx} has empty or inverted chunk range [{chunk_start},{chunk_end})",
                    file_view.file_hash().hex()
                )));
            }

            let info = match xorb_meta_cache.get(&xorb_hash_hex) {
                Some(info) => info.clone(),
                None => {
                    if !state.storage.xorb_exists(&xorb_hash_hex).await? {
                        return Err(AppError::BadRequest(format!(
                            "referenced xorb not found: {xorb_hash_hex}"
                        )));
                    }
                    let xorb_data = state.storage.get_xorb(&xorb_hash_hex).await?;
                    let info = Arc::new(xorb_info_from_stored(&xorb_data)?);
                    xorb_meta_cache.insert(xorb_hash_hex.clone(), info.clone());
                    info
                }
            };

            if chunk_end > info.num_chunks as usize {
                return Err(AppError::BadRequest(format!(
                    "xorb {xorb_hash_hex} term [{chunk_start},{chunk_end}) exceeds its {} chunks",
                    info.num_chunks
                )));
            }

            let chunk_hashes = &info.chunk_hashes[chunk_start..chunk_end];

            // Validate the per-term verification hash if the shard carries one.
            if file_view.contains_verification() {
                let computed = range_hash_from_chunks(chunk_hashes);
                let expected = file_view.verification(term_idx).range_hash;

                if computed != expected {
                    return Err(AppError::BadRequest(format!(
                        "verification hash mismatch for file {} term {term_idx}: computed={}, expected={}",
                        file_view.file_hash().hex(),
                        computed.hex(),
                        expected.hex()
                    )));
                }
            }

            let mut prev_unpacked = if chunk_start == 0 {
                0
            } else {
                info.unpacked_chunk_offsets[chunk_start - 1]
            };
            for k in chunk_start..chunk_end {
                let unpacked_end = info.unpacked_chunk_offsets[k];
                file_chunks.push((info.chunk_hashes[k], (unpacked_end - prev_unpacked) as u64));
                prev_unpacked = unpacked_end;
            }
        }

        let computed_file_hash = file_hash(&file_chunks);
        if computed_file_hash != file_view.file_hash() {
            return Err(AppError::BadRequest(format!(
                "file hash mismatch: declared={}, computed={}",
                file_view.file_hash().hex(),
                computed_file_hash.hex()
            )));
        }

        let logical_bytes: u64 = file_chunks.iter().map(|(_, size)| size).sum();
        file_sizes.push((computed_file_hash.hex(), logical_bytes));
    }

    // Content-address the shard the same way xet-core names shard files:
    // keyed blake3 (compute_data_hash) over the raw bytes.
    let shard_hash_hex = compute_data_hash(&body).hex();

    // Store the shard
    let was_inserted = state.storage.put_shard(&shard_hash_hex, body).await?;

    // Index file hashes and record the uploader's ownership claim on each —
    // the accounting record and the deletion refcount.
    let owner = claims.owner();
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for (file_hash_hex, logical_bytes) in &file_sizes {
        state.file_index.put(file_hash_hex, &shard_hash_hex).await?;
        state
            .file_index
            .claim(
                owner,
                file_hash_hex,
                OwnershipClaim {
                    logical_bytes: *logical_bytes,
                    created_at_unix: now_unix,
                },
            )
            .await?;
    }

    // Index chunk hashes from the xorb (CAS info) section in one batched write
    let entries: Vec<(String, ChunkLocation)> = (0..shard.num_xorb())
        .filter_map(|i| shard.xorb(i))
        .flat_map(|xorb_view| {
            let xorb_hash_hex = xorb_view.xorb_hash().hex();
            (0..xorb_view.num_entries()).map(move |i| {
                (
                    xorb_view.chunk(i).chunk_hash.hex(),
                    ChunkLocation {
                        xorb_hash: xorb_hash_hex.clone(),
                        chunk_index: i as u32,
                    },
                )
            })
        })
        .collect();
    state.chunk_index.put_batch(entries).await?;

    Ok(Json(ShardUploadResponse {
        result: if was_inserted { 1 } else { 0 },
    }))
}
