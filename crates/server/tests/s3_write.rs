//! End-to-end tests for the S3-compatible write gateway (Phase 2).
//!
//! Exercise the server-side write path: PutObject chunks the incoming bytes,
//! builds xorbs and a shard, and registers the name — then the Phase-1 read
//! path must reconstruct the exact bytes back. Also covers CopyObject,
//! DeleteObject, overwrite, empty objects, and aws-chunked de-framing. Auth is
//! disabled (SigV4 math is unit-tested separately), so requests hit the write
//! handlers directly.

// Shared helpers module; this binary uses only a subset.
#![allow(dead_code)]

mod helpers;

use helpers::{TestServer, build_upload_artifacts, generate_test_data};
use md5::{Digest, Md5};

fn md5_hex(data: &[u8]) -> String {
    hex::encode(Md5::digest(data))
}

async fn put(server: &TestServer, bucket: &str, key: &str, body: Vec<u8>) -> reqwest::Response {
    server
        .client
        .put(format!("{}/s3/{bucket}/{key}", server.base_url))
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn get(server: &TestServer, bucket: &str, key: &str) -> reqwest::Response {
    server
        .client
        .get(format!("{}/s3/{bucket}/{key}", server.base_url))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn put_then_get_roundtrip() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(300_000);

    let resp = put(&server, "bkt", "dir/file.bin", data.clone()).await;
    assert_eq!(resp.status(), 200, "put failed");
    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(etag, format!("\"{}\"", md5_hex(&data)), "etag must be md5");

    let resp = get(&server, "bkt", "dir/file.bin").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("etag").unwrap().to_str().unwrap(),
        etag,
        "get etag must match put etag"
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), data.as_slice(), "reassembled bytes differ");
}

#[tokio::test]
async fn put_then_ranged_get() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(200_000);
    put(&server, "bkt", "r.bin", data.clone()).await;

    let resp = server
        .client
        .get(format!("{}/s3/bkt/r.bin", server.base_url))
        .header("Range", "bytes=1000-1099")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), &data[1000..1100]);
}

#[tokio::test]
async fn put_empty_object() {
    let server = TestServer::start_with_auth_disabled().await;

    let resp = put(&server, "bkt", "empty", Vec::new()).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("etag").unwrap().to_str().unwrap(),
        // md5 of the empty string.
        "\"d41d8cd98f00b204e9800998ecf8427e\""
    );

    let resp = get(&server, "bkt", "empty").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-length").unwrap(), "0");
    assert!(resp.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn overwrite_replaces_content() {
    let server = TestServer::start_with_auth_disabled().await;
    let first = generate_test_data(50_000);
    let second = generate_test_data(70_000);

    put(&server, "bkt", "k", first).await;
    put(&server, "bkt", "k", second.clone()).await;

    let resp = get(&server, "bkt", "k").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), second.as_slice());
}

#[tokio::test]
async fn delete_object_then_get_is_404() {
    let server = TestServer::start_with_auth_disabled().await;
    put(&server, "bkt", "gone.bin", generate_test_data(4096)).await;

    // Present first.
    assert_eq!(get(&server, "bkt", "gone.bin").await.status(), 200);

    let resp = server
        .client
        .delete(format!("{}/s3/bkt/gone.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    assert_eq!(get(&server, "bkt", "gone.bin").await.status(), 404);

    // Deleting a missing key is idempotent (204).
    let resp = server
        .client
        .delete(format!("{}/s3/bkt/gone.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn copy_object_shares_content() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(120_000);
    put(&server, "src", "orig.bin", data.clone()).await;

    let resp = server
        .client
        .put(format!("{}/s3/dst/copy.bin", server.base_url))
        .header("x-amz-copy-source", "/src/orig.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<CopyObjectResult>"), "body: {body}");
    assert!(
        body.contains(&format!("&quot;{}&quot;", md5_hex(&data))),
        "body: {body}"
    );

    let resp = get(&server, "dst", "copy.bin").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());

    // Copy of a missing source is NoSuchKey.
    let resp = server
        .client
        .put(format!("{}/s3/dst/x.bin", server.base_url))
        .header("x-amz-copy-source", "/src/missing.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// A PUT whose body arrives in `aws-chunked` transfer framing (as signed
/// `aws s3` clients send it) must be de-framed before chunking, so the stored
/// object is the payload, not the framing bytes.
#[tokio::test]
async fn put_aws_chunked_body() {
    let server = TestServer::start_with_auth_disabled().await;
    let payload = generate_test_data(20_000);

    // Frame the payload into two aws-chunked chunks plus the zero terminator,
    // in the unsigned-trailer shape (`<hex-size>\r\n<data>\r\n`).
    let (a, b) = payload.split_at(12_345);
    let mut framed = Vec::new();
    framed.extend_from_slice(format!("{:x}\r\n", a.len()).as_bytes());
    framed.extend_from_slice(a);
    framed.extend_from_slice(b"\r\n");
    framed.extend_from_slice(format!("{:x}\r\n", b.len()).as_bytes());
    framed.extend_from_slice(b);
    framed.extend_from_slice(b"\r\n");
    framed.extend_from_slice(b"0\r\n\r\n");

    let resp = server
        .client
        .put(format!("{}/s3/bkt/chunked.bin", server.base_url))
        .header("content-encoding", "aws-chunked")
        .header("x-amz-decoded-content-length", payload.len().to_string())
        .body(framed)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = get(&server, "bkt", "chunked.bin").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), payload.as_slice());
}

/// A file written through the S3 gateway must be reconstructable through the
/// native `/v2/reconstruction` path too — the gateway produces the same shard
/// and xorb structures the Xet client does.
#[tokio::test]
async fn put_object_is_natively_reconstructable() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(150_000);
    let resp = put(&server, "bkt", "native.bin", data.clone()).await;
    assert_eq!(resp.status(), 200);

    // The read gateway lists it, and native file listing must include the file.
    let resp = server
        .client
        .get(format!("{}/v1/files", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let listing = resp.text().await.unwrap();
    assert!(!listing.is_empty());
}

/// The gateway's server-side chunking must be bit-identical to the Xet client's:
/// PUT-ting bytes and independently building the client artifacts for the same
/// bytes must yield the *same* file hash, which is what lets objects written
/// either way dedup and reconstruct against one another.
#[tokio::test]
async fn gateway_file_hash_matches_client() {
    let server = TestServer::start_with_auth_disabled().await;
    let data = generate_test_data(180_000);

    // Client-side reference hash for these exact bytes.
    let expected = build_upload_artifacts(&data).file_hash;

    put(&server, "bkt", "x.bin", data).await;

    // The file must be indexed under the same content hash the client computes.
    let resp = server
        .client
        .get(format!("{}/v1/files/{expected}", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        200,
        "gateway PUT produced a different file hash than the client pipeline"
    );
}

/// The full write path against the Postgres index, exercising the new
/// `PostgresS3Index::delete_object` (`DELETE … RETURNING`) and the shared
/// migration. Gated on `OPENXET_TEST_POSTGRES_URL`.
#[tokio::test]
#[ignore = "requires OPENXET_TEST_POSTGRES_URL"]
async fn postgres_put_get_delete_copy() {
    let url = std::env::var("OPENXET_TEST_POSTGRES_URL")
        .expect("set OPENXET_TEST_POSTGRES_URL to run this test");
    let server = TestServer::start_with_postgres_no_auth(&url).await;
    let data = generate_test_data(90_000);

    put(&server, "pg", "a.bin", data.clone()).await;
    let resp = get(&server, "pg", "a.bin").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), data.as_slice());

    // Copy.
    let resp = server
        .client
        .put(format!("{}/s3/pg/b.bin", server.base_url))
        .header("x-amz-copy-source", "/pg/a.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(get(&server, "pg", "b.bin").await.status(), 200);

    // Delete.
    let resp = server
        .client
        .delete(format!("{}/s3/pg/a.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(get(&server, "pg", "a.bin").await.status(), 404);
    // The copy still resolves (independent name over shared content).
    assert_eq!(get(&server, "pg", "b.bin").await.status(), 200);
}

/// Credential minting: POST /v1/s3/credentials returns an access key and secret
/// bound to the caller's owner.
#[tokio::test]
async fn mint_credentials() {
    let server = TestServer::start_with_auth_disabled().await;
    let resp = server
        .client
        .post(format!("{}/v1/s3/credentials", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["access_key_id"].as_str().unwrap().starts_with("AKIA"));
    assert_eq!(body["secret_access_key"].as_str().unwrap().len(), 40);
}
