//! With auth enabled, the filesystem xorb route must not serve bytes to
//! anyone who merely knows a xorb hash: a request needs a signed URL from a
//! reconstruction response (and a Range inside the signed span) or a bearer
//! token.

#[allow(dead_code)]
mod helpers;

use helpers::*;
use openxet_cas_types::reconstruction::QueryReconstructionResponseV2;

/// Upload a file and return its first signed fetch URL, the byte span it
/// covers, and the xorb hash.
async fn signed_fetch(server: &TestServer) -> (String, u64, u64, String) {
    let artifacts = build_upload_artifacts(&generate_test_data(200_000));
    upload_artifacts(server, &artifacts).await;

    let recon: QueryReconstructionResponseV2 = server
        .client
        .get(format!(
            "{}/v2/reconstructions/{}",
            server.base_url, artifacts.file_hash
        ))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (hash, fetches) = recon.xorbs.into_iter().next().unwrap();
    let fetch = fetches.into_iter().next().unwrap();
    let bytes = fetch.ranges[0].bytes;
    (fetch.url, bytes.start, bytes.end, hash)
}

async fn get(server: &TestServer, url: &str, range: Option<(u64, u64)>) -> reqwest::StatusCode {
    let mut req = server.client.get(url);
    if let Some((s, e)) = range {
        req = req.header("range", format!("bytes={s}-{e}"));
    }
    req.send().await.unwrap().status()
}

#[tokio::test]
async fn unsigned_xorb_get_is_401() {
    let server = TestServer::start().await;
    let (_, start, end, hash) = signed_fetch(&server).await;
    let bare = format!("{}/v1/xorbs/default/{hash}", server.base_url);

    assert_eq!(get(&server, &bare, None).await, 401);
    assert_eq!(get(&server, &bare, Some((start, end))).await, 401);
}

#[tokio::test]
async fn signed_url_serves_its_span() {
    let server = TestServer::start().await;
    let (url, start, end, _) = signed_fetch(&server).await;
    assert!(
        url.contains("sig="),
        "fallback fetch URLs must be signed: {url}"
    );

    assert_eq!(get(&server, &url, Some((start, end))).await, 206);
    // A sub-range of the signed span is fine (retries may narrow a request).
    assert_eq!(get(&server, &url, Some((start, start))).await, 206);
}

#[tokio::test]
async fn signed_url_does_not_extend_beyond_its_span() {
    let server = TestServer::start().await;
    let (url, start, end, _) = signed_fetch(&server).await;

    assert_eq!(get(&server, &url, None).await, 403);
    assert_eq!(get(&server, &url, Some((start, end + 1))).await, 403);
}

#[tokio::test]
async fn tampered_signed_url_is_401() {
    let server = TestServer::start().await;
    let (url, start, end, hash) = signed_fetch(&server).await;

    // Widen the signed span.
    let widened = url.replacen(
        &format!("r={start}-{end}"),
        &format!("r=0-{}", end + 1000),
        1,
    );
    assert_eq!(get(&server, &widened, Some((0, end + 1000))).await, 401);

    // Push the expiry out.
    let (prefix, rest) = url.split_once("&exp=").unwrap();
    let sig = rest.split_once("&sig=").unwrap().1;
    let extended = format!("{prefix}&exp=99999999999&sig={sig}");
    assert_eq!(get(&server, &extended, Some((start, end))).await, 401);

    // Reuse the signature for another xorb.
    let other = url.replacen(&hash, &"0".repeat(64), 1);
    assert_eq!(get(&server, &other, Some((start, end))).await, 401);

    // Forge a signature.
    let forged = format!("{prefix}&exp=99999999999&sig={}", "0".repeat(64));
    assert_eq!(get(&server, &forged, Some((start, end))).await, 401);
}

#[tokio::test]
async fn bearer_token_still_reads_xorbs() {
    let server = TestServer::start().await;
    let (_, _, _, hash) = signed_fetch(&server).await;
    let resp = server
        .client
        .get(format!("{}/v1/xorbs/default/{hash}", server.base_url))
        .bearer_auth(server.read_token())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn auth_disabled_serves_unsigned_xorbs() {
    let server = TestServer::start_with_auth_disabled().await;
    let (_, start, end, hash) = signed_fetch(&server).await;
    let bare = format!("{}/v1/xorbs/default/{hash}", server.base_url);
    assert_eq!(get(&server, &bare, Some((start, end))).await, 206);
}
