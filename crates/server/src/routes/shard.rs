use axum::Json;
use axum::extract::State;
use bytes::Bytes;
use serde::Serialize;

use openxet_cas_types::shard::{MAX_SHARD_SIZE, Shard};
use openxet_cas_types::xorb::deserialize_xorb_range;
use openxet_hashing::{MerkleHash, compute_chunk_hash, compute_verification_hash};

use crate::auth::RequireWrite;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::index::ChunkLocation;
use crate::storage::{ChunkIndex, FileIndex, StorageBackend, validate_hash};

#[derive(Debug, Serialize)]
pub struct ShardUploadResponse {
    pub result: u8,
}

pub async fn post_shard(
    State(state): State<AppState>,
    _auth: RequireWrite,
    body: Bytes,
) -> Result<Json<ShardUploadResponse>, AppError> {
    if body.len() > MAX_SHARD_SIZE {
        return Err(AppError::PayloadTooLarge);
    }

    // Parse the shard
    let shard = Shard::from_bytes(&body)?;

    // Uploaded shards MUST NOT have a footer
    if shard.header.footer_size != 0 {
        return Err(AppError::BadRequest(
            "upload shards must have footer_size=0".to_string(),
        ));
    }

    // Validate all referenced xorbs exist and verify verification hashes
    for file_block in &shard.file_info_blocks {
        for (i, entry) in file_block.entries.iter().enumerate() {
            let xorb_hash_hex = entry.cas_hash.to_hex();
            validate_hash(&xorb_hash_hex)?;

            if !state.storage.xorb_exists(&xorb_hash_hex).await? {
                return Err(AppError::BadRequest(format!(
                    "referenced xorb not found: {xorb_hash_hex}"
                )));
            }

            // Validate verification entry if present
            if i < file_block.verification_entries.len() {
                let xorb_data = state.storage.get_xorb(&xorb_hash_hex).await?;
                let chunk_start = entry.chunk_index_start as usize;
                let chunk_end = entry.chunk_index_end as usize;

                let chunks =
                    deserialize_xorb_range(&xorb_data, chunk_start, chunk_end).map_err(|e| {
                        AppError::BadRequest(format!(
                            "failed to read xorb {xorb_hash_hex} chunks [{chunk_start},{chunk_end}): {e}"
                        ))
                    })?;

                let chunk_hashes: Vec<MerkleHash> =
                    chunks.iter().map(|c| compute_chunk_hash(&c.data)).collect();

                let computed = compute_verification_hash(&chunk_hashes);
                let expected = &file_block.verification_entries[i].range_hash;

                if &computed != expected {
                    return Err(AppError::BadRequest(format!(
                        "verification hash mismatch for file {} term {i}: computed={}, expected={}",
                        file_block.header.file_hash.to_hex(),
                        computed.to_hex(),
                        expected.to_hex()
                    )));
                }
            }
        }
    }

    // Content-address the shard: blake3 hash of the raw bytes (unkeyed)
    let shard_hash_bytes = blake3::hash(&body);
    let shard_hash = MerkleHash::from_bytes(*shard_hash_bytes.as_bytes());
    let shard_hash_hex = shard_hash.to_hex();

    // Store the shard
    let was_inserted = state.storage.put_shard(&shard_hash_hex, body).await?;

    // Index file hashes
    for file_block in &shard.file_info_blocks {
        let file_hash_hex = file_block.header.file_hash.to_hex();
        state
            .file_index
            .put(&file_hash_hex, &shard_hash_hex)
            .await?;
    }

    // Index chunk hashes from CAS info section
    for cas_block in &shard.cas_info_blocks {
        let xorb_hash_hex = cas_block.header.cas_hash.to_hex();
        for (i, entry) in cas_block.entries.iter().enumerate() {
            state
                .chunk_index
                .put(
                    &entry.chunk_hash.to_hex(),
                    ChunkLocation {
                        xorb_hash: xorb_hash_hex.clone(),
                        chunk_index: i as u32,
                    },
                )
                .await?;
        }
    }

    Ok(Json(ShardUploadResponse {
        result: if was_inserted { 1 } else { 0 },
    }))
}
