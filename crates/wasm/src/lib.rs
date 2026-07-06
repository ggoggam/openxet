//! openxet-wasm — the Xet client-side upload pipeline, compiled to WebAssembly
//! for the web UI.
//!
//! The Xet wire protocol requires the *client* to chunk, hash, and pack files
//! (`POST /v1/xorbs`, `POST /v1/shards`). This crate reuses the workspace's own
//! chunking/hashing/cas-types crates — the same code the server validates
//! against — so browser uploads are wire-compatible by construction.
//!
//! `plan_upload` is pure: it turns file bytes into upload artifacts; the
//! JavaScript side performs the HTTP calls.

use wasm_bindgen::prelude::*;

use openxet_cas_types::chunk::CompressionType;
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

/// Upload artifacts for one file: the xorbs to POST, the shard that registers
/// the file, and the file's content hash (its identity).
#[wasm_bindgen]
pub struct UploadPlan {
    file_hash: String,
    chunk_count: usize,
    xorbs: Vec<(String, Vec<u8>)>,
    shard_bytes: Vec<u8>,
}

#[wasm_bindgen]
impl UploadPlan {
    #[wasm_bindgen(getter)]
    pub fn file_hash(&self) -> String {
        self.file_hash.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    #[wasm_bindgen(getter)]
    pub fn xorb_count(&self) -> usize {
        self.xorbs.len()
    }

    pub fn xorb_hash(&self, i: usize) -> String {
        self.xorbs[i].0.clone()
    }

    pub fn xorb_data(&self, i: usize) -> Vec<u8> {
        self.xorbs[i].1.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn shard_bytes(&self) -> Vec<u8> {
        self.shard_bytes.clone()
    }
}

#[wasm_bindgen]
pub fn plan_upload(data: &[u8]) -> Result<UploadPlan, JsError> {
    build_upload_plan(data).map_err(|e| JsError::new(&e))
}

/// CDC chunk → hash → pack into xorbs → build the upload shard.
///
/// Mirrors `openxet-client`'s put pipeline minus the global-dedup query.
// ponytail: no browser-side global dedup — every chunk is uploaded; the server
// still dedups whole xorbs idempotently. Add /v1/chunks HMAC matching here if
// browser upload traffic ever matters.
pub fn build_upload_plan(data: &[u8]) -> Result<UploadPlan, String> {
    if data.is_empty() {
        return Err("refusing to upload an empty file".to_string());
    }

    // 1. Chunk + hash
    let chunk_infos = chunk_data(data);
    let chunk_slices: Vec<&[u8]> = chunk_infos
        .iter()
        .map(|ci| &data[ci.offset..ci.offset + ci.length])
        .collect();
    let chunk_hashes: Vec<MerkleHash> =
        chunk_slices.iter().map(|d| compute_chunk_hash(d)).collect();
    let hashes_and_sizes: Vec<(MerkleHash, usize)> = chunk_hashes
        .iter()
        .zip(&chunk_slices)
        .map(|(h, d)| (*h, d.len()))
        .collect();
    let file_hash = compute_file_hash(&hashes_and_sizes);

    // 2. Pack chunks into xorbs under the soft size limit
    struct Group {
        start: usize,
        end: usize,
        bytes: Vec<u8>,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut start = 0usize;
    for (i, slice) in chunk_slices.iter().enumerate() {
        let serialized =
            serialize_single_chunk(slice, CompressionType::Lz4).map_err(|e| e.to_string())?;
        if !buf.is_empty() && buf.len() + serialized.len() > XORB_SOFT_LIMIT {
            groups.push(Group {
                start,
                end: i,
                bytes: std::mem::take(&mut buf),
            });
            start = i;
        }
        buf.extend_from_slice(&serialized);
    }
    groups.push(Group {
        start,
        end: chunk_slices.len(),
        bytes: buf,
    });

    // 3. Per-xorb shard metadata
    let mut xorbs = Vec::with_capacity(groups.len());
    let mut entries = Vec::with_capacity(groups.len());
    let mut verifications = Vec::with_capacity(groups.len());
    let mut cas_info_blocks = Vec::with_capacity(groups.len());

    for g in &groups {
        let group_hs = &hashes_and_sizes[g.start..g.end];
        let xorb_hash = compute_xorb_hash(group_hs);
        let unpacked: u32 = group_hs.iter().map(|(_, s)| *s as u32).sum();

        entries.push(FileDataSequenceEntry {
            cas_hash: xorb_hash,
            cas_flags: 0,
            unpacked_segment_bytes: unpacked,
            chunk_index_start: 0,
            chunk_index_end: (g.end - g.start) as u32,
        });
        verifications.push(FileVerificationEntry {
            range_hash: compute_verification_hash(&chunk_hashes[g.start..g.end]),
        });

        let mut byte_offset = 0u32;
        let cas_entries: Vec<CASChunkSequenceEntry> = group_hs
            .iter()
            .map(|(h, s)| {
                let entry = CASChunkSequenceEntry {
                    chunk_hash: *h,
                    chunk_byte_range_start: byte_offset,
                    unpacked_segment_bytes: *s as u32,
                };
                byte_offset += *s as u32;
                entry
            })
            .collect();
        cas_info_blocks.push(CASInfoBlock {
            header: CASChunkSequenceHeader {
                cas_hash: xorb_hash,
                cas_flags: 0,
                num_entries: cas_entries.len() as u32,
                num_bytes_in_cas: unpacked,
                num_bytes_on_disk: g.bytes.len() as u32,
            },
            entries: cas_entries,
        });

        xorbs.push((xorb_hash.to_hex(), g.bytes.clone()));
    }

    // 4. The upload shard
    let shard = Shard {
        header: ShardHeader::new(0),
        file_info_blocks: vec![FileInfoBlock {
            header: FileDataSequenceHeader {
                file_hash,
                file_flags: MDB_FILE_FLAG_WITH_VERIFICATION,
                num_entries: entries.len() as u32,
            },
            entries,
            verification_entries: verifications,
            metadata_ext: None,
        }],
        cas_info_blocks,
        footer: None,
    };
    let shard_bytes = shard.to_upload_bytes().map_err(|e| e.to_string())?;

    Ok(UploadPlan {
        file_hash: file_hash.to_hex(),
        chunk_count: chunk_slices.len(),
        xorbs,
        shard_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_covers_all_bytes_and_parses_back() {
        let data: Vec<u8> = (0..512 * 1024u32).map(|i| (i * 31 % 251) as u8).collect();
        let plan = build_upload_plan(&data).unwrap();

        assert_eq!(plan.file_hash.len(), 64);
        assert!(plan.chunk_count >= 1);
        assert_eq!(plan.xorbs.len(), 1); // 512 KiB fits one xorb

        // The xorb round-trips to the original bytes
        let chunks = openxet_cas_types::xorb::deserialize_xorb(&plan.xorbs[0].1).unwrap();
        let total: usize = chunks.iter().map(|c| c.data.len()).sum();
        assert_eq!(total, data.len());
        let rebuilt: Vec<u8> = chunks.into_iter().flat_map(|c| c.data).collect();
        assert_eq!(rebuilt, data);

        // The shard parses and references the file + xorb consistently
        let shard = Shard::from_bytes(&plan.shard_bytes).unwrap();
        assert_eq!(shard.file_info_blocks.len(), 1);
        let fb = &shard.file_info_blocks[0];
        assert_eq!(fb.header.file_hash.to_hex(), plan.file_hash);
        assert_eq!(fb.entries.len(), 1);
        assert_eq!(fb.entries[0].cas_hash.to_hex(), plan.xorbs[0].0);
        assert_eq!(fb.entries[0].unpacked_segment_bytes as usize, data.len());
    }

    #[test]
    fn empty_input_rejected() {
        assert!(build_upload_plan(&[]).is_err());
    }
}
