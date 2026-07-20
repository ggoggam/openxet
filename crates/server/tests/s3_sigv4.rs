//! SigV4 authentication for the S3 gateway (auth enabled).
//!
//! Signs a real GetObject request with an independently-implemented SigV4
//! signer and asserts the server accepts it — exercising the full path through
//! axum (route matching, canonical URI/query, credential lookup, comparison),
//! which the crypto-only unit vector in `routes::s3::sigv4` cannot. Also checks
//! that unsigned and mis-signed requests are rejected.

mod helpers;

use helpers::{TestServer, build_upload_artifacts, generate_test_data, upload_artifacts};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const REGION: &str = "us-east-1";

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Sign a GET request and return the `Authorization` header value. `host` is
/// the authority (host:port); `path` is the full request path (including the
/// `/s3` prefix). Uses the fixed timestamp `20240101T000000Z`.
fn sign_get(host: &str, path: &str) -> (String, String) {
    let amz_date = "20240101T000000Z";
    let date_stamp = "20240101";
    let empty_hash = sha256_hex(b"");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{empty_hash}\nx-amz-date:{amz_date}\n");
    let canonical_request =
        format!("GET\n{path}\n\n{canonical_headers}\n{signed_headers}\n{empty_hash}");
    let scope = format!("{date_stamp}/{REGION}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac(
        format!("AWS4{SECRET_KEY}").as_bytes(),
        date_stamp.as_bytes(),
    );
    let k_region = hmac(&k_date, REGION.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
    let auth = format!(
        "AWS4-HMAC-SHA256 Credential={ACCESS_KEY}/{scope}, \
         SignedHeaders={signed_headers}, Signature={signature}"
    );
    (auth, amz_date.to_string())
}

async fn setup() -> (TestServer, String) {
    let server = TestServer::start().await; // auth enabled
    server
        .seed_s3_credential(ACCESS_KEY, SECRET_KEY, "s3-user")
        .await;

    let data = generate_test_data(50_000);
    let artifacts = build_upload_artifacts(&data);
    upload_artifacts(&server, &artifacts).await;

    // Register the object (management endpoint uses the bearer token path).
    let resp = server
        .client
        .post(format!("{}/v1/s3/objects", server.base_url))
        .bearer_auth(server.write_token())
        .json(&json!({ "bucket": "bkt", "key": "k.bin", "file_hash": artifacts.file_hash }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "register failed");

    (server, hex::encode(data))
}

#[tokio::test]
async fn signed_request_is_accepted() {
    let (server, data_hex) = setup().await;
    let host = server.base_url.strip_prefix("http://").unwrap().to_string();
    let (auth, amz_date) = sign_get(&host, "/s3/bkt/k.bin");

    let resp = server
        .client
        .get(format!("{}/s3/bkt/k.bin", server.base_url))
        .header("host", &host)
        .header("x-amz-date", &amz_date)
        .header("x-amz-content-sha256", sha256_hex(b""))
        .header("authorization", auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "signed request rejected");
    assert_eq!(hex::encode(resp.bytes().await.unwrap()), data_hex);
}

#[tokio::test]
async fn unsigned_request_is_denied() {
    let (server, _) = setup().await;
    let resp = server
        .client
        .get(format!("{}/s3/bkt/k.bin", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(
        resp.text()
            .await
            .unwrap()
            .contains("<Code>AccessDenied</Code>")
    );
}

#[tokio::test]
async fn bad_signature_is_denied() {
    let (server, _) = setup().await;
    let host = server.base_url.strip_prefix("http://").unwrap().to_string();
    let (mut auth, amz_date) = sign_get(&host, "/s3/bkt/k.bin");
    // Corrupt the signature hex.
    auth.truncate(auth.len() - 4);
    auth.push_str("0000");

    let resp = server
        .client
        .get(format!("{}/s3/bkt/k.bin", server.base_url))
        .header("host", &host)
        .header("x-amz-date", &amz_date)
        .header("x-amz-content-sha256", sha256_hex(b""))
        .header("authorization", auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}
