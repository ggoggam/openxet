mod helpers;

use hmac::{Hmac, Mac};
use openxet_cas_types::shard::Shard;
use openxet_hashing::MerkleHash;
use sha2::Sha256;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

type HmacSha256 = Hmac<Sha256>;

/// Compute HMAC-SHA256 of a MerkleHash, returning a new MerkleHash.
fn hmac_hash(key: &[u8; 32], hash: &MerkleHash) -> MerkleHash {
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(hash.as_bytes());
    let result = mac.finalize().into_bytes();
    MerkleHash::from_bytes(result.into())
}

/// Dedup endpoint returns an HMAC-protected shard with CAS info.
#[tokio::test]
async fn test_dedup_returns_hmac_shard() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let chunk_hash = artifacts.chunk_hashes[0].to_hex();

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
    let shard = Shard::from_bytes(&body).unwrap();

    // Dedup response should have footer with HMAC key
    assert!(shard.footer.is_some());
    let footer = shard.footer.as_ref().unwrap();
    assert_ne!(footer.chunk_hash_hmac_key, [0u8; 32]);

    // Should have no file info blocks (dedup response)
    assert!(shard.file_info_blocks.is_empty());

    // Should have CAS info blocks
    assert!(!shard.cas_info_blocks.is_empty());
}

/// Verify HMAC consistency: client can verify chunk identity by HMACing its own hashes.
#[tokio::test]
async fn test_dedup_hmac_verification() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let chunk_hash = artifacts.chunk_hashes[0].to_hex();

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
    let shard = Shard::from_bytes(&body).unwrap();
    let footer = shard.footer.as_ref().unwrap();
    let hmac_key = &footer.chunk_hash_hmac_key;

    // HMAC our known chunk hash and verify it appears in the response
    let hmac_of_known = hmac_hash(hmac_key, &artifacts.chunk_hashes[0]);

    let mut found = false;
    for cas_block in &shard.cas_info_blocks {
        for entry in &cas_block.entries {
            if entry.chunk_hash == hmac_of_known {
                found = true;
                break;
            }
        }
    }
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
