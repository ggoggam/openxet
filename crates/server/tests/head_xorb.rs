mod helpers;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};

/// HEAD on an uploaded xorb reports its stored size as Content-Length plus
/// chunk count and unpacked size headers, with no body.
#[tokio::test]
async fn test_head_xorb_reports_metadata() {
    let server = TestServer::start().await;
    let data = generate_test_data(256 * 1024); // multiple CDC chunks
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let token = server.read_token();
    let mut total_chunks = 0u64;
    let mut total_unpacked = 0u64;

    for (hash, xorb_bytes) in &artifacts.xorb_entries {
        let resp = server
            .client
            .head(format!("{}/v1/xorbs/default/{hash}", server.base_url))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let header_u64 = |name: &str| -> u64 {
            resp.headers()
                .get(name)
                .unwrap_or_else(|| panic!("missing {name} header"))
                .to_str()
                .unwrap()
                .parse()
                .unwrap()
        };

        // Content-Length advertises the stored (serialized) size.
        assert_eq!(header_u64("content-length"), xorb_bytes.len() as u64);
        total_chunks += header_u64("x-xorb-num-chunks");
        total_unpacked += header_u64("x-xorb-unpacked-bytes");

        let body = resp.bytes().await.unwrap();
        assert!(body.is_empty(), "HEAD response must have no body");
    }

    assert_eq!(total_chunks, artifacts.chunk_hashes.len() as u64);
    // Every uploaded byte is accounted for across the xorbs' unpacked sizes.
    assert_eq!(total_unpacked, data.len() as u64);
}

/// HEAD is the pre-upload existence probe: unknown xorbs 404.
#[tokio::test]
async fn test_head_xorb_missing_is_404() {
    let server = TestServer::start().await;
    let token = server.read_token();

    let absent = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
    let resp = server
        .client
        .head(format!("{}/v1/xorbs/default/{absent}", server.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

/// HEAD validates the hash like every other xorb route.
#[tokio::test]
async fn test_head_xorb_invalid_hash_is_400() {
    let server = TestServer::start().await;
    let token = server.read_token();

    let resp = server
        .client
        .head(format!("{}/v1/xorbs/default/not-a-hash", server.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

/// Unlike GET (which stands in for presigned URLs), HEAD answers from the
/// index on any backend and therefore requires read auth.
#[tokio::test]
async fn test_head_xorb_requires_auth() {
    let server = TestServer::start().await;
    let data = generate_test_data(64 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    let (hash, _) = &artifacts.xorb_entries[0];
    let resp = server
        .client
        .head(format!("{}/v1/xorbs/default/{hash}", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}
