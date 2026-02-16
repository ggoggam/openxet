mod helpers;

use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use openxet_cas_types::shard::{Shard, ShardHeader};

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

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

    // Verify via management content endpoint (full data integrity check)
    let content_resp = server
        .client
        .get(format!(
            "{}/api/files/{}/content",
            server.base_url, artifacts.file_hash
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(content_resp.status(), 200);
    let downloaded = content_resp.bytes().await.unwrap();
    assert_eq!(downloaded.as_ref(), data.as_slice());
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

/// Management upload endpoint round-trip.
#[tokio::test]
async fn test_management_upload_then_download() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);

    // Upload via management endpoint (no auth required)
    let resp = server
        .client
        .post(format!("{}/api/upload", server.base_url))
        .body(data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let upload_resp: serde_json::Value = resp.json().await.unwrap();
    let file_hash = upload_resp["file_hash"].as_str().unwrap();
    assert_eq!(upload_resp["file_size"], data.len());

    // Download content
    let resp = server
        .client
        .get(format!("{}/api/files/{file_hash}/content", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());

    // Verify stats
    let resp = server
        .client
        .get(format!("{}/api/stats", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let stats: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(stats["files_count"], 1);
    assert!(stats["xorbs_count"].as_u64().unwrap() >= 1);
    assert!(stats["shards_count"].as_u64().unwrap() >= 1);

    // Verify file list
    let resp = server
        .client
        .get(format!("{}/api/files", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let files: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["hash"].as_str().unwrap(), file_hash);
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

/// Multipart upload session flow.
#[tokio::test]
async fn test_multipart_upload_flow() {
    let server = TestServer::start().await;
    let data = generate_test_data(100 * 1024); // 100 KiB

    // Init
    let resp = server
        .client
        .post(format!("{}/api/upload/init", server.base_url))
        .json(&serde_json::json!({ "file_size": data.len() }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let init: serde_json::Value = resp.json().await.unwrap();
    let session_id = init["session_id"].as_str().unwrap();

    // Upload in two parts
    let mid = data.len() / 2;
    let resp = server
        .client
        .put(format!("{}/api/upload/{session_id}/0", server.base_url))
        .body(data[..mid].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .put(format!("{}/api/upload/{session_id}/1", server.base_url))
        .body(data[mid..].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Complete
    let resp = server
        .client
        .post(format!(
            "{}/api/upload/{session_id}/complete",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let upload_resp: serde_json::Value = resp.json().await.unwrap();
    let file_hash = upload_resp["file_hash"].as_str().unwrap();

    // Verify content
    let resp = server
        .client
        .get(format!("{}/api/files/{file_hash}/content", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());
}
