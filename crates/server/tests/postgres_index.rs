//! Integration test for the Postgres index backend.
//!
//! Gated on `OPENXET_TEST_POSTGRES_URL` so the default suite (which has no
//! database) skips it. To run:
//!
//! ```sh
//! OPENXET_TEST_POSTGRES_URL=postgres://postgres:test@localhost:55432/openxet \
//!     cargo test -p openxet-server --test postgres_index -- --ignored
//! ```

mod helpers;

use helpers::{TestServer, build_upload_artifacts, download_via_protocol, upload_artifacts};

/// Full upload → reconstruct roundtrip plus cross-upload dedup, all backed by
/// Postgres indexes instead of RocksDB.
#[tokio::test]
#[ignore = "requires OPENXET_TEST_POSTGRES_URL"]
async fn postgres_upload_reconstruct_and_dedup() {
    let url = std::env::var("OPENXET_TEST_POSTGRES_URL")
        .expect("set OPENXET_TEST_POSTGRES_URL to run this test");

    // Start from a clean schema so dedup assertions are deterministic.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .unwrap();
    sqlx::query("DROP TABLE IF EXISTS file_index, chunk_index")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let server = TestServer::start_with_postgres(&url).await;

    // Upload a file, then read it back byte-for-byte through the protocol.
    let data = helpers::generate_test_data(512 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let roundtrip = download_via_protocol(&server, &artifacts.file_hash).await;
    assert_eq!(roundtrip, data, "reconstructed file differs from original");

    // Re-uploading the same content is deduplicated: every xorb reports
    // "already existed" (result == 0) on the second shard upload.
    let token = server.write_token();
    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .bearer_auth(&token)
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "second shard upload failed");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["result"], 0, "second shard upload should dedup");

    // Global-dedup query: this reads the xorb layout from Postgres (not the
    // object store) to build the response, so a known chunk must resolve.
    let chunk_hash = artifacts.chunk_hashes[0].to_hex();
    let resp = server
        .client
        .get(format!(
            "{}/v1/chunks/default-merkledb/{chunk_hash}",
            server.base_url
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "dedup query for a known chunk failed");
    let dedup_shard = resp.bytes().await.unwrap();
    assert!(
        !dedup_shard.is_empty(),
        "dedup response should be non-empty"
    );
}
