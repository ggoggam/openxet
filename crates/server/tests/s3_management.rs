//! Tests for the S3 gateway's native management JSON API: listing buckets and
//! object names, deleting a name, and minting/listing/revoking credentials.
//!
//! These are the endpoints the web management UI drives. Objects are seeded
//! through the write gateway (auth disabled), then inspected via `/v1/s3/*`.

#![allow(dead_code)]

mod helpers;

use helpers::{TestServer, generate_test_data};

async fn put(server: &TestServer, bucket: &str, key: &str, body: Vec<u8>) -> reqwest::Response {
    server
        .client
        .put(format!("{}/s3/{bucket}/{key}", server.base_url))
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn get_json(server: &TestServer, path: &str) -> serde_json::Value {
    let resp = server
        .client
        .get(format!("{}{path}", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "GET {path} failed");
    resp.json().await.unwrap()
}

#[tokio::test]
async fn gateway_info_reports_prefix_and_enabled() {
    let server = TestServer::start_with_auth_disabled().await;
    let info = get_json(&server, "/v1/s3/info").await;
    assert_eq!(info["enabled"], true);
    assert_eq!(info["prefix"], "/s3");
    // No public_url configured in tests, so the endpoint falls back to the prefix.
    assert_eq!(info["endpoint"], "/s3");
}

#[tokio::test]
async fn list_buckets_aggregates_counts_and_size() {
    let server = TestServer::start_with_auth_disabled().await;
    put(&server, "alpha", "one.bin", generate_test_data(1000)).await;
    put(&server, "alpha", "two.bin", generate_test_data(2000)).await;
    put(&server, "beta", "x.bin", generate_test_data(500)).await;

    let body = get_json(&server, "/v1/s3/buckets").await;
    let buckets = body["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 2);

    // Ordered by bucket name.
    assert_eq!(buckets[0]["bucket"], "alpha");
    assert_eq!(buckets[0]["object_count"], 2);
    assert_eq!(buckets[0]["total_size"], 3000);
    assert_eq!(buckets[1]["bucket"], "beta");
    assert_eq!(buckets[1]["object_count"], 1);
    assert_eq!(buckets[1]["total_size"], 500);
}

#[tokio::test]
async fn list_objects_filters_and_paginates() {
    let server = TestServer::start_with_auth_disabled().await;
    for key in ["a/1", "a/2", "b/1"] {
        put(&server, "bkt", key, generate_test_data(100)).await;
    }

    // Prefix filter.
    let body = get_json(&server, "/v1/s3/objects?bucket=bkt&prefix=a/").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["key"], "a/1");
    assert_eq!(items[1]["key"], "a/2");
    assert!(items[0]["file_hash"].as_str().unwrap().len() == 64);

    // Keyset pagination: first page of 2 across the whole bucket, then the rest.
    let page1 = get_json(&server, "/v1/s3/objects?bucket=bkt&limit=2").await;
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    let cursor = page1["next_cursor"].as_str().expect("expected next_cursor");
    let page2 = get_json(
        &server,
        &format!("/v1/s3/objects?bucket=bkt&limit=2&cursor={cursor}"),
    )
    .await;
    let items2 = page2["items"].as_array().unwrap();
    assert_eq!(items2.len(), 1);
    assert_eq!(items2[0]["key"], "b/1");
    assert!(page2["next_cursor"].is_null());
}

#[tokio::test]
async fn list_objects_requires_bucket() {
    let server = TestServer::start_with_auth_disabled().await;
    let resp = server
        .client
        .get(format!("{}/v1/s3/objects", server.base_url))
        .send()
        .await
        .unwrap();
    // Missing required `bucket` query param → 400 from the Query extractor.
    assert!(resp.status().is_client_error(), "status: {}", resp.status());
}

#[tokio::test]
async fn delete_object_removes_the_name() {
    let server = TestServer::start_with_auth_disabled().await;
    put(&server, "bkt", "doomed", generate_test_data(256)).await;

    let resp = server
        .client
        .delete(format!(
            "{}/v1/s3/objects?bucket=bkt&key=doomed",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["deleted"],
        true
    );

    // Gone from the listing and from the read gateway.
    let body = get_json(&server, "/v1/s3/objects?bucket=bkt").await;
    assert!(body["items"].as_array().unwrap().is_empty());
    let resp = server
        .client
        .get(format!("{}/s3/bkt/doomed", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Deleting a missing name reports deleted=false (idempotent, not an error).
    let resp = server
        .client
        .delete(format!(
            "{}/v1/s3/objects?bucket=bkt&key=doomed",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["deleted"],
        false
    );
}

#[tokio::test]
async fn credentials_mint_list_and_revoke() {
    let server = TestServer::start_with_auth_disabled().await;

    // Mint two.
    let mut keys = Vec::new();
    for _ in 0..2 {
        let resp = server
            .client
            .post(format!("{}/v1/s3/credentials", server.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        keys.push(body["access_key_id"].as_str().unwrap().to_string());
    }

    // List returns both, without secrets.
    let body = get_json(&server, "/v1/s3/credentials").await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert!(item.get("secret_key").is_none());
        assert!(item.get("secret_access_key").is_none());
        assert!(item["access_key_id"].as_str().unwrap().starts_with("AKIA"));
    }

    // Revoke one.
    let resp = server
        .client
        .delete(format!("{}/v1/s3/credentials/{}", server.base_url, keys[0]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["deleted"],
        true
    );

    let body = get_json(&server, "/v1/s3/credentials").await;
    let remaining = body["items"].as_array().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["access_key_id"], keys[1]);
}
