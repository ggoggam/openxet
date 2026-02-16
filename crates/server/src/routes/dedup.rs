use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use openxet_cas_types::chunk::ChunkHeader;
use openxet_cas_types::shard::{
    CASChunkSequenceEntry, CASChunkSequenceHeader, CASInfoBlock, FOOTER_SIZE,
    MDB_SHARD_FOOTER_VERSION, MDB_SHARD_HEADER_VERSION, Shard, ShardFooter, ShardHeader,
};
use openxet_hashing::MerkleHash;

use crate::auth::RequireRead;
use crate::error::AppError;
use crate::state::AppState;
use crate::storage::{ChunkIndex, StorageBackend, validate_hash};

type HmacSha256 = Hmac<Sha256>;

/// HMAC a MerkleHash with the given key, returning a new MerkleHash.
fn hmac_hash(key: &[u8; 32], hash: &MerkleHash) -> MerkleHash {
    let mut mac =
        HmacSha256::new_from_slice(key).expect("HMAC key length is always valid for SHA256");
    mac.update(hash.as_bytes());
    let result = mac.finalize().into_bytes();
    MerkleHash::from_bytes(result.into())
}

/// Parse chunk hashes from a xorb binary by walking chunk headers.
fn parse_xorb_chunk_hashes(xorb_data: &[u8]) -> Vec<(MerkleHash, u32, u32)> {
    let mut result = Vec::new();
    let mut pos = 0usize;
    let mut byte_offset = 0u32;

    while pos + ChunkHeader::SIZE <= xorb_data.len() {
        let header_bytes: [u8; 8] = xorb_data[pos..pos + 8].try_into().unwrap();
        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };

        let compressed_start = pos + ChunkHeader::SIZE;
        let compressed_end = compressed_start + header.compressed_size as usize;
        if compressed_end > xorb_data.len() {
            break;
        }

        // Decompress to compute the chunk hash
        let compressed = &xorb_data[compressed_start..compressed_end];
        let decompressed = openxet_cas_types::chunk::decompress_chunk(
            compressed,
            header.compression_type,
            header.uncompressed_size as usize,
        );

        if let Ok(data) = decompressed {
            let chunk_hash = openxet_hashing::compute_chunk_hash(&data);
            result.push((chunk_hash, byte_offset, data.len() as u32));
        }

        byte_offset += header.uncompressed_size;
        pos = compressed_end;
    }

    result
}

pub async fn get_dedup(
    State(state): State<AppState>,
    _auth: RequireRead,
    Path(hash): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    validate_hash(&hash)?;

    // Look up chunk in index
    let locations = state.chunk_index.get(&hash).await?;
    if locations.is_empty() {
        return Err(AppError::NotFound(format!("chunk not found: {hash}")));
    }

    // Group by xorb_hash
    let mut xorb_map: HashMap<String, Vec<u32>> = HashMap::new();
    for loc in &locations {
        xorb_map
            .entry(loc.xorb_hash.clone())
            .or_default()
            .push(loc.chunk_index);
    }

    // Generate random HMAC key
    let hmac_key: [u8; 32] = rand::random();

    // Build CAS info blocks
    let mut cas_info_blocks = Vec::new();

    for xorb_hash_hex in xorb_map.keys() {
        let xorb_data = state.storage.get_xorb(xorb_hash_hex).await?;
        let chunk_info = parse_xorb_chunk_hashes(&xorb_data);

        let xorb_hash = MerkleHash::from_hex(xorb_hash_hex)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("bad stored hash: {e}")))?;

        let total_uncompressed: u32 = chunk_info.iter().map(|(_, _, size)| size).sum();

        let entries: Vec<CASChunkSequenceEntry> = chunk_info
            .iter()
            .map(|(ch, byte_start, size)| CASChunkSequenceEntry {
                chunk_hash: hmac_hash(&hmac_key, ch),
                chunk_byte_range_start: *byte_start,
                unpacked_segment_bytes: *size,
            })
            .collect();

        cas_info_blocks.push(CASInfoBlock {
            header: CASChunkSequenceHeader {
                cas_hash: xorb_hash,
                cas_flags: 0,
                num_entries: entries.len() as u32,
                num_bytes_in_cas: total_uncompressed,
                num_bytes_on_disk: xorb_data.len() as u32,
            },
            entries,
        });
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Build the shard with footer
    let shard = Shard {
        header: ShardHeader {
            tag: openxet_cas_types::shard::MDB_SHARD_HEADER_TAG,
            version: MDB_SHARD_HEADER_VERSION,
            footer_size: FOOTER_SIZE as u64,
        },
        file_info_blocks: vec![], // dedup responses have no file info
        cas_info_blocks,
        footer: Some(ShardFooter {
            version: MDB_SHARD_FOOTER_VERSION,
            file_info_offset: 0,
            cas_info_offset: 0,
            chunk_hash_hmac_key: hmac_key,
            shard_creation_timestamp: now,
            shard_key_expiry: now + state.config.auth.shard_key_ttl_seconds,
            footer_offset: 0,
        }),
    };

    let shard_bytes = shard
        .to_bytes()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("failed to serialize shard: {e}")))?;

    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        shard_bytes,
    ))
}
