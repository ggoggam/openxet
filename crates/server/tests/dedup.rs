mod helpers;

use std::io::Cursor;

use xet_core_structures::metadata_shard::MDBShardInfo;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

/// Dedup endpoint returns an HMAC-keyed shard with CAS info.
#[tokio::test]
async fn test_dedup_returns_hmac_shard() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let chunk_hash = artifacts.chunk_hashes[0].hex();

    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{chunk_hash}",
            server.base_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.bytes().await.unwrap();
    let mut reader = Cursor::new(&body[..]);
    let shard_info = MDBShardInfo::load_from_reader(&mut reader).unwrap();

    // Dedup response footer must carry an HMAC key
    assert!(shard_info.chunk_hashes_protected());
    assert!(shard_info.chunk_hmac_key().is_some());

    // Should have no file info (dedup response), but xorb blocks present
    assert_eq!(shard_info.num_file_entries(), 0);
    let xorb_blocks = shard_info.read_all_xorb_blocks_full(&mut reader).unwrap();
    assert!(!xorb_blocks.is_empty());
}

/// HMAC consistency, verified the way a real xet-core client consumes the
/// response: raw (unkeyed) chunk hashes fed to chunk_hash_dedup_query must
/// match the keyed entries, and manual keyed-hash lookup must agree.
#[tokio::test]
async fn test_dedup_hmac_verification() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let chunk_hash = artifacts.chunk_hashes[0].hex();

    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{chunk_hash}",
            server.base_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.bytes().await.unwrap();
    let mut reader = Cursor::new(&body[..]);
    let shard_info = MDBShardInfo::load_from_reader(&mut reader).unwrap();

    // The client-side query path: raw hashes in, keyed matching internally.
    let (matched, entry) = shard_info
        .chunk_hash_dedup_query(&mut reader, &artifacts.chunk_hashes)
        .unwrap()
        .expect("uploaded chunks must dedup against the response shard");
    assert!(matched >= 1);
    assert_eq!(entry.xorb_hash.hex(), artifacts.xorb_entries[0].0);

    // Manual check: HMAC our known chunk hash and find it among the entries.
    let key = shard_info.chunk_hmac_key().unwrap();
    let keyed_known = artifacts.chunk_hashes[0].hmac(key);

    let xorb_blocks = shard_info.read_all_xorb_blocks_full(&mut reader).unwrap();
    let found = xorb_blocks
        .iter()
        .flat_map(|b| &b.chunks)
        .any(|e| e.chunk_hash == keyed_known);
    assert!(
        found,
        "HMAC of known chunk hash not found in dedup response"
    );
}

/// Querying dedup for an unknown chunk returns 404.
#[tokio::test]
async fn test_dedup_unknown_chunk_404() {
    let server = TestServer::start().await;
    let token = server.read_token();
    let unknown_hash = "c".repeat(64);

    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{unknown_hash}",
            server.base_url
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
