//! Integration tests exercising xet-core's RemoteClient behavior.
//!
//! These tests verify that our server is wire-compatible with HuggingFace's
//! xet-core client by using the *actual* xet-core-structures client
//! primitives (xorb serialization, shard parsing, dedup queries,
//! deserialize_chunks) against our HTTP endpoints, replicating xet-core's
//! request patterns, URL paths, and response parsing.

mod helpers;

use std::io::Cursor;

use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use xet_core_structures::metadata_shard::MDBShardInfo;
use xet_core_structures::xorb_object::{XorbObject, deserialize_chunks};

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

// ─── Client-side helpers (the same code paths real xet-core clients run) ─────

/// Strip the trailing XorbObjectInfoV1 footer from a serialized xorb, leaving
/// only the raw chunk frames — the spec-minimal upload form.
fn strip_xorb_footer(xorb_bytes: &[u8]) -> Vec<u8> {
    let xorb = XorbObject::deserialize(&mut Cursor::new(xorb_bytes)).unwrap();
    let chunk_region_end = *xorb.info.chunk_boundary_offsets.last().unwrap() as usize;
    xorb_bytes[..chunk_region_end].to_vec()
}

/// Simulate xet-core's download flow: given a QueryReconstructionResponse,
/// fetch xorb byte ranges and decode them with the same `deserialize_chunks`
/// call xet-core's RemoteClient uses, then assemble the file.
async fn reconstruct_file_from_response(
    client: &reqwest::Client,
    recon: &QueryReconstructionResponse,
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

        // xet-core fetches the xorb byte range using the url and url_range, with
        // no Authorization header (the url is presigned / self-authenticating).
        let resp = client
            .get(&fi.url)
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

        // Decode the fetched chunk frames exactly as xet-core does.
        let (decoded, boundaries) =
            deserialize_chunks(&mut Cursor::new(&xorb_range_data[..])).unwrap();

        // The fetch_info range may cover more chunks than this term needs.
        // Select this term's sub-range within the fetched slice.
        let local_start = term.range.start - fi.range.start;
        let local_end = term.range.end - fi.range.start;
        assert!(local_end < boundaries.len());

        let term_bytes = &decoded[boundaries[local_start] as usize..boundaries[local_end] as usize];

        if term_idx == 0 {
            let skip = recon.offset_into_first_range as usize;
            result.extend_from_slice(&term_bytes[skip.min(term_bytes.len())..]);
        } else {
            result.extend_from_slice(term_bytes);
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

/// Test 2: spec-minimal clients may upload xorbs WITHOUT the metadata footer.
/// Our server must validate and store those too (real xet-core always
/// includes the footer; that path is covered by every other test here since
/// build_upload_artifacts serializes through SerializedXorbObject).
#[tokio::test]
async fn test_footerless_xorb_upload_accepted() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    // Upload xorbs with the footer stripped (hash is over chunk content, so
    // it is unchanged).
    for (hash, xorb_data) in &artifacts.xorb_entries {
        let footerless = strip_xorb_footer(xorb_data);
        assert!(footerless.len() < xorb_data.len());

        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .body(footerless)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "footer-less xorb should be accepted after content validation"
        );

        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["was_inserted"], true);
    }

    // Shard registration + full reconstruction still work
    let resp = server
        .client
        .post(format!("{}/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

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
    let reconstructed = reconstruct_file_from_response(&server.client, &recon).await;
    assert_eq!(reconstructed, data);
}

/// Test 3: a xorb whose trailing bytes are not a valid footer must be
/// rejected — the server would otherwise store metadata it can't trust.
#[tokio::test]
async fn test_corrupt_footer_rejected() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    let (hash, xorb_data) = &artifacts.xorb_entries[0];
    let mut corrupted = xorb_data.to_vec();
    // Flip bytes inside the footer's chunk-hash section (past the chunk region).
    let footer_start = strip_xorb_footer(xorb_data).len();
    corrupted[footer_start + 10] ^= 0xFF;
    corrupted[footer_start + 11] ^= 0xFF;

    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
        .bearer_auth(&token)
        .body(corrupted)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        400,
        "xorb with corrupt footer must be rejected"
    );
}

/// Test 4: Full xet-core upload + download round-trip with large data.
/// Generates ~2 MiB of pseudo-random data to ensure multiple CDC chunks,
/// then reconstructs via the client download flow.
#[tokio::test]
async fn test_xetcore_full_roundtrip_large_file() {
    let server = TestServer::start().await;
    let data = generate_test_data(2 * 1024 * 1024); // 2 MiB
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

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
    let reconstructed = reconstruct_file_from_response(&server.client, &recon).await;
    assert_eq!(reconstructed.len(), data.len());
    assert_eq!(reconstructed, data);
}

/// Test 5: xet-core dedup flow — query chunk, parse the keyed shard with the
/// real client parser, and match raw chunk hashes through the HMAC.
#[tokio::test]
async fn test_xetcore_dedup_flow() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let read_token = server.read_token();

    // Query dedup for a known chunk (as xet-core does)
    let chunk_hash = artifacts.chunk_hashes[0];
    let chunk_hash_hex = chunk_hash.hex();

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
    let mut reader = Cursor::new(&shard_bytes[..]);
    let shard_info = MDBShardInfo::load_from_reader(&mut reader)
        .expect("dedup response should parse with the real client parser");

    // xet-core expects: footer with HMAC key and a future expiry
    let key = shard_info
        .chunk_hmac_key()
        .expect("dedup shard must carry an HMAC key");
    assert!(
        shard_info.metadata.shard_key_expiry > shard_info.metadata.shard_creation_timestamp,
        "key expiry must be in the future"
    );

    // xet-core expects: xorb (CAS) info with HMAC-keyed chunk hashes
    let xorb_blocks = shard_info.read_all_xorb_blocks_full(&mut reader).unwrap();
    assert!(
        !xorb_blocks.is_empty(),
        "dedup shard must have xorb info blocks"
    );

    // The blake3-keyed HMAC of our raw chunk hash must appear in the entries
    let keyed = chunk_hash.hmac(key);
    let found = xorb_blocks
        .iter()
        .flat_map(|b| &b.chunks)
        .any(|e| e.chunk_hash == keyed);
    assert!(
        found,
        "HMAC of queried chunk hash should appear in dedup response"
    );

    // And the end-to-end client query path agrees
    let (matched, entry) = shard_info
        .chunk_hash_dedup_query(&mut reader, &artifacts.chunk_hashes)
        .unwrap()
        .expect("dedup query must match the uploaded chunks");
    assert!(matched >= 1);
    assert_eq!(entry.xorb_hash.hex(), artifacts.xorb_entries[0].0);
}

/// Test 6: xet-core reconstruction with Range header for partial download.
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

    // The ranged reconstruction actually decodes to the right bytes
    let reconstructed = reconstruct_file_from_response(&server.client, &recon).await;
    assert!(reconstructed.len() as u64 >= requested_bytes);
    assert_eq!(
        &reconstructed[..requested_bytes as usize],
        &data[range_start as usize..=range_end as usize],
    );
}

/// Test 7: xet-core response format validation.
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

/// Test 8: Very large file (10 MiB) — ensures many chunks work correctly
/// with the xet-core client download flow.
#[tokio::test]
async fn test_xetcore_large_file_multi_xorb() {
    let server = TestServer::start().await;
    let data = generate_test_data(10 * 1024 * 1024); // 10 MiB
    let artifacts = build_upload_artifacts(&data);

    // Should produce multiple chunks given CDC parameters (target 64K)
    assert!(
        artifacts.chunk_hashes.len() > 50,
        "10 MiB should produce many chunks (got {})",
        artifacts.chunk_hashes.len()
    );

    upload_artifacts(&server, &artifacts).await;

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
    let reconstructed = reconstruct_file_from_response(&server.client, &recon).await;
    assert_eq!(reconstructed.len(), data.len());
    assert_eq!(reconstructed, data);
}

/// Test 9: Dedup across two uploads — xet-core relies on dedup to avoid
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
                chunk_hash.hex()
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

/// Test 10: fetch_info url_range must cover exactly the chunk frames (never
/// the metadata footer), and every fetched range must decode cleanly.
#[tokio::test]
async fn test_xetcore_fetch_info_url_range_excludes_footer() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

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

    for (xorb_hash, infos) in &recon.fetch_info {
        for info in infos {
            // Fetched without an Authorization header, as xet-core does.
            let resp = server
                .client
                .get(&info.url)
                .header(
                    "Range",
                    format!("bytes={}-{}", info.url_range.start, info.url_range.end),
                )
                .send()
                .await
                .unwrap();

            let range_data = resp.bytes().await.unwrap();

            // Every fetched range must decode with the client's chunk-frame
            // decoder — a footer byte in the range would make this fail.
            let (_, boundaries) = deserialize_chunks(&mut Cursor::new(&range_data[..]))
                .unwrap_or_else(|e| panic!("range for {xorb_hash} failed to decode: {e}"));
            let chunk_count = boundaries.len() - 1;

            assert!(
                chunk_count >= (info.range.end - info.range.start),
                "expected at least {} chunks, got {chunk_count} for {xorb_hash}",
                info.range.end - info.range.start,
            );
        }
    }
}
