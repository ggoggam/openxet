//! Integration test for the Postgres index backend.
//!
//! Gated on `OPENXET_TEST_POSTGRES_URL` so the default suite (which has no
//! database) skips it. To run:
//!
//! ```sh
//! # against the compose postgres (docker/compose.rustfs.yaml)
//! OPENXET_TEST_POSTGRES_URL=postgres://openxet:openxet@localhost:5432/openxet \
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
    // `_sqlx_migrations` must go too: dropping the data tables while leaving
    // the migration ledger would make the migrator skip recreating them.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .unwrap();
    sqlx::query(
        "DROP TABLE IF EXISTS file_index, chunk_index, xorb_layout, file_ownership, \
         _sqlx_migrations",
    )
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
    let chunk_hash = artifacts.chunk_hashes[0].hex();
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

    // Management listings against Postgres: the file and its xorbs are
    // enumerable, and file detail resolves owners + referenced xorbs.
    let resp = server
        .client
        .get(format!("{}/v1/files?limit=10", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "file listing failed");
    let files: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(files["items"].as_array().unwrap().len(), 1);
    assert_eq!(files["items"][0]["file_hash"], artifacts.file_hash);
    assert_eq!(files["items"][0]["logical_bytes"], data.len() as u64);

    let resp = server
        .client
        .get(format!("{}/v1/xorbs?limit=1", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "xorb listing failed");
    let xorbs: serde_json::Value = resp.json().await.unwrap();
    assert!(!xorbs["items"].as_array().unwrap().is_empty());
    assert!(xorbs["items"][0]["chunk_count"].as_u64().unwrap() > 0);

    let resp = server
        .client
        .get(format!(
            "{}/v1/files/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "file detail failed");
    let detail: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(detail["owners"][0]["owner"], "test-user");
    assert!(!detail["xorbs"].as_array().unwrap().is_empty());

    // Lifecycle against Postgres: the upload recorded an ownership claim,
    // accounting sees it, and delete + GC reclaims storage and index rows.
    let resp = server
        .client
        .get(format!("{}/v1/accounting", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "accounting query failed");
    let acc: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(acc["claimed_files"], 1);
    assert_eq!(acc["unique_file_bytes"], data.len() as u64);
    assert_eq!(acc["owners"][0]["owner"], "test-user");

    let resp = server
        .client
        .delete(format!(
            "{}/v1/files/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "delete failed");
    let del: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(del["deleted"], true);

    let resp = server
        .client
        .post(format!("{}/v1/gc?grace_seconds=0", server.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "gc failed");
    let report: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(report["deleted_xorbs"], artifacts.xorb_entries.len() as u64);
    assert_eq!(report["deleted_shards"], 1);

    // The dedup entry must be gone from the Postgres chunk index too.
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
    assert_eq!(resp.status(), 404, "collected chunk still advertised");
}
