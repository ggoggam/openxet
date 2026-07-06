mod helpers;

use openxet_cas_types::shard::Shard;
use openxet_hashing::MerkleHash;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

/// A shard that declares a file_hash not matching its actual chunk content must
/// be rejected — otherwise a writer could register arbitrary bytes under any
/// file hash and poison later reconstructions (the core CAS invariant).
#[tokio::test]
async fn test_shard_rejects_mismatched_file_hash() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);

    // Happy path: xorbs + a correctly-labeled shard upload succeed.
    upload_artifacts(&server, &artifacts).await;

    // Tamper: keep the same xorbs/terms but claim a bogus file_hash.
    let mut shard = Shard::from_bytes(&artifacts.shard_bytes).unwrap();
    shard.file_info_blocks[0].header.file_hash = MerkleHash::from_bytes([0xAB; 32]);
    let tampered = shard.to_bytes().unwrap();

    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(server.write_token())
        .body(tampered)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        400,
        "shard with mismatched file_hash must be rejected"
    );
}
