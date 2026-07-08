mod helpers;

use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use xet_core_structures::metadata_shard::MDBShardFileHeader;
use xet_core_structures::metadata_shard::file_structs::FileDataSequenceHeader;
use xet_core_structures::metadata_shard::xorb_structs::XorbChunkSequenceHeader;

use helpers::{
    TestServer, build_upload_artifacts, download_via_protocol, generate_test_data, upload_artifacts,
};

/// Full CAS protocol round-trip: upload xorbs → upload shard → reconstruct → verify data.
#[tokio::test]
async fn test_cas_upload_then_reconstruct() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024); // 256 KiB — multiple CDC chunks
    let artifacts = build_upload_artifacts(&data);

    // Upload via CAS protocol
    upload_artifacts(&server, &artifacts).await;

    // Query reconstruction
    let token = server.read_token();
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let recon: QueryReconstructionResponse = resp.json().await.unwrap();
    assert_eq!(recon.offset_into_first_range, 0);
    assert!(!recon.terms.is_empty());

    // Verify total size matches
    let total: u64 = recon.terms.iter().map(|t| t.unpacked_length).sum();
    assert_eq!(total, data.len() as u64);

    // Verify each term has fetch info
    for term in &recon.terms {
        assert!(
            recon.fetch_info.contains_key(&term.hash),
            "missing fetch_info for xorb {}",
            term.hash
        );
    }

    // Full data integrity check: download via the protocol (fetch_info ranges)
    let downloaded = download_via_protocol(&server, &artifacts.file_hash).await;
    assert_eq!(downloaded, data);
}

/// Xorb uploads are idempotent.
#[tokio::test]
async fn test_xorb_upload_idempotent() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    let (hash, xorb_data) = &artifacts.xorb_entries[0];

    // First upload
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
    assert_eq!(body["was_inserted"], true);

    // Second upload — same xorb
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
    assert_eq!(body["was_inserted"], false);
}

/// All CAS endpoints require authentication.
#[tokio::test]
async fn test_auth_required_for_cas_endpoints() {
    let server = TestServer::start().await;
    let dummy_hash = "a".repeat(64);

    // No auth → 401
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{dummy_hash}",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{dummy_hash}",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{dummy_hash}", server.base_url))
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Read token on write endpoints → 403 (valid token, insufficient scope)
    let read_token = server.read_token();

    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{dummy_hash}", server.base_url))
        .bearer_auth(&read_token)
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(&read_token)
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Read token on read endpoints → should work (404 for missing data, not 401)
    let resp = server
        .client
        .get(format!(
            "{}/v1/reconstructions/{dummy_hash}",
            server.base_url
        ))
        .bearer_auth(&read_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// Malformed hash in the path is 400 on every hash-parameterized endpoint
/// (spec: "Malformed hash in the path. Fix the path before retrying").
#[tokio::test]
async fn test_malformed_hash_is_400() {
    let server = TestServer::start().await;
    let bad = "not-a-valid-hash"; // wrong length and non-hex

    // Reconstruction (read)
    let resp = server
        .client
        .get(format!("{}/v1/reconstructions/{bad}", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Dedup (read)
    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{bad}",
            server.base_url
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Xorb upload (write)
    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{bad}", server.base_url))
        .bearer_auth(server.write_token())
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// Oversized uploads return 413 Payload Too Large. (The HF spec documents this
/// as a 400, but we return the semantically correct HTTP status; xet-core
/// treats both as non-retryable 4xx.)
#[tokio::test]
async fn test_oversized_upload_is_413() {
    let server = TestServer::start().await;
    let token = server.write_token();
    let body = vec![0u8; 64 * 1024 * 1024 + 1]; // MAX_SHARD_SIZE + 1

    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(&token)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}

/// Shard upload with non-zero footer_size is rejected.
#[tokio::test]
async fn test_shard_rejects_nonzero_footer() {
    let server = TestServer::start().await;
    let token = server.write_token();

    // Build an otherwise-empty shard whose header declares footer_size != 0
    // (MDBShardFileHeader::default() carries the real footer size).
    let mut shard_bytes = Vec::new();
    MDBShardFileHeader::default()
        .serialize(&mut shard_bytes)
        .unwrap();
    FileDataSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .unwrap();
    XorbChunkSequenceHeader::bookend()
        .serialize(&mut shard_bytes)
        .unwrap();

    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(&token)
        .body(shard_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// Xorb hash mismatch is rejected.
#[tokio::test]
async fn test_xorb_hash_mismatch_rejected() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let token = server.write_token();

    let (_, xorb_data) = &artifacts.xorb_entries[0];
    let wrong_hash = "b".repeat(64);

    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{wrong_hash}", server.base_url))
        .bearer_auth(&token)
        .body(xorb_data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}
