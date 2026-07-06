//! openxet-wasm — the Xet client-side upload pipeline, compiled to WebAssembly
//! for the web UI.
//!
//! The Xet wire protocol requires the *client* to chunk, hash, and pack files
//! (`POST /v1/xorbs`, `POST /v1/shards`). This crate reuses the workspace's own
//! chunking/hashing/cas-types crates — the same code the server validates
//! against — so browser uploads are wire-compatible by construction.
//!
//! Upload is a two-phase session (mirroring `openxet-client`'s put pipeline):
//!
//! 1. `UploadSession::new(data)` chunks + hashes. JS then drives the global
//!    dedup loop: `next_query_hash()` → `GET /v1/chunks/default-merkledb/{h}`
//!    → on hit, `apply_dedup_shard(bytes)` resolves chunks to existing xorbs.
//! 2. `finish()` packs only the still-unresolved chunks into new xorbs and
//!    builds the shard; JS performs the HTTP uploads.

use hmac::{Hmac, Mac};
use sha2::Sha256;
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

type HmacSha256 = Hmac<Sha256>;

fn hmac_chunk(key: &[u8; 32], hash: &MerkleHash) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("valid key length");
    mac.update(hash.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Where a file chunk lives, once resolved.
#[derive(Clone)]
struct Placement {
    xorb_hash: MerkleHash,
    index_in_xorb: u32,
}

/// Only 1-in-64 chunks are probed against the server in a miss region —
/// roughly one query per 4 MiB at the 64 KiB target chunk size. A hit
/// resolves whole xorbs at a time, so re-uploads of existing data still
/// dedup near-fully; fresh uploads pay ~n/64 probe round trips, not n.
// ponytail: coarse probe mask; shrink the mask (more probes) if partial-file
// dedup recall ever matters more than first-upload latency.
const PROBE_MASK: u8 = 0x3F;

/// Upload artifacts for one file: the xorbs to POST, the shard that registers
/// the file, and the file's content hash (its identity).
#[wasm_bindgen]
pub struct UploadPlan {
    file_hash: String,
    chunk_count: usize,
    deduped_chunk_count: usize,
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

    /// Chunks resolved to already-stored xorbs (not uploaded again).
    #[wasm_bindgen(getter)]
    pub fn deduped_chunk_count(&self) -> usize {
        self.deduped_chunk_count
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

    pub fn xorb_size(&self, i: usize) -> usize {
        self.xorbs[i].1.len()
    }

    #[wasm_bindgen(getter)]
    pub fn shard_bytes(&self) -> Vec<u8> {
        self.shard_bytes.clone()
    }
}

/// Chunk → dedup-query → pack pipeline state for one file upload.
#[wasm_bindgen]
pub struct UploadSession {
    data: Vec<u8>,
    ranges: Vec<(usize, usize)>, // (offset, length) per chunk
    chunk_hashes: Vec<MerkleHash>,
    placement: Vec<Option<Placement>>,
    probed: Vec<bool>, // chunks already handed out as probe candidates
}

#[wasm_bindgen]
impl UploadSession {
    #[wasm_bindgen(constructor)]
    pub fn new(data: Vec<u8>) -> Result<UploadSession, JsError> {
        if data.is_empty() {
            return Err(JsError::new("refusing to upload an empty file"));
        }
        let chunk_infos = chunk_data(&data);
        let ranges: Vec<(usize, usize)> =
            chunk_infos.iter().map(|ci| (ci.offset, ci.length)).collect();
        let chunk_hashes: Vec<MerkleHash> = ranges
            .iter()
            .map(|&(o, l)| compute_chunk_hash(&data[o..o + l]))
            .collect();
        let n = chunk_hashes.len();
        Ok(UploadSession {
            data,
            ranges,
            chunk_hashes,
            placement: vec![None; n],
            probed: vec![false; n],
        })
    }

    /// Next batch of chunk hashes to query against
    /// `/v1/chunks/default-merkledb/{hash}`, up to `max` per call; empty when
    /// the dedup pass is done. Candidates within a batch are independent, so
    /// JS may fire them concurrently, feeding each hit to `apply_dedup_shard`.
    /// Call again after the batch settles: hits create new continuation
    /// candidates, and already-resolved chunks drop out.
    ///
    /// Candidates: the first chunk, the first unresolved chunk after any
    /// resolved run (the continuation of known data), and 1-in-64 chunks
    /// inside miss regions. Each chunk is handed out at most once.
    pub fn next_query_batch(&mut self, max: usize) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..self.chunk_hashes.len() {
            if out.len() >= max {
                break;
            }
            if self.probed[i] || self.placement[i].is_some() {
                continue;
            }
            let after_resolved = i > 0 && self.placement[i - 1].is_some();
            let eligible = self.chunk_hashes[i].as_bytes()[0] & PROBE_MASK == 0;
            if i == 0 || after_resolved || eligible {
                self.probed[i] = true;
                out.push(self.chunk_hashes[i].to_hex());
            }
        }
        out
    }

    /// Apply a dedup shard response: HMAC-match our chunk hashes against its
    /// CAS entries and resolve every covered chunk to its existing xorb.
    pub fn apply_dedup_shard(&mut self, bytes: &[u8]) -> Result<(), JsError> {
        let shard = Shard::from_bytes(bytes)
            .map_err(|e| JsError::new(&format!("parsing dedup shard: {e}")))?;
        let footer = shard
            .footer
            .as_ref()
            .ok_or_else(|| JsError::new("dedup shard missing footer"))?;
        let key = &footer.chunk_hash_hmac_key;

        // HMAC(chunk_hash) -> (xorb_hash, index_in_xorb)
        let mut by_hmac = std::collections::HashMap::new();
        for block in &shard.cas_info_blocks {
            for (idx, entry) in block.entries.iter().enumerate() {
                by_hmac.insert(
                    *entry.chunk_hash.as_bytes(),
                    Placement {
                        xorb_hash: block.header.cas_hash,
                        index_in_xorb: idx as u32,
                    },
                );
            }
        }

        for j in 0..self.chunk_hashes.len() {
            if self.placement[j].is_some() {
                continue;
            }
            if let Some(p) = by_hmac.get(&hmac_chunk(key, &self.chunk_hashes[j])) {
                self.placement[j] = Some(p.clone());
            }
        }
        Ok(())
    }

    /// Pack the still-unresolved chunks into new xorbs and build the shard.
    /// The session is consumed; use the returned plan for the HTTP uploads.
    pub fn finish(&mut self) -> Result<UploadPlan, JsError> {
        build_plan(
            std::mem::take(&mut self.data),
            std::mem::take(&mut self.ranges),
            std::mem::take(&mut self.chunk_hashes),
            std::mem::take(&mut self.placement),
        )
        .map_err(|e| JsError::new(&e))
    }
}

/// Pack new chunks into xorbs, coalesce placements into reconstruction terms,
/// and build the upload shard. Mirrors `openxet-client`'s put steps 3–4.
fn build_plan(
    data: Vec<u8>,
    ranges: Vec<(usize, usize)>,
    chunk_hashes: Vec<MerkleHash>,
    mut placement: Vec<Option<Placement>>,
) -> Result<UploadPlan, String> {
    let n = chunk_hashes.len();
    let sizes: Vec<usize> = ranges.iter().map(|&(_, l)| l).collect();
    let slice = |i: usize| -> &[u8] {
        let (o, l) = ranges[i];
        &data[o..o + l]
    };

    let hashes_and_sizes: Vec<(MerkleHash, usize)> = chunk_hashes
        .iter()
        .copied()
        .zip(sizes.iter().copied())
        .collect();
    let file_hash = compute_file_hash(&hashes_and_sizes);
    let deduped_chunk_count = placement.iter().filter(|p| p.is_some()).count();

    // Pack unresolved chunks into new xorbs (greedy fill by serialized size).
    // `built` records each new xorb's hash and the global chunk indices it
    // holds, in order.
    let mut xorbs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut built: Vec<(MerkleHash, Vec<usize>)> = Vec::new();
    {
        fn seal(
            group: &mut Vec<usize>,
            buf: &mut Vec<u8>,
            chunk_hashes: &[MerkleHash],
            sizes: &[usize],
            placement: &mut [Option<Placement>],
            xorbs: &mut Vec<(String, Vec<u8>)>,
            built: &mut Vec<(MerkleHash, Vec<usize>)>,
        ) {
            let group_hs: Vec<(MerkleHash, usize)> =
                group.iter().map(|&k| (chunk_hashes[k], sizes[k])).collect();
            let xorb_hash = compute_xorb_hash(&group_hs);
            for (local_idx, &gi) in group.iter().enumerate() {
                placement[gi] = Some(Placement {
                    xorb_hash,
                    index_in_xorb: local_idx as u32,
                });
            }
            xorbs.push((xorb_hash.to_hex(), std::mem::take(buf)));
            built.push((xorb_hash, std::mem::take(group)));
        }

        let mut group: Vec<usize> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        for idx in 0..n {
            if placement[idx].is_some() {
                continue;
            }
            let serialized = serialize_single_chunk(slice(idx), CompressionType::Lz4)
                .map_err(|e| e.to_string())?;
            if !buf.is_empty() && buf.len() + serialized.len() > XORB_SOFT_LIMIT {
                seal(
                    &mut group,
                    &mut buf,
                    &chunk_hashes,
                    &sizes,
                    &mut placement,
                    &mut xorbs,
                    &mut built,
                );
            }
            buf.extend_from_slice(&serialized);
            group.push(idx);
        }
        if !group.is_empty() {
            seal(
                &mut group,
                &mut buf,
                &chunk_hashes,
                &sizes,
                &mut placement,
                &mut xorbs,
                &mut built,
            );
        }
    }

    let placement: Vec<Placement> = placement
        .into_iter()
        .map(|p| p.expect("every chunk resolved or packed"))
        .collect();

    // Coalesce consecutive chunks sharing a xorb + adjacent indices into
    // reconstruction terms; one verification entry per term.
    let mut entries: Vec<FileDataSequenceEntry> = Vec::new();
    let mut verifications: Vec<FileVerificationEntry> = Vec::new();
    let mut t = 0;
    while t < n {
        let xorb = placement[t].xorb_hash;
        let start_idx = placement[t].index_in_xorb;
        let mut end = t + 1;
        let mut expected_idx = start_idx + 1;
        while end < n
            && placement[end].xorb_hash == xorb
            && placement[end].index_in_xorb == expected_idx
        {
            expected_idx += 1;
            end += 1;
        }

        entries.push(FileDataSequenceEntry {
            cas_hash: xorb,
            cas_flags: 0,
            unpacked_segment_bytes: (t..end).map(|k| sizes[k] as u32).sum(),
            chunk_index_start: start_idx,
            chunk_index_end: placement[end - 1].index_in_xorb + 1,
        });
        verifications.push(FileVerificationEntry {
            range_hash: compute_verification_hash(&chunk_hashes[t..end]),
        });
        t = end;
    }

    // CAS info only for the xorbs built this run — existing ones are already
    // registered server-side.
    let cas_info_blocks: Vec<CASInfoBlock> = built
        .iter()
        .zip(&xorbs)
        .map(|((xorb_hash, group), (_, bytes))| {
            let mut byte_offset = 0u32;
            let cas_entries: Vec<CASChunkSequenceEntry> = group
                .iter()
                .map(|&k| {
                    let entry = CASChunkSequenceEntry {
                        chunk_hash: chunk_hashes[k],
                        chunk_byte_range_start: byte_offset,
                        unpacked_segment_bytes: sizes[k] as u32,
                    };
                    byte_offset += sizes[k] as u32;
                    entry
                })
                .collect();
            CASInfoBlock {
                header: CASChunkSequenceHeader {
                    cas_hash: *xorb_hash,
                    cas_flags: 0,
                    num_entries: cas_entries.len() as u32,
                    num_bytes_in_cas: byte_offset,
                    num_bytes_on_disk: bytes.len() as u32,
                },
                entries: cas_entries,
            }
        })
        .collect();

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
        chunk_count: n,
        deduped_chunk_count,
        xorbs,
        shard_bytes,
    })
}

/// One-shot plan without global dedup (kept for tests and non-dedup callers).
#[wasm_bindgen]
pub fn plan_upload(data: &[u8]) -> Result<UploadPlan, JsError> {
    build_upload_plan(data).map_err(|e| JsError::new(&e))
}

pub fn build_upload_plan(data: &[u8]) -> Result<UploadPlan, String> {
    if data.is_empty() {
        return Err("refusing to upload an empty file".to_string());
    }
    let chunk_infos = chunk_data(data);
    let ranges: Vec<(usize, usize)> =
        chunk_infos.iter().map(|ci| (ci.offset, ci.length)).collect();
    let chunk_hashes: Vec<MerkleHash> = ranges
        .iter()
        .map(|&(o, l)| compute_chunk_hash(&data[o..o + l]))
        .collect();
    let n = chunk_hashes.len();
    build_plan(data.to_vec(), ranges, chunk_hashes, vec![None; n])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_data() -> Vec<u8> {
        (0..512 * 1024u32).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn plan_covers_all_bytes_and_parses_back() {
        let data = test_data();
        let plan = build_upload_plan(&data).unwrap();

        assert_eq!(plan.file_hash.len(), 64);
        assert!(plan.chunk_count >= 1);
        assert_eq!(plan.deduped_chunk_count, 0);
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
        // (JsError is only constructible on wasm targets, so the session
        // constructor's identical guard isn't exercised here.)
        assert!(build_upload_plan(&[]).is_err());
    }

    #[test]
    fn session_without_dedup_matches_plan_upload() {
        let data = test_data();
        let direct = build_upload_plan(&data).unwrap();

        let mut session = UploadSession::new(data).unwrap();
        // Drain the probe batches without answering (all misses).
        let mut probes = 0;
        loop {
            let batch = session.next_query_batch(16);
            if batch.is_empty() {
                break;
            }
            probes += batch.len();
        }
        assert!(probes >= 1); // chunk 0 is always probed
        let plan = session.finish().unwrap();

        assert_eq!(plan.file_hash, direct.file_hash);
        assert_eq!(plan.shard_bytes, direct.shard_bytes);
        assert_eq!(plan.xorbs.len(), direct.xorbs.len());
        assert_eq!(plan.xorbs[0].0, direct.xorbs[0].0);
    }

    #[test]
    fn full_dedup_uploads_no_xorbs() {
        let data = test_data();
        // First upload registers the xorb; feed its CAS info back as a dedup
        // shard (HMAC key = the identity path is exercised with a real key).
        let first = build_upload_plan(&data).unwrap();
        let first_shard = Shard::from_bytes(&first.shard_bytes).unwrap();

        // Build a dedup-style response: same CAS blocks, HMAC'd chunk hashes.
        let key = [7u8; 32];
        let cas_info_blocks: Vec<CASInfoBlock> = first_shard
            .cas_info_blocks
            .iter()
            .map(|b| CASInfoBlock {
                header: b.header.clone(),
                entries: b
                    .entries
                    .iter()
                    .map(|e| CASChunkSequenceEntry {
                        chunk_hash: MerkleHash::from_bytes(hmac_chunk(&key, &e.chunk_hash)),
                        ..e.clone()
                    })
                    .collect(),
            })
            .collect();
        let dedup_shard = Shard {
            header: openxet_cas_types::shard::ShardHeader {
                tag: openxet_cas_types::shard::MDB_SHARD_HEADER_TAG,
                version: openxet_cas_types::shard::MDB_SHARD_HEADER_VERSION,
                footer_size: openxet_cas_types::shard::FOOTER_SIZE as u64,
            },
            file_info_blocks: vec![],
            cas_info_blocks,
            footer: Some(openxet_cas_types::shard::ShardFooter {
                version: openxet_cas_types::shard::MDB_SHARD_FOOTER_VERSION,
                file_info_offset: 0,
                cas_info_offset: 0,
                chunk_hash_hmac_key: key,
                shard_creation_timestamp: 0,
                shard_key_expiry: u64::MAX,
                footer_offset: 0,
            }),
        };
        let dedup_bytes = dedup_shard.to_bytes().unwrap();

        let mut session = UploadSession::new(data).unwrap();
        let batch = session.next_query_batch(16);
        assert!(!batch.is_empty(), "chunk 0 probed");
        assert_eq!(batch[0].len(), 64);
        session.apply_dedup_shard(&dedup_bytes).unwrap();
        assert!(
            session.next_query_batch(16).is_empty(),
            "everything resolved"
        );

        let plan = session.finish().unwrap();
        assert_eq!(plan.file_hash, first.file_hash);
        assert_eq!(plan.xorbs.len(), 0, "no new xorbs to upload");
        assert_eq!(plan.deduped_chunk_count, plan.chunk_count);

        // Shard still references the existing xorb for reconstruction, with
        // no CAS info (nothing new was built).
        let shard = Shard::from_bytes(&plan.shard_bytes).unwrap();
        assert_eq!(shard.cas_info_blocks.len(), 0);
        assert_eq!(
            shard.file_info_blocks[0].entries[0].cas_hash.to_hex(),
            first.xorbs[0].0
        );
    }
}
