//! End-to-end tests for the S3-compatible read gateway (Phase 1).
//!
//! Upload a file via the Xet protocol, register a friendly `(bucket, key)`
//! name for it, then read it back through the S3 surface: GetObject (full +
//! ranged), HeadObject, and ListObjectsV2 (with and without a delimiter). Auth
//! is disabled, so the SigV4 path is bypassed (`aws --no-sign-request`); the
//! signature math itself is covered by the unit vector in `routes::s3::sigv4`.

mod helpers;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};
use serde_json::json;

/// Register an already-uploaded file under a `(bucket, key)` name.
async fn register(server: &TestServer, bucket: &str, key: &str, file_hash: &str) {
    let resp = server
        .client
        .post(format!("{}/v1/s3/objects", server.base_url))
        .json(&json!({ "bucket": bucket, "key": key, "file_hash": file_hash }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "register failed");
}

#[tokio::test]
async fn get_object_returns_full_file() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;
    register(&server, "bkt", "dir/file.bin", &artifacts.file_hash).await;

    let resp = server
        .client
        .get(format!("{}/s3/bkt/dir/file.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("etag").unwrap().to_str().unwrap(),
        format!("\"{}\"", artifacts.file_hash)
    );
    assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), data.as_slice(), "reassembled bytes differ");
}

#[tokio::test]
async fn head_object_reports_metadata() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(100_000);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;
    register(&server, "bkt", "a.bin", &artifacts.file_hash).await;

    let resp = server
        .client
        .head(format!("{}/s3/bkt/a.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap(),
        data.len().to_string()
    );
    assert_eq!(
        resp.headers().get("etag").unwrap().to_str().unwrap(),
        format!("\"{}\"", artifacts.file_hash)
    );
    assert!(resp.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn get_object_range_and_suffix() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(200_000);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;
    register(&server, "bkt", "r.bin", &artifacts.file_hash).await;

    // Closed range in the middle.
    let resp = server
        .client
        .get(format!("{}/s3/bkt/r.bin", server.base_url))
        .header("Range", "bytes=1000-1099")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers()
            .get("content-range")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("bytes 1000-1099/{}", data.len())
    );
    assert_eq!(resp.bytes().await.unwrap().as_ref(), &data[1000..1100]);

    // Suffix range: last 50 bytes.
    let resp = server
        .client
        .get(format!("{}/s3/bkt/r.bin", server.base_url))
        .header("Range", "bytes=-50")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.bytes().await.unwrap().as_ref(),
        &data[data.len() - 50..]
    );

    // Unsatisfiable range.
    let resp = server
        .client
        .get(format!("{}/s3/bkt/r.bin", server.base_url))
        .header(
            "Range",
            format!("bytes={}-{}", data.len() + 10, data.len() + 20),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 416);
}

#[tokio::test]
async fn get_missing_key_is_no_such_key() {
    let server = TestServer::start_with_auth_disabled().await;
    let resp = server
        .client
        .get(format!("{}/s3/bkt/nope.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Code>NoSuchKey</Code>"), "body: {body}");
}

#[tokio::test]
async fn list_objects_v2_prefix_and_delimiter() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(4096);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;
    register(&server, "bkt", "docs/a.txt", &artifacts.file_hash).await;
    register(&server, "bkt", "docs/b.txt", &artifacts.file_hash).await;
    register(&server, "bkt", "top.txt", &artifacts.file_hash).await;

    // Prefix filter: only the docs/ keys.
    let resp = server
        .client
        .get(format!(
            "{}/s3/bkt?list-type=2&prefix=docs/",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Key>docs/a.txt</Key>"), "body: {body}");
    assert!(body.contains("<Key>docs/b.txt</Key>"));
    assert!(!body.contains("<Key>top.txt</Key>"));

    // Delimiter grouping: docs/ collapses into a CommonPrefix at the root.
    let resp = server
        .client
        .get(format!(
            "{}/s3/bkt?list-type=2&delimiter=/",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<CommonPrefixes><Prefix>docs/</Prefix></CommonPrefixes>"),
        "body: {body}"
    );
    assert!(body.contains("<Key>top.txt</Key>"));
    assert!(!body.contains("<Key>docs/a.txt</Key>"));
}

/// Same read flow against the Postgres index, so `PostgresS3Index`
/// (put/get/list) and the `0002` postgres migration are exercised, not just
/// SQLite. Gated on `OPENXET_TEST_POSTGRES_URL`.
#[tokio::test]
#[ignore = "requires OPENXET_TEST_POSTGRES_URL"]
async fn postgres_get_list_roundtrip() {
    let url = std::env::var("OPENXET_TEST_POSTGRES_URL")
        .expect("set OPENXET_TEST_POSTGRES_URL to run this test");
    let server = TestServer::start_with_postgres_no_auth(&url).await;

    let data = generate_test_data(80_000);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;
    register(&server, "pgbkt", "sub/obj.bin", &artifacts.file_hash).await;

    // GetObject
    let resp = server
        .client
        .get(format!("{}/s3/pgbkt/sub/obj.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());

    // ListObjectsV2 with prefix
    let resp = server
        .client
        .get(format!(
            "{}/s3/pgbkt?list-type=2&prefix=sub/",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.text()
            .await
            .unwrap()
            .contains("<Key>sub/obj.bin</Key>")
    );
}

#[tokio::test]
async fn head_bucket_succeeds() {
    let server = TestServer::start_with_auth_disabled().await;
    let resp = server
        .client
        .head(format!("{}/s3/anybucket", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
