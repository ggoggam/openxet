//! openxet-wasm — the Xet client-side upload pipeline, compiled to WebAssembly
//! for the web UI.
//!
//! The Xet wire protocol requires the *client* to chunk, hash, and pack files
//! (`POST /v1/xorbs`, `POST /v1/shards`). This crate builds on HuggingFace's
//! own `xet-core-structures` / `xet-data` crates — the same code real
//! `hf_xet` clients run — so browser uploads are wire-compatible by
//! construction.
//!
//! Upload is a two-phase session (following the Xet put protocol):
//!
//! 1. `UploadSession::new(data)` chunks + hashes. JS then drives the global
//!    dedup loop: `next_query_batch()` → `GET /v1/chunks/default-merkledb/{h}`
//!    → on hit, `apply_dedup_shard(bytes)` resolves chunks to existing xorbs.
//! 2. `finish()` packs only the still-unresolved chunks into new xorbs and
//!    builds the shard; JS performs the HTTP uploads.

use std::collections::HashMap;
use std::io::Cursor;

use wasm_bindgen::prelude::*;

use xet_core_structures::merklehash::{MerkleHash, file_hash};
use xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks;
use xet_core_structures::metadata_shard::constants::hash_is_global_dedup_eligible;
use xet_core_structures::metadata_shard::file_structs::{
    FileDataSequenceEntry, FileDataSequenceHeader, FileVerificationEntry, MDBFileInfo,
};
use xet_core_structures::metadata_shard::xorb_structs::{MDBXorbInfo, XorbChunkSequenceHeader};
use xet_core_structures::metadata_shard::{MDBShardFileHeader, MDBShardInfo};
use xet_core_structures::xorb_object::constants::{MAX_XORB_BYTES, MAX_XORB_CHUNKS};
use xet_core_structures::xorb_object::{
    Chunk, CompressionScheme, RawXorbData, SerializedXorbObject, deserialize_chunks,
};
use xet_data::deduplication::Chunker;

/// Where a file chunk lives, once resolved.
#[derive(Clone)]
struct Placement {
    xorb_hash: MerkleHash,
    index_in_xorb: u32,
}

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
    chunks: Vec<Chunk>,
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
        let chunks = Chunker::default().next_block(&data, true);
        let n = chunks.len();
        Ok(UploadSession {
            chunks,
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
    /// resolved run (the continuation of known data), and chunks that pass
    /// xet-core's global-dedup eligibility predicate (hash % 1024 == 0, i.e.
    /// roughly one probe per 64 MiB at the 64 KiB target chunk size). Each
    /// chunk is handed out at most once.
    pub fn next_query_batch(&mut self, max: usize) -> Vec<String> {
        let mut out = Vec::new();
        for i in 0..self.chunks.len() {
            if out.len() >= max {
                break;
            }
            if self.probed[i] || self.placement[i].is_some() {
                continue;
            }
            let after_resolved = i > 0 && self.placement[i - 1].is_some();
            let eligible = hash_is_global_dedup_eligible(&self.chunks[i].hash);
            if i == 0 || after_resolved || eligible {
                self.probed[i] = true;
                out.push(self.chunks[i].hash.hex());
            }
        }
        out
    }

    /// Apply a dedup shard response: HMAC-match our chunk hashes against its
    /// (keyed) CAS entries and resolve every covered chunk to its existing
    /// xorb.
    pub fn apply_dedup_shard(&mut self, bytes: &[u8]) -> Result<(), JsError> {
        let mut reader = Cursor::new(bytes);
        let shard_info = MDBShardInfo::load_from_reader(&mut reader)
            .map_err(|e| JsError::new(&format!("parsing dedup shard: {e}")))?;
        let Some(key) = shard_info.chunk_hmac_key() else {
            return Err(JsError::new("dedup shard missing hmac key"));
        };

        let xorb_blocks = shard_info
            .read_all_xorb_blocks_full(&mut reader)
            .map_err(|e| JsError::new(&format!("reading dedup shard xorb blocks: {e}")))?;

        // keyed chunk hash -> (xorb_hash, index_in_xorb)
        let mut by_keyed_hash = HashMap::new();
        for block in &xorb_blocks {
            for (idx, entry) in block.chunks.iter().enumerate() {
                by_keyed_hash.insert(
                    entry.chunk_hash,
                    Placement {
                        xorb_hash: block.metadata.xorb_hash,
                        index_in_xorb: idx as u32,
                    },
                );
            }
        }

        for j in 0..self.chunks.len() {
            if self.placement[j].is_some() {
                continue;
            }
            if let Some(p) = by_keyed_hash.get(&self.chunks[j].hash.hmac(key)) {
                self.placement[j] = Some(p.clone());
            }
        }
        Ok(())
    }

    /// Pack the still-unresolved chunks into new xorbs and build the shard.
    /// The session is consumed; use the returned plan for the HTTP uploads.
    pub fn finish(&mut self) -> Result<UploadPlan, JsError> {
        build_plan(
            std::mem::take(&mut self.chunks),
            std::mem::take(&mut self.placement),
        )
        .map_err(|e| JsError::new(&e))
    }
}

/// Serialize one group of chunks into a xorb: LZ4-compressed chunk frames
/// followed by the metadata footer real xet-core clients also write.
fn seal_xorb(chunks: &[Chunk]) -> Result<(MerkleHash, MDBXorbInfo, Vec<u8>), String> {
    let raw = RawXorbData::from_chunks(chunks, vec![0]);
    let xorb_hash = raw.hash();
    let mut xorb_info = raw.xorb_info.clone();

    let serialized = SerializedXorbObject::from_xorb_with_compression(
        raw,
        CompressionScheme::LZ4,
        /* serialize_footer= */ true,
    )
    .map_err(|e| format!("serializing xorb: {e}"))?;

    xorb_info.metadata.num_bytes_on_disk = serialized.serialized_data.len() as u32;
    Ok((xorb_hash, xorb_info, serialized.serialized_data))
}

/// Pack new chunks into xorbs, coalesce placements into reconstruction terms,
/// and build the upload shard (the final steps of the Xet put protocol).
fn build_plan(
    chunks: Vec<Chunk>,
    mut placement: Vec<Option<Placement>>,
) -> Result<UploadPlan, String> {
    let n = chunks.len();

    let hashes_and_sizes: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|c| (c.hash, c.data.len() as u64))
        .collect();
    let file_hash = file_hash(&hashes_and_sizes);
    let deduped_chunk_count = placement.iter().filter(|p| p.is_some()).count();

    // Pack unresolved chunks into new xorbs, cutting at xet-core's raw-byte
    // and chunk-count thresholds.
    let mut xorbs: Vec<(String, Vec<u8>)> = Vec::new();
    let mut new_xorb_infos: Vec<MDBXorbInfo> = Vec::new();
    {
        let mut group: Vec<usize> = Vec::new();
        let mut group_bytes = 0usize;

        let seal = |group: &mut Vec<usize>,
                    group_bytes: &mut usize,
                    placement: &mut Vec<Option<Placement>>,
                    xorbs: &mut Vec<(String, Vec<u8>)>,
                    new_xorb_infos: &mut Vec<MDBXorbInfo>|
         -> Result<(), String> {
            let group_chunks: Vec<Chunk> = group.iter().map(|&k| chunks[k].clone()).collect();
            let (xorb_hash, xorb_info, bytes) = seal_xorb(&group_chunks)?;
            for (local_idx, &global_idx) in group.iter().enumerate() {
                placement[global_idx] = Some(Placement {
                    xorb_hash,
                    index_in_xorb: local_idx as u32,
                });
            }
            xorbs.push((xorb_hash.hex(), bytes));
            new_xorb_infos.push(xorb_info);
            group.clear();
            *group_bytes = 0;
            Ok(())
        };

        for idx in 0..n {
            if placement[idx].is_some() {
                continue;
            }
            let len = chunks[idx].data.len();
            if !group.is_empty()
                && (group_bytes + len > *MAX_XORB_BYTES || group.len() >= *MAX_XORB_CHUNKS)
            {
                seal(
                    &mut group,
                    &mut group_bytes,
                    &mut placement,
                    &mut xorbs,
                    &mut new_xorb_infos,
                )?;
            }
            group.push(idx);
            group_bytes += len;
        }
        if !group.is_empty() {
            seal(
                &mut group,
                &mut group_bytes,
                &mut placement,
                &mut xorbs,
                &mut new_xorb_infos,
            )?;
        }
    }

    let placement: Vec<Placement> = placement
        .into_iter()
        .map(|p| p.expect("every chunk resolved or packed"))
        .collect();

    // Coalesce consecutive chunks sharing a xorb + adjacent indices into
    // reconstruction terms; one verification entry per term.
    let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|c| c.hash).collect();
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

        let unpacked: u32 = (t..end).map(|k| chunks[k].data.len() as u32).sum();
        entries.push(FileDataSequenceEntry::new(
            xorb,
            unpacked,
            start_idx,
            placement[end - 1].index_in_xorb + 1,
        ));
        verifications.push(FileVerificationEntry::new(range_hash_from_chunks(
            &chunk_hashes[t..end],
        )));
        t = end;
    }

    // Upload shard format: header with footer_size=0, file info section,
    // bookend, xorb (CAS) info section, bookend — no lookup tables or footer
    // (xet-core strips those before uploading too).
    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(
            file_hash,
            entries.len(),
            /* contains_verification= */ true,
            /* contains_metadata_ext= */ false,
        ),
        segments: entries,
        verification: verifications,
        metadata_ext: None,
    };

    let mut shard_bytes = Vec::new();
    let header = MDBShardFileHeader {
        footer_size: 0,
        ..Default::default()
    };
    header
        .serialize(&mut shard_bytes)
        .map_err(|e| e.to_string())?;
    file_info
        .serialize(&mut shard_bytes)
        .map_err(|e| e.to_string())?;
    FileDataSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .map_err(|e| e.to_string())?;
    // CAS info only for the xorbs built this run — existing ones are already
    // registered server-side.
    for xorb_info in &new_xorb_infos {
        xorb_info
            .serialize(&mut shard_bytes)
            .map_err(|e| e.to_string())?;
    }
    XorbChunkSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .map_err(|e| e.to_string())?;

    Ok(UploadPlan {
        file_hash: file_hash.hex(),
        chunk_count: n,
        deduped_chunk_count,
        xorbs,
        shard_bytes,
    })
}

/// Decode a fetched xorb byte range (a sequence of chunk frames, as returned
/// by a reconstruction `fetch_info` URL + `url_range`) into raw file bytes.
/// `start`/`end` are chunk indices *relative to the fetched slice* — i.e.
/// `term.range` shifted by the fetch info's `range.start`.
#[wasm_bindgen]
pub fn decode_chunks(data: &[u8], start: usize, end: usize) -> Result<Vec<u8>, JsError> {
    decode_chunks_impl(data, start, end).map_err(|e| JsError::new(&e))
}

fn decode_chunks_impl(data: &[u8], start: usize, end: usize) -> Result<Vec<u8>, String> {
    let (bytes, boundaries) =
        deserialize_chunks(&mut Cursor::new(data)).map_err(|e| format!("decoding chunks: {e}"))?;

    // `boundaries` holds cumulative uncompressed offsets starting with 0.
    if start > end || end >= boundaries.len() {
        return Err(format!(
            "chunk range [{start},{end}) out of bounds for {} chunks",
            boundaries.len().saturating_sub(1)
        ));
    }
    Ok(bytes[boundaries[start] as usize..boundaries[end] as usize].to_vec())
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
    let chunks = Chunker::default().next_block(data, true);
    let n = chunks.len();
    build_plan(chunks, vec![None; n])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use xet_core_structures::metadata_shard::shard_in_memory::MDBInMemoryShard;
    use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;

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

        // The xorb round-trips to the original bytes via its metadata footer
        let mut cursor = Cursor::new(&plan.xorbs[0].1[..]);
        let xorb = xet_core_structures::xorb_object::XorbObject::deserialize(&mut cursor).unwrap();
        assert_eq!(xorb.info.num_chunks as usize, plan.chunk_count);
        let rebuilt = xorb.get_all_bytes(&mut cursor).unwrap();
        assert_eq!(rebuilt, data);

        // decode_chunks handles a fetched chunk-range slice (frames only, no
        // footer), the shape reconstruction fetch_info URLs return.
        let (start, end) = xorb.get_byte_offset(0, xorb.info.num_chunks).unwrap();
        let slice = &plan.xorbs[0].1[start as usize..end as usize];
        let decoded = decode_chunks_impl(slice, 0, plan.chunk_count).unwrap();
        assert_eq!(decoded, data);

        // The shard parses and references the file + xorb consistently
        let shard =
            MDBMinimalShard::from_reader(&mut Cursor::new(&plan.shard_bytes[..]), true, true)
                .unwrap();
        assert_eq!(shard.num_files(), 1);
        let fv = shard.file(0).unwrap();
        assert_eq!(fv.file_hash().hex(), plan.file_hash);
        assert_eq!(fv.num_entries(), 1);
        assert_eq!(fv.entry(0).xorb_hash.hex(), plan.xorbs[0].0);
        assert_eq!(fv.entry(0).unpacked_segment_bytes as usize, data.len());
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
        // First upload registers the xorb; feed its CAS info back as a keyed
        // dedup shard, exactly like the server builds one.
        let first = build_upload_plan(&data).unwrap();
        let first_shard =
            MDBMinimalShard::from_reader(&mut Cursor::new(&first.shard_bytes[..]), true, true)
                .unwrap();

        let mut mem_shard = MDBInMemoryShard::default();
        for i in 0..first_shard.num_xorb() {
            let info: MDBXorbInfo = first_shard.xorb(i).unwrap().into();
            mem_shard.add_xorb_block(info).unwrap();
        }
        let mut unkeyed = Vec::new();
        let shard_info = MDBShardInfo::serialize_from(&mut unkeyed, &mem_shard, None).unwrap();

        let key = MerkleHash::from([7u8; 32]);
        let mut dedup_bytes = Vec::new();
        shard_info
            .export_as_keyed_shard(
                &mut Cursor::new(&unkeyed[..]),
                &mut dedup_bytes,
                key,
                Duration::from_secs(3600),
                false,
                true,
                true,
            )
            .unwrap();

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
        let shard =
            MDBMinimalShard::from_reader(&mut Cursor::new(&plan.shard_bytes[..]), true, true)
                .unwrap();
        assert_eq!(shard.num_xorb(), 0);
        assert_eq!(
            shard.file(0).unwrap().entry(0).xorb_hash.hex(),
            first.xorbs[0].0
        );
    }
}
