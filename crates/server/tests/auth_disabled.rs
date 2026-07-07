#[allow(dead_code)]
mod helpers;

use helpers::TestServer;

/// With auth disabled, requests without any Authorization header must pass
/// token checks (a missing object is 404, never 401).
#[tokio::test]
async fn test_auth_disabled_allows_unauthenticated_requests() {
    let server = TestServer::start_with_auth_disabled().await;

    let hash = "0".repeat(64);
    let resp = server
        .client
        .get(format!("{}/v1/reconstructions/{hash}", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Write endpoint: garbage body should fail validation, not auth.
    let resp = server
        .client
        .post(format!("{}/v1/shards", server.base_url))
        .body("not a shard")
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), 401);
}
