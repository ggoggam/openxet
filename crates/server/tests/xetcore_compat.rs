//! Integration tests simulating xet-core's RemoteClient behavior.
//!
//! These tests verify that our server is wire-compatible with HuggingFace's
//! xet-core client by replicating the exact HTTP request patterns, URL paths,
//! body formats, and response parsing that xet-core uses.

mod helpers;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use openxet_cas_types::chunk::{ChunkHeader, decompress_chunk};
use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use openxet_cas_types::shard::Shard;
use openxet_hashing::MerkleHash;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

type HmacSha256 = Hmac<Sha256>;

// ─── xet-core xorb format helpers ────────────────────────────────────────────

/// Simulate what xet-core does: append a CasObjectInfoV1 footer to a serialized xorb.
///
/// The footer contains chunk hashes and boundary info for efficient random access.
/// Format:
///   "XETBLOB" (7B) + version(1B) + xorb_hash(32B)
///   "XBLBHSH" (7B) + version(1B) + num_chunks(4B) + chunk_hashes(32B each)
///   "XBLBBND" (7B) + version(1B) + num_chunks(4B) + compressed_offsets(4B each) + uncompressed_offsets(4B each)
///   trailer: num_chunks(4B) + hash_offset(4B) + boundary_offset(4B) + reserved(16B)
///   info_length(4B)
fn append_cas_object_footer(
    xorb_data: &[u8],
    xorb_hash: &MerkleHash,
    chunk_hashes: &[MerkleHash],
    compressed_offsets: &[u32],
    uncompressed_offsets: &[u32],
) -> Vec<u8> {
    let num_chunks = chunk_hashes.len() as u32;
    let mut footer = Vec::new();

    // Header section
    footer.extend_from_slice(b"XETBLOB");
    footer.push(1); // version
    footer.extend_from_slice(xorb_hash.as_bytes());

    // Hash section
    let hash_section_start = footer.len();
    footer.extend_from_slice(b"XBLBHSH");
    footer.push(0); // version
    footer.extend_from_slice(&num_chunks.to_le_bytes());
    for h in chunk_hashes {
        footer.extend_from_slice(h.as_bytes());
    }

    // Boundary section
    let boundary_section_start = footer.len();
    footer.extend_from_slice(b"XBLBBND");
    footer.push(1); // version
    footer.extend_from_slice(&num_chunks.to_le_bytes());
    for off in compressed_offsets {
        footer.extend_from_slice(&off.to_le_bytes());
    }
    for off in uncompressed_offsets {
        footer.extend_from_slice(&off.to_le_bytes());
    }

    // Trailer
    let footer_end = footer.len() + 4 + 4 + 4 + 16;
    footer.extend_from_slice(&num_chunks.to_le_bytes());
    footer.extend_from_slice(&((footer_end - hash_section_start) as u32).to_le_bytes());
    footer.extend_from_slice(&((footer_end - boundary_section_start) as u32).to_le_bytes());
    footer.extend_from_slice(&[0u8; 16]); // reserved

    let info_length = footer.len() as u32;
    footer.extend_from_slice(&info_length.to_le_bytes());

    let mut result = xorb_data.to_vec();
    result.extend_from_slice(&footer);
    result
}

/// Build xorb data with CasObjectInfoV1 footer, mimicking xet-core's SerializedCasObject.
fn build_xorb_with_footer(
    artifacts: &helpers::UploadArtifacts,
    xorb_index: usize,
) -> (String, Vec<u8>) {
    let (hash, raw_data) = &artifacts.xorb_entries[xorb_index];

    // Walk chunk headers to gather offsets
    let mut chunk_hashes = Vec::new();
    let mut compressed_offsets = Vec::new();
    let mut uncompressed_offsets = Vec::new();
    let mut pos = 0usize;
    let mut uncompressed_pos = 0u32;

    while pos + ChunkHeader::SIZE <= raw_data.len() {
        let header_bytes: [u8; 8] = raw_data[pos..pos + 8].try_into().unwrap();
        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };

        compressed_offsets.push(pos as u32);
        uncompressed_offsets.push(uncompressed_pos);

        let compressed_start = pos + ChunkHeader::SIZE;
        let compressed_end = compressed_start + header.compressed_size as usize;
        if compressed_end > raw_data.len() {
            break;
        }

        let compressed = &raw_data[compressed_start..compressed_end];
        let decompressed = decompress_chunk(
            compressed,
            header.compression_type,
            header.uncompressed_size as usize,
        )
        .unwrap();

        chunk_hashes.push(openxet_hashing::compute_chunk_hash(&decompressed));
        uncompressed_pos += decompressed.len() as u32;
        pos = compressed_end;
    }

    let xorb_hash = MerkleHash::from_hex(hash).unwrap();
    let with_footer = append_cas_object_footer(
        raw_data,
        &xorb_hash,
        &chunk_hashes,
        &compressed_offsets,
        &uncompressed_offsets,
    );

    (hash.clone(), with_footer)
}

// ─── Reconstruction helpers (simulating xet-core client download) ────────────

/// Simulate xet-core's download flow: given a QueryReconstructionResponse,
/// fetch xorb ranges, parse chunk headers, decompress, and reconstruct the file.
async fn reconstruct_file_from_response(
    client: &reqwest::Client,
    recon: &QueryReconstructionResponse,
    token: &str,
) -> Vec<u8> {
    let mut result = Vec::new();

    for (term_idx, term) in recon.terms.iter().enumerate() {
        let fetch_infos = recon
            .fetch_info
            .get(&term.hash)
            .unwrap_or_else(|| panic!("missing fetch_info for xorb {}", term.hash));

        // Find the fetch info that covers this term's chunk range
        let fi = fetch_infos
            .iter()
            .find(|fi| fi.range.start <= term.range.start && fi.range.end >= term.range.end)
            .unwrap_or_else(|| {
                panic!(
                    "no fetch_info covers range {:?} for xorb {}",
                    term.range, term.hash
                )
            });

        // xet-core fetches the xorb byte range using the url and url_range
        let resp = client
            .get(&fi.url)
            .bearer_auth(token)
            .header(
                "Range",
                format!("bytes={}-{}", fi.url_range.start, fi.url_range.end),
            )
            .send()
            .await
            .unwrap();

        assert!(
            resp.status().is_success() || resp.status().as_u16() == 206,
            "fetch xorb range failed: {} for url {}",
            resp.status(),
            fi.url
        );

        let xorb_range_data = resp.bytes().await.unwrap();

        // Parse chunk headers from the fetched range and decompress
        let mut chunks = Vec::new();
        let mut pos = 0usize;

        while pos + ChunkHeader::SIZE <= xorb_range_data.len() {
            let header_bytes: [u8; 8] = xorb_range_data[pos..pos + 8].try_into().unwrap();
            let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
                break;
            };

            let compressed_start = pos + ChunkHeader::SIZE;
            let compressed_end = compressed_start + header.compressed_size as usize;
            if compressed_end > xorb_range_data.len() {
                break;
            }

            let compressed = &xorb_range_data[compressed_start..compressed_end];
            let decompressed = decompress_chunk(
                compressed,
                header.compression_type,
                header.uncompressed_size as usize,
            )
            .unwrap();

            chunks.push(decompressed);
            pos = compressed_end;
        }

        // The fetch_info range may cover more chunks than this term needs.
        // Select only the chunks for this term's sub-range within the fetch_info range.
        let local_start = term.range.start - fi.range.start;
        let local_end = term.range.end - fi.range.start;

        for chunk in &chunks[local_start..local_end.min(chunks.len())] {
            if term_idx == 0 && recon.offset_into_first_range > 0 {
                let skip = recon.offset_into_first_range as usize;
                if skip < chunk.len() {
                    result.extend_from_slice(&chunk[skip..]);
                }
            } else {
                result.extend_from_slice(chunk);
            }
        }
    }

    result
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Test 1: xet-core posts shards to /shards (no /v1/ prefix).
/// This is a critical compatibility requirement.
#[tokio::test]
async fn test_xetcore_shard_path_without_v1_prefix() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Upload xorbs via standard path
    for (hash, xorb_data) in &artifacts.xorb_entries {
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(xorb_data.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Upload shard via xet-core's path: /shards (NOT /v1/shards)
    let resp = server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "xet-core shard path /shards should be accepted"
    );

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["result"], 1, "shard should be newly inserted");

    // Verify file is accessible via reconstruction
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Test 2: xet-core sends xorbs WITH CasObjectInfoV1 footer appended.
/// Our server must accept and store them correctly.
#[tokio::test]
async fn test_xetcore_xorb_with_cas_object_footer() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Upload xorbs WITH footer (as xet-core does)
    for i in 0..artifacts.xorb_entries.len() {
        let (hash, xorb_with_footer) = build_xorb_with_footer(&artifacts, i);

        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(xorb_with_footer)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "xorb with CasObjectInfoV1 footer should be accepted"
        );

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["was_inserted"], true);
    }

    // Upload shard
    let resp = server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Verify reconstruction works with footer-bearing xorbs
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();
    assert!(!recon.terms.is_empty());

    // Verify we can reconstruct the full file from the response
    let reconstructed =
        reconstruct_file_from_response(&server.client, &recon, &server.read_token()).await;
    assert_eq!(
        reconstructed.len(),
        data.len(),
        "reconstructed file size mismatch"
    );
    assert_eq!(reconstructed, data, "reconstructed file content mismatch");
}

/// Test 3: Full xet-core upload + download round-trip with large data.
/// Generates ~2 MiB of pseudo-random data to ensure multiple xorb groups
/// and multiple CDC chunks, then reconstructs via the client download flow.
#[tokio::test]
async fn test_xetcore_full_roundtrip_large_file() {
    let server = TestServer::start().await;
    let data = generate_test_data(2 * 1024 * 1024); // 2 MiB
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Upload xorbs (with footer, as xet-core does)
    for i in 0..artifacts.xorb_entries.len() {
        let (hash, xorb_with_footer) = build_xorb_with_footer(&artifacts, i);

        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(xorb_with_footer)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Upload shard via /shards (xet-core path)
    let resp = server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Download: query reconstruction (as xet-core does)
    let read_token = server.read_token();
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&read_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();

    // Verify response structure matches what xet-core expects
    assert_eq!(recon.offset_into_first_range, 0);
    assert!(!recon.terms.is_empty());
    for term in &recon.terms {
        assert!(
            recon.fetch_info.contains_key(&term.hash),
            "xet-core expects fetch_info for every term hash"
        );
        let fi = &recon.fetch_info[&term.hash];
        assert!(!fi.is_empty());
        for info in fi {
            assert!(!info.url.is_empty(), "xet-core expects a URL in fetch_info");
        }
    }

    // Reconstruct (simulating xet-core client)
    let reconstructed = reconstruct_file_from_response(&server.client, &recon, &read_token).await;
    assert_eq!(reconstructed.len(), data.len());
    assert_eq!(reconstructed, data);
}

/// Test 4: xet-core dedup flow — query chunk, verify HMAC, find matches.
#[tokio::test]
async fn test_xetcore_dedup_flow() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let read_token = server.read_token();

    // Query dedup for a known chunk (as xet-core does)
    let chunk_hash = &artifacts.chunk_hashes[0];
    let chunk_hash_hex = chunk_hash.to_hex();

    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{chunk_hash_hex}",
            server.base_url
        ))
        .bearer_auth(&read_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let shard_bytes = resp.bytes().await.unwrap();
    let shard = Shard::from_bytes(&shard_bytes).expect("dedup response should be valid shard");

    // xet-core expects: footer with HMAC key
    let footer = shard.footer.as_ref().expect("dedup shard must have footer");
    assert!(footer.has_hmac_key(), "footer must contain HMAC key");
    assert!(
        footer.shard_key_expiry > footer.shard_creation_timestamp,
        "key expiry must be in the future"
    );

    // xet-core expects: CAS info blocks with HMAC-protected chunk hashes
    assert!(
        !shard.cas_info_blocks.is_empty(),
        "dedup shard must have CAS info blocks"
    );

    // xet-core verifies its own chunk hash by HMAC-ing it with the footer key
    let mut mac =
        HmacSha256::new_from_slice(&footer.chunk_hash_hmac_key).expect("HMAC key length valid");
    mac.update(chunk_hash.as_bytes());
    let hmac_result = mac.finalize().into_bytes();
    let expected_hmac = MerkleHash::from_bytes(hmac_result.into());

    // Find the HMAC'd hash in the CAS info entries
    let mut found = false;
    for block in &shard.cas_info_blocks {
        for entry in &block.entries {
            if entry.chunk_hash == expected_hmac {
                found = true;
            }
        }
    }
    assert!(
        found,
        "HMAC of queried chunk hash should appear in dedup response"
    );
}

/// Test 5: xet-core reconstruction with Range header for partial download.
#[tokio::test]
async fn test_xetcore_range_reconstruction() {
    let server = TestServer::start().await;
    let data = generate_test_data(512 * 1024); // 512 KiB
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let read_token = server.read_token();

    // Request a byte range (as xet-core does with FileRange)
    let range_start = 1000u64;
    let range_end = 50_000u64;

    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&read_token)
        .header("Range", format!("bytes={range_start}-{range_end}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();

    // xet-core uses offset_into_first_range to skip into the first term
    // The total unpacked minus the offset should cover our requested range
    let total_unpacked: u64 = recon.terms.iter().map(|t| t.unpacked_length).sum();
    let effective_bytes = total_unpacked - recon.offset_into_first_range;
    let requested_bytes = range_end - range_start + 1;
    assert!(
        effective_bytes >= requested_bytes,
        "effective bytes ({effective_bytes}) must cover requested range ({requested_bytes})"
    );
}

/// Test 6: xet-core response format validation.
/// Verifies JSON field names and types match xet-core's expected deserialization.
#[tokio::test]
async fn test_xetcore_response_format_compat() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Upload xorb — verify response matches UploadXorbResponse
    let (hash, xorb_data) = &artifacts.xorb_entries[0];
    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
        .bearer_auth(&token)
        .body(xorb_data.clone())
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("was_inserted").is_some(),
        "xorb response must have 'was_inserted' field"
    );
    assert!(
        body["was_inserted"].is_boolean(),
        "'was_inserted' must be boolean"
    );

    // Upload remaining xorbs + shard
    for (h, d) in artifacts.xorb_entries.iter().skip(1) {
        server
            .client
            .post(format!("{}/v1/xorbs/default/{h}", server.base_url))
            .bearer_auth(&token)
            .body(d.clone())
            .send()
            .await
            .unwrap();
    }

    let resp = server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("result").is_some(),
        "shard response must have 'result' field"
    );
    assert!(
        body["result"].is_number(),
        "'result' must be a number (0 or 1)"
    );
    let result = body["result"].as_u64().unwrap();
    assert!(
        result == 0 || result == 1,
        "'result' must be 0 (exists) or 1 (sync performed)"
    );

    // Reconstruction response — verify all expected fields
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();

    assert!(
        body.get("offset_into_first_range").is_some(),
        "must have 'offset_into_first_range'"
    );
    assert!(
        body["offset_into_first_range"].is_number(),
        "'offset_into_first_range' must be number"
    );

    assert!(body.get("terms").is_some(), "must have 'terms'");
    assert!(body["terms"].is_array(), "'terms' must be array");

    let terms = body["terms"].as_array().unwrap();
    for term in terms {
        assert!(term.get("hash").is_some(), "term must have 'hash'");
        assert!(term["hash"].is_string(), "term 'hash' must be string");
        assert!(
            term.get("unpacked_length").is_some(),
            "term must have 'unpacked_length'"
        );
        assert!(
            term["unpacked_length"].is_number(),
            "term 'unpacked_length' must be number"
        );
        assert!(term.get("range").is_some(), "term must have 'range'");
        assert!(
            term["range"].get("start").is_some(),
            "range must have 'start'"
        );
        assert!(term["range"].get("end").is_some(), "range must have 'end'");
    }

    assert!(body.get("fetch_info").is_some(), "must have 'fetch_info'");
    assert!(
        body["fetch_info"].is_object(),
        "'fetch_info' must be object (HashMap)"
    );

    for (key, infos) in body["fetch_info"].as_object().unwrap() {
        assert_eq!(key.len(), 64, "fetch_info key must be 64-char hex hash");
        assert!(infos.is_array(), "fetch_info value must be array");
        for info in infos.as_array().unwrap() {
            assert!(info.get("range").is_some(), "fetch_info must have 'range'");
            assert!(info.get("url").is_some(), "fetch_info must have 'url'");
            assert!(info["url"].is_string(), "fetch_info 'url' must be string");
            assert!(
                info.get("url_range").is_some(),
                "fetch_info must have 'url_range'"
            );
            assert!(
                info["url_range"].get("start").is_some(),
                "url_range must have 'start'"
            );
            assert!(
                info["url_range"].get("end").is_some(),
                "url_range must have 'end'"
            );
        }
    }
}

/// Test 7: Very large file (10 MiB) — ensures multiple xorb groups work correctly
/// with the xet-core client download flow.
#[tokio::test]
async fn test_xetcore_large_file_multi_xorb() {
    let server = TestServer::start().await;
    let data = generate_test_data(10 * 1024 * 1024); // 10 MiB
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Should produce multiple chunks given CDC parameters (target 64K, min 8K, max 128K)
    assert!(
        artifacts.chunk_hashes.len() > 50,
        "10 MiB should produce many chunks (got {})",
        artifacts.chunk_hashes.len()
    );

    // Upload all xorbs with footer
    for i in 0..artifacts.xorb_entries.len() {
        let (hash, xorb_with_footer) = build_xorb_with_footer(&artifacts, i);
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(xorb_with_footer)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Upload shard via /shards
    let resp = server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Full reconstruction
    let read_token = server.read_token();
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&read_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();

    // Reconstruct and verify
    let reconstructed = reconstruct_file_from_response(&server.client, &recon, &read_token).await;
    assert_eq!(reconstructed.len(), data.len());
    assert_eq!(reconstructed, data);
}

/// Test 8: Dedup across two uploads — xet-core relies on dedup to avoid
/// re-uploading chunks that already exist on the server.
#[tokio::test]
async fn test_xetcore_dedup_across_uploads() {
    let server = TestServer::start().await;

    // Upload a file
    let data1 = generate_test_data(256 * 1024);
    let artifacts1 = build_upload_artifacts(&data1);
    upload_artifacts(&server, &artifacts1).await;

    let read_token = server.read_token();

    // For each chunk in the first upload, verify dedup returns it
    let mut dedup_hits = 0;
    for chunk_hash in &artifacts1.chunk_hashes {
        let resp = server
            .client
            .get(format!(
                "{}/v1/chunks/default-merkledb/{}",
                server.base_url,
                chunk_hash.to_hex()
            ))
            .bearer_auth(&read_token)
            .send()
            .await
            .unwrap();

        if resp.status() == 200 {
            dedup_hits += 1;
        }
    }

    assert_eq!(
        dedup_hits,
        artifacts1.chunk_hashes.len(),
        "all chunks from first upload should be found via dedup"
    );

    // Upload the same data again — xorbs should be idempotent
    let token = server.write_token();
    for (hash, xorb_data) in &artifacts1.xorb_entries {
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(xorb_data.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["was_inserted"], false,
            "re-uploading same xorb should return was_inserted=false"
        );
    }
}

/// Test 9: Verify the server handles xet-core's reconstruction response correctly
/// when there are xorbs with footer bytes — the url_range in fetch_info must
/// still point to valid chunk data (not include the footer).
#[tokio::test]
async fn test_xetcore_fetch_info_url_range_with_footer() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Upload xorbs with footer
    for i in 0..artifacts.xorb_entries.len() {
        let (hash, xorb_with_footer) = build_xorb_with_footer(&artifacts, i);
        server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(xorb_with_footer)
            .send()
            .await
            .unwrap();
    }

    // Upload shard
    server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();

    // Get reconstruction
    let read_token = server.read_token();
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&read_token)
        .send()
        .await
        .unwrap();

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();

    // For each fetch_info entry, verify the url_range points to parseable chunk data
    for (xorb_hash, infos) in &recon.fetch_info {
        for info in infos {
            let resp = server
                .client
                .get(&info.url)
                .bearer_auth(&read_token)
                .header(
                    "Range",
                    format!("bytes={}-{}", info.url_range.start, info.url_range.end),
                )
                .send()
                .await
                .unwrap();

            let range_data = resp.bytes().await.unwrap();

            // Parse all chunks in the range — they must all be valid
            let mut pos = 0usize;
            let mut chunk_count = 0;
            while pos + ChunkHeader::SIZE <= range_data.len() {
                let header_bytes: [u8; 8] = range_data[pos..pos + 8].try_into().unwrap();
                let header = ChunkHeader::from_bytes(&header_bytes).unwrap_or_else(|e| {
                    panic!("invalid chunk header in {xorb_hash} at byte {pos}: {e}")
                });

                let compressed_end = pos + ChunkHeader::SIZE + header.compressed_size as usize;
                assert!(
                    compressed_end <= range_data.len(),
                    "chunk data extends beyond fetched range in {xorb_hash}"
                );

                let compressed = &range_data[pos + ChunkHeader::SIZE..compressed_end];
                decompress_chunk(
                    compressed,
                    header.compression_type,
                    header.uncompressed_size as usize,
                )
                .unwrap_or_else(|e| panic!("failed to decompress chunk in {xorb_hash}: {e}"));

                chunk_count += 1;
                pos = compressed_end;
            }

            assert!(
                chunk_count >= (info.range.end - info.range.start),
                "expected at least {} chunks, got {chunk_count} for {xorb_hash}",
                info.range.end - info.range.start,
            );
        }
    }
}
