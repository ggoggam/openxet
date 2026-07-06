mod helpers;

use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use openxet_cas_types::shard::{Shard, ShardHeader};

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

    // Read token on write endpoints → 401
    let read_token = server.read_token();

    let resp = server
        .client
        .post(format!("{}/v1/xorbs/default/{dummy_hash}", server.base_url))
        .bearer_auth(&read_token)
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(&read_token)
        .body(vec![0u8; 8])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

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

/// Shard upload with non-zero footer_size is rejected.
#[tokio::test]
async fn test_shard_rejects_nonzero_footer() {
    let server = TestServer::start().await;
    let token = server.write_token();

    // Build a shard with footer_size != 0
    let shard = Shard {
        header: ShardHeader::new(64), // non-zero footer
        file_info_blocks: vec![],
        cas_info_blocks: vec![],
        footer: None,
    };
    let shard_bytes = shard.to_bytes().unwrap();

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
