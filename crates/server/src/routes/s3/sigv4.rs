//! AWS Signature Version 4 verification for the S3 gateway.
//!
//! Scope is deliberately narrow (see the plan): header-signed requests only
//! (`Authorization: AWS4-HMAC-SHA256 …`). Presigned-URL query authentication and
//! POST-policy uploads are out of scope and rejected rather than half-supported.
//! GET/HEAD carry no body, so the payload hash comes straight from the
//! `x-amz-content-sha256` header — no body buffering.
//!
//! When `auth.enabled` is false the whole check is skipped so
//! `aws s3 --no-sign-request` works for local verification, mirroring the
//! existing bearer-token bypass in [`crate::auth::middleware`].

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::request::Parts;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::state::AppState;

use super::error::S3Error;

type HmacSha256 = Hmac<Sha256>;

/// The authenticated identity for an S3 request: the accounting owner the
/// request acts as (anonymous requests, allowed only when auth is disabled,
/// act as `"default"`, matching [`crate::auth::jwt::Claims::owner`]).
pub struct S3Auth {
    #[allow(dead_code)] // owner is used by write paths in later phases
    pub owner_id: String,
}

impl FromRequestParts<AppState> for S3Auth {
    type Rejection = S3Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, S3Error> {
        if !state.config.auth.enabled {
            return Ok(S3Auth {
                owner_id: "default".to_string(),
            });
        }
        authorize(parts, state).await
    }
}

/// Fields parsed out of an `AWS4-HMAC-SHA256` Authorization header.
struct AuthHeader {
    access_key_id: String,
    date_stamp: String,
    region: String,
    service: String,
    signed_headers: String,
    signature: String,
}

fn parse_auth_header(value: &str) -> Option<AuthHeader> {
    let rest = value.strip_prefix("AWS4-HMAC-SHA256")?.trim_start();
    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for part in rest.split(',') {
        let part = part.trim();
        let (k, v) = part.split_once('=')?;
        match k {
            "Credential" => credential = Some(v.to_string()),
            "SignedHeaders" => signed_headers = Some(v.to_string()),
            "Signature" => signature = Some(v.to_string()),
            _ => {}
        }
    }
    // Credential = AKID/date/region/service/aws4_request
    let credential = credential?;
    let mut scope = credential.splitn(5, '/');
    let access_key_id = scope.next()?.to_string();
    let date_stamp = scope.next()?.to_string();
    let region = scope.next()?.to_string();
    let service = scope.next()?.to_string();
    let terminator = scope.next()?;
    if terminator != "aws4_request" {
        return None;
    }
    Some(AuthHeader {
        access_key_id,
        date_stamp,
        region,
        service,
        signed_headers: signed_headers?,
        signature: signature?,
    })
}

/// RFC 3986 encoding as AWS specifies for canonical strings: unreserved
/// characters pass through, everything else is percent-encoded uppercase.
/// `/` is preserved in path context (`encode_slash = false`) and encoded in
/// query context.
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Canonicalize the query string: URI-encode each name/value and sort. The
/// incoming query is already percent-encoded, so decode (via `form_urlencoded`)
/// then re-encode to AWS rules — idempotent for AWS-encoded input.
fn canonical_query_string(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (uri_encode(&k, true), uri_encode(&v, true)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Canonical header value: trim and collapse internal whitespace runs to a
/// single space (AWS rule for unquoted values).
fn canonical_header_value(v: &str) -> String {
    v.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Derive the SigV4 signing key from the secret and the credential scope.
fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// Compute the hex signature for a request. Split out from the extractor so it
/// can be unit-tested against AWS's published vectors.
#[allow(clippy::too_many_arguments)]
fn compute_signature(
    secret: &str,
    method: &str,
    canonical_uri: &str,
    canonical_query: &str,
    signed_headers: &str,
    canonical_headers: &str,
    hashed_payload: &str,
    amz_date: &str,
    date_stamp: &str,
    region: &str,
    service: &str,
) -> String {
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{hashed_payload}"
    );
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let key = signing_key(secret, date_stamp, region, service);
    hex::encode(hmac(&key, string_to_sign.as_bytes()))
}

/// Constant-time comparison of two byte slices.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut r = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        r |= x ^ y;
    }
    r == 0
}

/// Build the canonical-headers block from the request's headers, in the exact
/// order named by `signed_headers`. Returns `None` if a named header is absent.
fn build_canonical_headers(headers: &HeaderMap, signed_headers: &str) -> Option<String> {
    let mut out = String::new();
    for name in signed_headers.split(';') {
        let value = headers.get(name)?.to_str().ok()?;
        out.push_str(name);
        out.push(':');
        out.push_str(&canonical_header_value(value));
        out.push('\n');
    }
    Some(out)
}

async fn authorize(parts: &Parts, state: &AppState) -> Result<S3Auth, S3Error> {
    let auth_value = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::access_denied("missing Authorization header"))?;

    if !auth_value.starts_with("AWS4-HMAC-SHA256") {
        // Presigned (query) auth and other schemes are out of scope.
        return Err(S3Error::access_denied(
            "only AWS4-HMAC-SHA256 header authorization is supported",
        ));
    }

    let parsed = parse_auth_header(auth_value)
        .ok_or_else(|| S3Error::access_denied("malformed Authorization header"))?;

    let cred = state
        .s3_index
        .get_credential(&parsed.access_key_id)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?
        .ok_or_else(|| S3Error::access_denied("unknown access key id"))?;

    let amz_date = parts
        .headers
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::access_denied("missing x-amz-date header"))?;

    let hashed_payload = parts
        .headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD");

    let canonical_headers = build_canonical_headers(&parts.headers, &parsed.signed_headers)
        .ok_or_else(|| S3Error::access_denied("a signed header is missing from the request"))?;

    // For S3, the canonical URI is the request path as sent (single-encoded);
    // we mount the gateway on full `/{prefix}/…` routes (not `nest`), so
    // `uri.path()` is already the full path the client signed.
    let canonical_uri = parts.uri.path();
    let canonical_query = canonical_query_string(parts.uri.query());

    let expected = compute_signature(
        &cred.secret_key,
        parts.method.as_str(),
        canonical_uri,
        &canonical_query,
        &parsed.signed_headers,
        &canonical_headers,
        hashed_payload,
        amz_date,
        &parsed.date_stamp,
        &parsed.region,
        &parsed.service,
    );

    if !ct_eq(expected.as_bytes(), parsed.signature.as_bytes()) {
        return Err(S3Error::access_denied(
            "signature does not match; check your secret key and clock",
        ));
    }

    Ok(S3Auth {
        owner_id: cred.owner_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's published "GET Object" SigV4 example
    /// (docs.aws.amazon.com, "Authenticating Requests: Using the Authorization
    /// Header"). Validates our canonical-request, string-to-sign, and
    /// signing-key derivation against a known-good signature.
    #[test]
    fn aws_get_object_vector() {
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let canonical_headers = format!(
            "host:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\n\
             x-amz-content-sha256:{empty_hash}\nx-amz-date:20130524T000000Z\n"
        );
        let sig = compute_signature(
            secret,
            "GET",
            "/test.txt",
            "",
            "host;range;x-amz-content-sha256;x-amz-date",
            &canonical_headers,
            empty_hash,
            "20130524T000000Z",
            "20130524",
            "us-east-1",
            "s3",
        );
        assert_eq!(
            sig,
            "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn uri_encode_rules() {
        assert_eq!(uri_encode("/a b/c", false), "/a%20b/c");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("~-_.AZaz09", true), "~-_.AZaz09");
    }

    #[test]
    fn canonical_query_sorts_and_encodes() {
        assert_eq!(
            canonical_query_string(Some("prefix=foo/bar&list-type=2")),
            "list-type=2&prefix=foo%2Fbar"
        );
    }
}
