mod helpers;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

/// Helper to request content with an optional Range header.
async fn get_content(
    server: &TestServer,
    file_hash: &str,
    range: Option<&str>,
) -> reqwest::Response {
    let token = server.read_token();
    let mut req = server
        .client
        .get(format!("{}/v1/content/{file_hash}", server.base_url));
    req = req.bearer_auth(&token);
    if let Some(range) = range {
        req = req.header("range", range);
    }
    req.send().await.unwrap()
}

/// Full file download returns the original bytes.
#[tokio::test]
async fn test_full_content_download() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let resp = get_content(&server, &artifacts.file_hash, None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("accept-ranges")
            .unwrap()
            .to_str()
            .unwrap(),
        "bytes"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), data.as_slice());
}

/// Range request returns the correct byte slice with 206 status.
#[tokio::test]
async fn test_range_partial_content() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let start = 1000usize;
    let end = 2000usize;
    let resp = get_content(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes={start}-{end}")),
    )
    .await;
    assert_eq!(resp.status(), 206);

    // Check Content-Range header
    let content_range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, format!("bytes {start}-{end}/{}", data.len()));

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), &data[start..=end]);
}

/// Range past end of file returns 416.
#[tokio::test]
async fn test_range_past_end_416() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let past_end = data.len() as u64;
    let resp = get_content(
        &server,
        &artifacts.file_hash,
        Some(&format!("bytes={past_end}-{}", past_end + 100)),
    )
    .await;
    assert_eq!(resp.status(), 416);
}

/// Range with end beyond file size is clamped.
#[tokio::test]
async fn test_range_clamped_end() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let start = 100usize;
    let resp = get_content(&server, &artifacts.file_hash, Some("bytes=100-999999999")).await;
    assert_eq!(resp.status(), 206);

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), &data[start..]);
}

/// Content-Length header matches the returned body size.
#[tokio::test]
async fn test_content_length_header() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let resp = get_content(&server, &artifacts.file_hash, None).await;
    let content_length: usize = resp
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(content_length, data.len());

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), content_length);
}
