//! Integration tests for the V2 reconstruction API
//! (`GET /v2/reconstructions/{file_id}`), which xet-core prefers over V1 and
//! probes with a 404/501 fallback.

mod helpers;

use std::io::Cursor;

use bytes::Bytes;

use openxet_cas_types::reconstruction::{
    QueryReconstructionResponse, QueryReconstructionResponseV2,
};
use xet_core_structures::merklehash::{MerkleHash, file_hash};
use xet_core_structures::metadata_shard::MDBShardFileHeader;
use xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks;
use xet_core_structures::metadata_shard::file_structs::{
    FileDataSequenceEntry, FileDataSequenceHeader, FileVerificationEntry, MDBFileInfo,
};
use xet_core_structures::metadata_shard::xorb_structs::XorbChunkSequenceHeader;
use xet_core_structures::xorb_object::{
    Chunk, CompressionScheme, RawXorbData, SerializedXorbObject, deserialize_chunks,
};
use xet_data::deduplication::Chunker;

use helpers::{
    TestServer, UploadArtifacts, build_upload_artifacts, generate_test_data, upload_artifacts,
};

/// Query the V2 reconstruction endpoint with an optional Range header.
async fn get_reconstruction_v2(
    server: &TestServer,
    file_hash: &str,
    range: Option<&str>,
) -> reqwest::Response {
    let token = server.read_token();
    let mut req = server.client.get(format!(
        "{}/v2/reconstructions/{file_hash}",
        server.base_url
    ));
    req = req.bearer_auth(&token);
    if let Some(range) = range {
        req = req.header("range", range);
    }
    req.send().await.unwrap()
}

/// Download a file the way a V2 client does: for each term, find the fetch
/// entry whose descriptor covers the term's chunk range, fetch that byte
/// range, decompress, and slice the term's chunks out of the decoded span.
async fn download_via_v2(server: &TestServer, file_hash: &str) -> Vec<u8> {
    let resp = get_reconstruction_v2(server, file_hash, None).await;
    assert_eq!(resp.status(), 200, "v2 reconstruction query failed");
    let recon: QueryReconstructionResponseV2 = resp.json().await.unwrap();

    let mut file_bytes = Vec::new();
    for term in &recon.terms {
        let entries = recon
            .xorbs
            .get(&term.hash)
            .unwrap_or_else(|| panic!("no v2 fetch entries for xorb {}", term.hash));

        let (fetch, desc) = entries
            .iter()
            .find_map(|f| {
                f.ranges
                    .iter()
                    .find(|d| d.chunks.contains_range(&term.range))
                    .map(|d| (f, d))
            })
            .unwrap_or_else(|| {
                panic!(
                    "no v2 descriptor covers term chunks [{}, {}) of xorb {}",
                    term.range.start, term.range.end, term.hash
                )
            });

        // V2 URLs are self-authenticating; xet-core sends no Authorization
        // header, only the descriptor's byte range.
        let resp = server
            .client
            .get(&fetch.url)
            .header(
                "range",
                format!("bytes={}-{}", desc.bytes.start, desc.bytes.end),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "v2 xorb range fetch failed");
        let part = resp.bytes().await.unwrap();

        // boundaries has a leading 0 followed by cumulative decoded chunk
        // ends, so local chunk i spans boundaries[i]..boundaries[i+1].
        let (bytes, boundaries) = deserialize_chunks(&mut Cursor::new(&part[..])).unwrap();
        let local_start = term.range.start - desc.chunks.start;
        let local_end = term.range.end - desc.chunks.start;
        file_bytes.extend_from_slice(
            &bytes[boundaries[local_start] as usize..boundaries[local_end] as usize],
        );
    }

    file_bytes[recon.offset_into_first_range as usize..].to_vec()
}

/// Full upload → V2 reconstruction → download roundtrip.
#[tokio::test]
async fn test_v2_roundtrip() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let downloaded = download_via_v2(&server, &artifacts.file_hash).await;
    assert_eq!(downloaded, data);
}

/// V1 and V2 responses agree on terms and offset; V2's per-xorb entries carry
/// exactly one descriptor each (single-range entries, S3-compatible), with the
/// same byte spans V1 advertises.
#[tokio::test]
async fn test_v2_matches_v1() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let v1: QueryReconstructionResponse = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let v2: QueryReconstructionResponseV2 =
        get_reconstruction_v2(&server, &artifacts.file_hash, None)
            .await
            .json()
            .await
            .unwrap();

    assert_eq!(v1.offset_into_first_range, v2.offset_into_first_range);
    assert_eq!(v1.terms.len(), v2.terms.len());
    for (t1, t2) in v1.terms.iter().zip(&v2.terms) {
        assert_eq!(t1.hash, t2.hash);
        assert_eq!(t1.unpacked_length, t2.unpacked_length);
        assert_eq!(t1.range, t2.range);
    }

    assert_eq!(v1.fetch_info.len(), v2.xorbs.len());
    for (hash, infos) in &v1.fetch_info {
        let entries = v2.xorbs.get(hash).expect("v2 missing xorb from v1");
        assert_eq!(infos.len(), entries.len());
        for (info, entry) in infos.iter().zip(entries) {
            assert_eq!(entry.ranges.len(), 1, "v2 entries must be single-range");
            let desc = &entry.ranges[0];
            assert_eq!(desc.chunks, info.range);
            assert_eq!(desc.bytes, info.url_range);
        }
    }
}

/// Range request returns only terms overlapping the requested byte range.
#[tokio::test]
async fn test_v2_range_partial() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let resp = get_reconstruction_v2(&server, &artifacts.file_hash, None).await;
    assert_eq!(resp.status(), 200);
    let full_recon: QueryReconstructionResponseV2 = resp.json().await.unwrap();
    let full_total: u64 = full_recon.terms.iter().map(|t| t.unpacked_length).sum();
    assert_eq!(full_total, data.len() as u64);

    let range_start = 1000u64;
    let range_end = 2000u64;
    let resp = get_reconstruction_v2(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes={range_start}-{range_end}")),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let range_recon: QueryReconstructionResponseV2 = resp.json().await.unwrap();

    let range_total: u64 = range_recon.terms.iter().map(|t| t.unpacked_length).sum();
    let effective_end = range_total - range_recon.offset_into_first_range;
    assert!(effective_end > range_end - range_start);
    assert!(range_recon.terms.len() <= full_recon.terms.len());
}

/// Range request past end of file returns 416.
#[tokio::test]
async fn test_v2_range_past_end_416() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let past_end = data.len() as u64;
    let resp = get_reconstruction_v2(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes={past_end}-{}", past_end + 100)),
    )
    .await;
    assert_eq!(resp.status(), 416);
}

/// The V2 endpoint requires a read token like V1.
#[tokio::test]
async fn test_v2_requires_auth() {
    let server = TestServer::start().await;
    let dummy_hash = "0".repeat(64);

    let resp = server
        .client
        .get(format!(
            "{}/v2/reconstructions/{dummy_hash}",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

/// Build artifacts for a file whose two terms reference the SAME xorb with
/// disjoint chunk ranges [0, 2) and [4, 6) — the shape produced when file
/// content deduplicates against another part of the same xorb.
fn build_dedup_artifacts(data: &[u8]) -> (UploadArtifacts, Vec<u8>) {
    let chunks: Vec<Chunk> = Chunker::default().next_block(data, true);
    assert!(
        chunks.len() >= 6,
        "test data must produce at least 6 chunks, got {}",
        chunks.len()
    );
    let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|c| c.hash).collect();

    let raw = RawXorbData::from_chunks(&chunks, vec![0]);
    let xorb_hash = raw.hash();
    let mut xorb_info = raw.xorb_info.clone();
    let serialized =
        SerializedXorbObject::from_xorb_with_compression(raw, CompressionScheme::LZ4, true)
            .unwrap();
    xorb_info.metadata.num_bytes_on_disk = serialized.serialized_data.len() as u32;

    // The file consists of chunks 0-1 followed by chunks 4-5 of the xorb.
    let term_ranges = [(0usize, 2usize), (4usize, 6usize)];
    let file_chunks: Vec<(MerkleHash, u64)> = term_ranges
        .iter()
        .flat_map(|&(s, e)| chunks[s..e].iter().map(|c| (c.hash, c.data.len() as u64)))
        .collect();
    let file_hash_hex = file_hash(&file_chunks).hex();

    let expected_bytes: Vec<u8> = term_ranges
        .iter()
        .flat_map(|&(s, e)| chunks[s..e].iter().flat_map(|c| c.data.iter().copied()))
        .collect();

    let segments = term_ranges
        .iter()
        .map(|&(s, e)| {
            let unpacked: u32 = chunks[s..e].iter().map(|c| c.data.len() as u32).sum();
            FileDataSequenceEntry::new(xorb_hash, unpacked, s as u32, e as u32)
        })
        .collect();
    let verification = term_ranges
        .iter()
        .map(|&(s, e)| FileVerificationEntry::new(range_hash_from_chunks(&chunk_hashes[s..e])))
        .collect();

    let file_info = MDBFileInfo {
        metadata: FileDataSequenceHeader::new(
            MerkleHash::from_hex(&file_hash_hex).unwrap(),
            term_ranges.len(),
            true,
            false,
        ),
        segments,
        verification,
        metadata_ext: None,
    };

    let mut shard_bytes = Vec::new();
    let header = MDBShardFileHeader {
        footer_size: 0,
        ..Default::default()
    };
    header.serialize(&mut shard_bytes).unwrap();
    file_info.serialize(&mut shard_bytes).unwrap();
    FileDataSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .unwrap();
    xorb_info.serialize(&mut shard_bytes).unwrap();
    XorbChunkSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .unwrap();

    let artifacts = UploadArtifacts {
        file_hash: file_hash_hex,
        xorb_entries: vec![(xorb_hash.hex(), Bytes::from(serialized.serialized_data))],
        shard_bytes: Bytes::from(shard_bytes),
        chunk_hashes,
    };

    (artifacts, expected_bytes)
}

/// A file referencing the same xorb from two disjoint terms gets fetch
/// coverage for BOTH terms — as two single-range entries in V2, and as two
/// fetch_info entries in V1.
#[tokio::test]
async fn test_v2_dedup_same_xorb_two_terms() {
    let server = TestServer::start().await;
    let data = generate_test_data(1024 * 1024);
    let (artifacts, expected_bytes) = build_dedup_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let v2: QueryReconstructionResponseV2 =
        get_reconstruction_v2(&server, &artifacts.file_hash, None)
            .await
            .json()
            .await
            .unwrap();

    assert_eq!(v2.terms.len(), 2);
    let xorb_hash = &v2.terms[0].hash;
    let entries = v2.xorbs.get(xorb_hash).expect("missing xorb fetch entries");
    assert_eq!(
        entries.len(),
        2,
        "disjoint terms on one xorb need two fetch entries"
    );

    let downloaded = download_via_v2(&server, &artifacts.file_hash).await;
    assert_eq!(downloaded, expected_bytes);

    // Regression check for V1: previously only the first term's range per
    // xorb was covered by fetch_info.
    let token = server.read_token();
    let v1: QueryReconstructionResponse = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let infos = v1.fetch_info.get(xorb_hash).expect("missing v1 fetch_info");
    for term in &v1.terms {
        assert!(
            infos.iter().any(|f| f.range.contains_range(&term.range)),
            "v1 fetch_info must cover term chunks [{}, {})",
            term.range.start,
            term.range.end
        );
    }
}
