//! A caller only reads files it holds an ownership claim on; admins read
//! everything. Uploading a file (or identical content) records the claim.

#[allow(dead_code)]
mod helpers;

use helpers::*;

async fn status(server: &TestServer, path: &str, token: &str) -> reqwest::StatusCode {
    server
        .client
        .get(format!("{}{path}", server.base_url))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn other_users_cannot_read_a_file() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    let alice = server.write_token_for("alice");
    upload_artifacts_with_token(&server, &artifacts, &alice).await;
    let file = &artifacts.file_hash;

    for token in [server.write_token_for("bob"), server.read_token_for("bob")] {
        for path in [
            format!("/v1/reconstructions/{file}"),
            format!("/v2/reconstructions/{file}"),
            format!("/v1/files/{file}"),
        ] {
            // Indistinguishable from a file that does not exist.
            assert_eq!(status(&server, &path, &token).await, 404, "{path}");
        }
    }

    // The owner (with any scope) and an admin can.
    for token in [
        alice.clone(),
        server.read_token_for("alice"),
        server.admin_token(),
    ] {
        assert_eq!(
            status(&server, &format!("/v2/reconstructions/{file}"), &token).await,
            200
        );
        assert_eq!(
            status(&server, &format!("/v1/files/{file}"), &token).await,
            200
        );
    }
    assert_eq!(
        download_via_protocol_with_token(&server, file, &server.read_token_for("alice")).await,
        data
    );
}

#[tokio::test]
async fn uploading_identical_content_grants_a_claim() {
    let server = TestServer::start().await;
    let data = generate_test_data(128 * 1024);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts_with_token(&server, &artifacts, &server.write_token_for("alice")).await;

    let bob = server.write_token_for("bob");
    let path = format!("/v2/reconstructions/{}", artifacts.file_hash);
    assert_eq!(status(&server, &path, &bob).await, 404);
    upload_artifacts_with_token(&server, &artifacts, &bob).await;
    assert_eq!(status(&server, &path, &bob).await, 200);

    // Bob's view of the file detail lists only his own claim.
    let detail: serde_json::Value = server
        .client
        .get(format!(
            "{}/v1/files/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(&bob)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owners: Vec<&str> = detail["owners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["owner"].as_str().unwrap())
        .collect();
    assert_eq!(owners, ["bob"]);
}

#[tokio::test]
async fn file_listing_is_scoped_to_the_caller() {
    let server = TestServer::start().await;
    let a = build_upload_artifacts(&generate_test_data(64 * 1024));
    let b = build_upload_artifacts(&generate_test_data(96 * 1024));
    upload_artifacts_with_token(&server, &a, &server.write_token_for("alice")).await;
    upload_artifacts_with_token(&server, &b, &server.write_token_for("bob")).await;

    let list = |token: String, query: &'static str| {
        let server = &server;
        async move {
            let resp = server
                .client
                .get(format!("{}/v1/files{query}", server.base_url))
                .bearer_auth(token)
                .send()
                .await
                .unwrap();
            let status = resp.status();
            let hashes: Vec<String> = if status.is_success() {
                let page: serde_json::Value = resp.json().await.unwrap();
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["file_hash"].as_str().unwrap().to_string())
                    .collect()
            } else {
                Vec::new()
            };
            (status, hashes)
        }
    };

    let (s, files) = list(server.read_token_for("bob"), "").await;
    assert_eq!(s, 200);
    assert_eq!(files, std::slice::from_ref(&b.file_hash));

    let (s, files) = list(server.read_token_for("bob"), "?owner=bob").await;
    assert_eq!(s, 200);
    assert_eq!(files, std::slice::from_ref(&b.file_hash));

    let (s, _) = list(server.read_token_for("bob"), "?owner=alice").await;
    assert_eq!(s, 403);

    let (s, files) = list(server.admin_token(), "").await;
    assert_eq!(s, 200);
    assert_eq!(files.len(), 2);
}

#[tokio::test]
async fn cross_tenant_management_endpoints_are_admin_only() {
    let server = TestServer::start().await;
    for path in ["/v1/accounting", "/v1/xorbs"] {
        assert_eq!(
            status(&server, path, &server.write_token()).await,
            403,
            "{path}"
        );
        assert_eq!(
            status(&server, path, &server.admin_token()).await,
            200,
            "{path}"
        );
    }
}
