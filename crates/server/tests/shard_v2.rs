//! POST /v2/shards: hf_xet >= 1.6 uploads shards here and expects an NDJSON
//! progress stream ending in a terminal `result` frame.

#[allow(dead_code)]
mod helpers;

use helpers::*;

async fn upload_xorbs(server: &TestServer, artifacts: &UploadArtifacts) {
    for (hash, data) in &artifacts.xorb_entries {
        let resp = server
            .client
            .post(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(server.write_token())
            .body(data.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }
}

#[tokio::test]
async fn v2_shard_upload_registers_files() {
    let server = TestServer::start().await;
    let data = generate_test_data(300_000);
    let artifacts = build_upload_artifacts(&data);
    upload_xorbs(&server, &artifacts).await;

    let resp = server
        .client
        .post(format!("{}/v2/shards", server.base_url))
        .bearer_auth(server.write_token())
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/x-ndjson"
    );
    let body = resp.text().await.unwrap();
    let last: serde_json::Value =
        serde_json::from_str(body.lines().rfind(|l| !l.is_empty()).unwrap()).unwrap();
    assert_eq!(last["type"], "result");

    assert_eq!(
        download_via_protocol(&server, &artifacts.file_hash).await,
        data
    );
}

#[tokio::test]
async fn v2_shard_upload_reports_validation_errors_as_status() {
    let server = TestServer::start().await;
    let artifacts = build_upload_artifacts(&generate_test_data(100_000));
    // Xorbs never uploaded: the shard references missing xorbs.
    let resp = server
        .client
        .post(format!("{}/v2/shards", server.base_url))
        .bearer_auth(server.write_token())
        .body(artifacts.shard_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn v2_shard_upload_requires_write() {
    let server = TestServer::start().await;
    let resp = server
        .client
        .post(format!("{}/v2/shards", server.base_url))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = server
        .client
        .post(format!("{}/v2/shards", server.base_url))
        .bearer_auth(server.read_token())
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}
