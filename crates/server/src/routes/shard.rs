use axum::Json;
use axum::extract::State;
use bytes::Bytes;
use serde::Serialize;

use openxet_cas_types::shard::{MAX_SHARD_SIZE, Shard};
use openxet_cas_types::xorb::deserialize_xorb_range;
use openxet_hashing::{
    MerkleHash, compute_chunk_hash, compute_file_hash, compute_verification_hash,
};

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

    // Validate every file block: referenced xorbs must exist, verification
    // hashes (when present) must match, and — critically — the declared
    // file_hash must equal the hash recomputed from the actual chunk content.
    // Without that last check a writer could register arbitrary bytes under any
    // file hash and poison later reconstructions (the core CAS invariant).
    for file_block in &shard.file_info_blocks {
        if file_block.entries.is_empty() {
            return Err(AppError::BadRequest(
                "file info block has no entries".to_string(),
            ));
        }

        // (chunk_hash, size) for the whole file, in term order, to rebuild the
        // file hash.
        let mut file_chunks: Vec<(MerkleHash, usize)> = Vec::new();

        for (i, entry) in file_block.entries.iter().enumerate() {
            let xorb_hash_hex = entry.cas_hash.to_hex();
            validate_hash(&xorb_hash_hex)?;

            if !state.storage.xorb_exists(&xorb_hash_hex).await? {
                return Err(AppError::BadRequest(format!(
                    "referenced xorb not found: {xorb_hash_hex}"
                )));
            }

            let chunk_start = entry.chunk_index_start as usize;
            let chunk_end = entry.chunk_index_end as usize;
            if chunk_end <= chunk_start {
                return Err(AppError::BadRequest(format!(
                    "file {} term {i} has empty or inverted chunk range [{chunk_start},{chunk_end})",
                    file_block.header.file_hash.to_hex()
                )));
            }

            let xorb_data = state.storage.get_xorb(&xorb_hash_hex).await?;
            let chunks =
                deserialize_xorb_range(&xorb_data, chunk_start, chunk_end).map_err(|e| {
                    AppError::BadRequest(format!(
                        "failed to read xorb {xorb_hash_hex} chunks [{chunk_start},{chunk_end}): {e}"
                    ))
                })?;

            // The term must resolve to exactly the chunks it claims.
            if chunks.len() != chunk_end - chunk_start {
                return Err(AppError::BadRequest(format!(
                    "xorb {xorb_hash_hex} term [{chunk_start},{chunk_end}) resolved {} chunks",
                    chunks.len()
                )));
            }

            let chunk_hashes: Vec<MerkleHash> =
                chunks.iter().map(|c| compute_chunk_hash(&c.data)).collect();

            // Validate the per-term verification hash if the shard carries one.
            if i < file_block.verification_entries.len() {
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

            for (h, c) in chunk_hashes.iter().zip(chunks.iter()) {
                file_chunks.push((*h, c.data.len()));
            }
        }

        let computed_file_hash = compute_file_hash(&file_chunks);
        if computed_file_hash != file_block.header.file_hash {
            return Err(AppError::BadRequest(format!(
                "file hash mismatch: declared={}, computed={}",
                file_block.header.file_hash.to_hex(),
                computed_file_hash.to_hex()
            )));
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

        // If the writer attached a sha256 (e.g. git-xet uses the Git LFS oid),
        // alias it to the file hash so clients can fetch by sha256. xet-core
        // stores the digest with MerkleHash's u64-LE byte order, so hex it the
        // same way to recover the original sha256 hex.
        if let Some(ext) = &file_block.metadata_ext {
            let sha_hex = MerkleHash::from_bytes(ext.sha256).to_hex();
            state
                .file_index
                .put(&format!("sha256:{sha_hex}"), &file_hash_hex)
                .await?;
        }
    }

    // Index chunk hashes from CAS info section in one batched write
    let entries: Vec<(String, ChunkLocation)> = shard
        .cas_info_blocks
        .iter()
        .flat_map(|cas_block| {
            let xorb_hash_hex = cas_block.header.cas_hash.to_hex();
            cas_block.entries.iter().enumerate().map(move |(i, entry)| {
                (
                    entry.chunk_hash.to_hex(),
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
