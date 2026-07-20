//! ListObjectsV2 and HeadBucket.
//!
//! Buckets are implicit in this gateway: any name is a valid namespace (there
//! is no separate bucket registry), so HeadBucket always succeeds and listing
//! just filters `s3_objects` by the requested bucket. Delimiter grouping into
//! `CommonPrefixes` is done in Rust over one keyset page — a Phase-1
//! approximation: truncation is decided by rows fetched, so a page that
//! collapses many keys under one prefix still counts them toward the page size.

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use std::collections::BTreeSet;

use crate::state::AppState;
use crate::storage::S3Object;

use super::error::{S3Error, xml_escape};
use super::format_iso8601;
use super::sigv4::S3Auth;

/// Default and maximum number of keys returned per ListObjectsV2 page.
const DEFAULT_MAX_KEYS: usize = 1000;
const MAX_MAX_KEYS: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(rename = "prefix")]
    prefix: Option<String>,
    #[serde(rename = "delimiter")]
    delimiter: Option<String>,
    #[serde(rename = "max-keys")]
    max_keys: Option<usize>,
    #[serde(rename = "continuation-token")]
    continuation_token: Option<String>,
    #[serde(rename = "start-after")]
    start_after: Option<String>,
    #[serde(rename = "encoding-type")]
    encoding_type: Option<String>,
}

/// HEAD /{prefix}/{bucket} — buckets are implicit, so this always succeeds.
pub async fn head_bucket(
    _state: State<AppState>,
    _auth: S3Auth,
    Path(_bucket): Path<String>,
) -> Result<Response, S3Error> {
    Ok(StatusCode::OK.into_response())
}

/// GET /{prefix}/{bucket}[?list-type=2&…] — list objects, grouping by delimiter.
pub async fn list_objects_v2(
    State(state): State<AppState>,
    _auth: S3Auth,
    Path(bucket): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Response, S3Error> {
    let prefix = params.prefix.unwrap_or_default();
    let delimiter = params.delimiter.filter(|d| !d.is_empty());
    let max_keys = params
        .max_keys
        .unwrap_or(DEFAULT_MAX_KEYS)
        .clamp(1, MAX_MAX_KEYS);
    let url_encode = params.encoding_type.as_deref() == Some("url");

    // Continuation token wins over start-after; both are keyset cursors on key.
    let after: Option<String> = match &params.continuation_token {
        Some(tok) => Some(decode_token(tok)?),
        None => params.start_after.clone(),
    };

    // Over-fetch one row to detect truncation.
    let rows = state
        .s3_index
        .list_objects(&bucket, &prefix, after.as_deref(), max_keys + 1)
        .await
        .map_err(|e| S3Error::internal(e.to_string()))?;

    let is_truncated = rows.len() > max_keys;
    let page: Vec<S3Object> = rows.into_iter().take(max_keys).collect();
    let next_token = if is_truncated {
        page.last().map(|o| encode_token(&o.key))
    } else {
        None
    };

    // Delimiter grouping: keys with a delimiter after the prefix collapse into
    // a CommonPrefix; the rest are Contents.
    let mut contents: Vec<S3Object> = Vec::new();
    let mut common_prefixes: BTreeSet<String> = BTreeSet::new();
    for obj in page {
        if let Some(delim) = &delimiter {
            let rest = &obj.key[prefix.len().min(obj.key.len())..];
            if let Some(idx) = rest.find(delim.as_str()) {
                let cp = format!("{}{}", prefix, &rest[..idx + delim.len()]);
                common_prefixes.insert(cp);
                continue;
            }
        }
        contents.push(obj);
    }

    let key_count = contents.len() + common_prefixes.len();
    let body = render_xml(RenderCtx {
        bucket: &bucket,
        prefix: &prefix,
        delimiter: delimiter.as_deref(),
        max_keys,
        key_count,
        is_truncated,
        continuation_token: params.continuation_token.as_deref(),
        next_token: next_token.as_deref(),
        start_after: params.start_after.as_deref(),
        url_encode,
        contents: &contents,
        common_prefixes: &common_prefixes,
    });

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        body,
    )
        .into_response())
}

struct RenderCtx<'a> {
    bucket: &'a str,
    prefix: &'a str,
    delimiter: Option<&'a str>,
    max_keys: usize,
    key_count: usize,
    is_truncated: bool,
    continuation_token: Option<&'a str>,
    next_token: Option<&'a str>,
    start_after: Option<&'a str>,
    url_encode: bool,
    contents: &'a [S3Object],
    common_prefixes: &'a BTreeSet<String>,
}

fn render_xml(ctx: RenderCtx) -> String {
    // Encode a value for output: percent-encode when encoding-type=url was
    // requested, then XML-escape regardless.
    let enc = |s: &str| -> String {
        if ctx.url_encode {
            xml_escape(&percent_encode(s))
        } else {
            xml_escape(s)
        }
    };

    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    xml.push_str("<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    xml.push_str(&format!("<Name>{}</Name>", xml_escape(ctx.bucket)));
    xml.push_str(&format!("<Prefix>{}</Prefix>", enc(ctx.prefix)));
    xml.push_str(&format!("<KeyCount>{}</KeyCount>", ctx.key_count));
    xml.push_str(&format!("<MaxKeys>{}</MaxKeys>", ctx.max_keys));
    if let Some(d) = ctx.delimiter {
        xml.push_str(&format!("<Delimiter>{}</Delimiter>", enc(d)));
    }
    if ctx.url_encode {
        xml.push_str("<EncodingType>url</EncodingType>");
    }
    xml.push_str(&format!("<IsTruncated>{}</IsTruncated>", ctx.is_truncated));
    if let Some(t) = ctx.continuation_token {
        xml.push_str(&format!(
            "<ContinuationToken>{}</ContinuationToken>",
            xml_escape(t)
        ));
    }
    if let Some(t) = ctx.next_token {
        xml.push_str(&format!(
            "<NextContinuationToken>{}</NextContinuationToken>",
            xml_escape(t)
        ));
    }
    if let Some(s) = ctx.start_after {
        xml.push_str(&format!("<StartAfter>{}</StartAfter>", enc(s)));
    }
    for obj in ctx.contents {
        xml.push_str("<Contents>");
        xml.push_str(&format!("<Key>{}</Key>", enc(&obj.key)));
        xml.push_str(&format!(
            "<LastModified>{}</LastModified>",
            format_iso8601(obj.last_modified)
        ));
        xml.push_str(&format!(
            "<ETag>&quot;{}&quot;</ETag>",
            xml_escape(&obj.etag)
        ));
        xml.push_str(&format!("<Size>{}</Size>", obj.size));
        xml.push_str("<StorageClass>STANDARD</StorageClass>");
        xml.push_str("</Contents>");
    }
    for cp in ctx.common_prefixes {
        xml.push_str(&format!(
            "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
            enc(cp)
        ));
    }
    xml.push_str("</ListBucketResult>");
    xml
}

/// Opaque continuation token: base64 of the last key returned. The client
/// treats it as opaque; we only need it to round-trip a keyset cursor.
fn encode_token(key: &str) -> String {
    BASE64.encode(key.as_bytes())
}

fn decode_token(token: &str) -> Result<String, S3Error> {
    let bytes = BASE64.decode(token.as_bytes()).map_err(|_| {
        S3Error::new(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "bad continuation-token",
        )
    })?;
    String::from_utf8(bytes).map_err(|_| {
        S3Error::new(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "bad continuation-token",
        )
    })
}

/// RFC 3986 percent-encoding for `encoding-type=url` output (`/` encoded).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(key: &str) -> S3Object {
        S3Object {
            bucket: "b".into(),
            key: key.into(),
            file_hash: "00".repeat(32),
            size: 3,
            etag: "00".repeat(32),
            owner_id: "default".into(),
            last_modified: 1_600_000_000,
        }
    }

    #[test]
    fn token_roundtrip() {
        let t = encode_token("some/key.txt");
        assert_eq!(decode_token(&t).unwrap(), "some/key.txt");
    }

    #[test]
    fn xml_lists_contents_and_common_prefixes() {
        let contents = vec![obj("a.txt")];
        let mut cps = BTreeSet::new();
        cps.insert("dir/".to_string());
        let xml = render_xml(RenderCtx {
            bucket: "b",
            prefix: "",
            delimiter: Some("/"),
            max_keys: 1000,
            key_count: 2,
            is_truncated: false,
            continuation_token: None,
            next_token: None,
            start_after: None,
            url_encode: false,
            contents: &contents,
            common_prefixes: &cps,
        });
        assert!(xml.contains("<Key>a.txt</Key>"));
        assert!(xml.contains("<CommonPrefixes><Prefix>dir/</Prefix></CommonPrefixes>"));
        assert!(xml.contains("<KeyCount>2</KeyCount>"));
        assert!(xml.contains("<ETag>&quot;"));
    }
}
