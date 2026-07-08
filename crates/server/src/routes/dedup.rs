use std::collections::HashMap;
use std::io::Cursor;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;

use xet_core_structures::merklehash::MerkleHash;
use xet_core_structures::metadata_shard::MDBShardInfo;
use xet_core_structures::metadata_shard::shard_in_memory::MDBInMemoryShard;
use xet_core_structures::metadata_shard::xorb_structs::{
    MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};

use crate::auth::RequireRead;
use crate::error::AppError;
use crate::routes::xorb_meta::{chunk_info_triples, xorb_info_from_stored};
use crate::state::AppState;
use crate::storage::{ChunkIndex, StorageBackend, validate_hash};

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

    // the spec permits adding likely-related xorbs to raise dedup hit rates —
    // add if cross-file dedup misses show up in practice.
    let mut xorb_map: HashMap<String, Vec<u32>> = HashMap::new();
    for loc in &locations {
        xorb_map
            .entry(loc.xorb_hash.clone())
            .or_default()
            .push(loc.chunk_index);
    }

    // Assemble the response shard's xorb blocks with *raw* chunk hashes;
    // export_as_keyed_shard below applies the HMAC key to every chunk hash
    // and stamps the key + expiry into the footer, exactly the shape
    // xet-core clients expect from a global-dedup query.
    let mut mem_shard = MDBInMemoryShard::default();

    for xorb_hash_hex in xorb_map.keys() {
        let xorb_hash = MerkleHash::from_hex(xorb_hash_hex)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("bad stored hash: {e}")))?;

        // Prefer the recorded layout (no object-store fetch, no parsing);
        // fall back to reading the xorb for entries stored before layouts were
        // recorded. Both yield (chunk_hash, byte_offset, uncompressed_size).
        let (chunk_info, num_bytes_on_disk): (Vec<(MerkleHash, u32, u32)>, u32) =
            match state.chunk_index.get_xorb_layout(xorb_hash_hex).await? {
                Some(layout) => {
                    let mut byte_offset = 0u32;
                    let mut info = Vec::with_capacity(layout.chunks.len());
                    for chunk in &layout.chunks {
                        let ch = MerkleHash::from_hex(&chunk.chunk_hash).map_err(|e| {
                            AppError::Internal(anyhow::anyhow!("bad stored chunk hash: {e}"))
                        })?;
                        info.push((ch, byte_offset, chunk.unpacked_size));
                        byte_offset += chunk.unpacked_size;
                    }
                    (info, layout.num_bytes_on_disk)
                }
                None => {
                    let xorb_data = state.storage.get_xorb(xorb_hash_hex).await?;
                    let on_disk = xorb_data.len() as u32;
                    let info = xorb_info_from_stored(&xorb_data)?;
                    (chunk_info_triples(&info), on_disk)
                }
            };

        let total_uncompressed: u32 = chunk_info.iter().map(|(_, _, size)| size).sum();

        let entries: Vec<XorbChunkSequenceEntry> = chunk_info
            .iter()
            .map(|(chunk_hash, byte_start, size)| {
                XorbChunkSequenceEntry::new(*chunk_hash, *size, *byte_start)
            })
            .collect();

        let mut header = XorbChunkSequenceHeader::new(xorb_hash, entries.len(), total_uncompressed);
        header.num_bytes_on_disk = num_bytes_on_disk;

        mem_shard
            .add_xorb_block(MDBXorbInfo {
                metadata: header,
                chunks: entries,
            })
            .map_err(|e| AppError::Internal(anyhow::anyhow!("assembling dedup shard: {e}")))?;
    }

    // Serialize unkeyed, then re-export keyed: a fresh random HMAC key per
    // response, valid for the configured TTL.
    let mut unkeyed = Vec::new();
    let shard_info = MDBShardInfo::serialize_from(&mut unkeyed, &mem_shard, None)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing dedup shard: {e}")))?;

    let hmac_key: MerkleHash = rand::random::<[u8; 32]>().into();
    let key_ttl = Duration::from_secs(state.config.auth.shard_key_ttl_seconds);

    let mut shard_bytes = Vec::new();
    shard_info
        .export_as_keyed_shard(
            &mut Cursor::new(&unkeyed[..]),
            &mut shard_bytes,
            hmac_key,
            key_ttl,
            false, // no file info in dedup responses
            true,  // xorb lookup table
            true,  // chunk lookup table (what clients query against)
        )
        .map_err(|e| AppError::Internal(anyhow::anyhow!("keying dedup shard: {e}")))?;

    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream")],
        shard_bytes,
    ))
}
