//! S3-compatible read gateway (Phase 1).
//!
//! A path-addressed `(bucket, key)` surface layered over the content-addressed
//! CAS, so existing S3 tooling (`aws s3`, `boto3`, `s3fs`) can read files that
//! were uploaded via the Xet client. Read-only: GetObject, HeadObject,
//! HeadBucket, ListObjectsV2. Writes (PutObject, multipart, DeleteObject,
//! CopyObject) and per-bucket tenancy are later phases.
//!
//! The gateway is mounted on full `/{prefix}/…` routes (not `nest`) so the
//! request path the SigV4 verifier sees matches exactly what the client signed.

mod error;
mod get;
mod list;
mod put;
mod sigv4;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::auth::{RequireRead, RequireWrite};
use crate::error::AppError;
use crate::pagination::{Page, clamp_limit, cursor_after};
use crate::state::AppState;
use crate::storage::{
    BucketSummary, FileIndex, S3Credential, S3CredentialSummary, S3Object, validate_hash,
};

/// Current unix time in seconds (saturating to 0 before the epoch).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build the gateway routes under `prefix` (e.g. `/s3`). Path-style addressing:
/// clients point `--endpoint-url {public_url}{prefix}` at the server.
pub fn gateway_routes(prefix: &str) -> Router<AppState> {
    Router::new()
        .route(
            &format!("{prefix}/{{bucket}}"),
            get(list::list_objects_v2).head(list::head_bucket),
        )
        .route(
            &format!("{prefix}/{{bucket}}/{{*key}}"),
            get(get::get_object)
                .head(get::head_object)
                .put(put::put_object)
                .delete(put::delete_object),
        )
}

#[derive(Debug, Deserialize)]
pub struct RegisterObjectRequest {
    pub bucket: String,
    pub key: String,
    /// Hash of an already-uploaded file (via the Xet client) to expose.
    pub file_hash: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterObjectResponse {
    pub bucket: String,
    pub key: String,
    pub file_hash: String,
    pub size: u64,
    pub etag: String,
}

/// POST /v1/s3/objects — register a friendly `(bucket, key)` name for an
/// already-uploaded file. The Phase-1 stand-in for S3 PutObject: it maps a name
/// onto existing content rather than accepting bytes. Requires write scope; the
/// caller becomes the object's accounting owner.
pub async fn register_object(
    State(state): State<AppState>,
    RequireWrite(claims): RequireWrite,
    Json(req): Json<RegisterObjectRequest>,
) -> Result<Json<RegisterObjectResponse>, AppError> {
    validate_hash(&req.file_hash)?;
    if req.bucket.is_empty() || req.key.is_empty() {
        return Err(AppError::BadRequest("bucket and key are required".into()));
    }

    // The file must already exist in the index (uploaded via the Xet client).
    if state.file_index.get(&req.file_hash).await?.is_none() {
        return Err(AppError::NotFound(format!(
            "file not found: {}",
            req.file_hash
        )));
    }

    // Logical size comes from an ownership claim, as GET /v1/files/{id} does.
    let size = state
        .file_index
        .file_claims(&req.file_hash)
        .await?
        .first()
        .map(|c| c.logical_bytes)
        .unwrap_or(0);

    let now = now_unix();

    let obj = S3Object {
        bucket: req.bucket.clone(),
        key: req.key.clone(),
        file_hash: req.file_hash.clone(),
        size,
        etag: req.file_hash.clone(),
        owner_id: claims.owner().to_string(),
        last_modified: now,
    };
    state.s3_index.put_object(&obj).await?;

    Ok(Json(RegisterObjectResponse {
        bucket: obj.bucket,
        key: obj.key,
        file_hash: obj.file_hash,
        size: obj.size,
        etag: obj.etag,
    }))
}

#[derive(Debug, Serialize)]
pub struct CreateCredentialResponse {
    pub access_key_id: String,
    /// The secret is shown once, at creation, and stored hashed nowhere else to
    /// recover it from — the caller must capture it now.
    pub secret_access_key: String,
    pub owner_id: String,
}

/// POST /v1/s3/credentials — mint a SigV4 access-key/secret pair for the
/// caller's accounting owner, so `aws`/`boto3` clients can sign gateway
/// requests. Requires write scope on the native API; the minted credential then
/// authorizes S3 requests as that same owner.
pub async fn create_credential(
    State(state): State<AppState>,
    RequireWrite(claims): RequireWrite,
) -> Result<Json<CreateCredentialResponse>, AppError> {
    // AKIA-prefixed id and a 40-char secret, matching the shape S3 tooling
    // expects; both are random and opaque.
    let access_key_id = format!("AKIA{}", random_b32(16));
    let secret_access_key = random_secret(40);

    let cred = S3Credential {
        access_key_id: access_key_id.clone(),
        secret_key: secret_access_key.clone(),
        owner_id: claims.owner().to_string(),
    };
    state.s3_index.put_credential(&cred, now_unix()).await?;

    Ok(Json(CreateCredentialResponse {
        access_key_id,
        secret_access_key,
        owner_id: cred.owner_id,
    }))
}

// ---- management listing / deletion (native JSON API) ----------------------

#[derive(Debug, Serialize)]
pub struct GatewayInfoResponse {
    /// Whether the S3 data plane (GetObject/PutObject/…) is mounted. Management
    /// endpoints work regardless; when this is false, registered names are not
    /// reachable over the S3 protocol until the gateway is enabled.
    pub enabled: bool,
    /// The path prefix the gateway is mounted under, e.g. `/s3`.
    pub prefix: String,
    /// The endpoint URL S3 clients should target (`--endpoint-url`). Absolute
    /// when the server has a configured public URL; otherwise just the prefix,
    /// for the caller to resolve against its own origin.
    pub endpoint: String,
}

/// GET /v1/s3/info — gateway connection details for the management UI.
pub async fn gateway_info(
    State(state): State<AppState>,
    _auth: RequireRead,
) -> Result<Json<GatewayInfoResponse>, AppError> {
    let prefix = state.config.server.s3_gateway_prefix.clone();
    let endpoint = match &state.config.server.public_url {
        Some(base) => format!("{}{}", base.trim_end_matches('/'), prefix),
        None => prefix.clone(),
    };
    Ok(Json(GatewayInfoResponse {
        enabled: state.config.server.s3_gateway_enabled,
        prefix,
        endpoint,
    }))
}

#[derive(Debug, Serialize)]
pub struct ListBucketsResponse {
    pub buckets: Vec<BucketSummary>,
}

/// GET /v1/s3/buckets — distinct buckets with object counts and total size.
pub async fn list_buckets(
    State(state): State<AppState>,
    _auth: RequireRead,
) -> Result<Json<ListBucketsResponse>, AppError> {
    let buckets = state.s3_index.list_buckets().await?;
    Ok(Json(ListBucketsResponse { buckets }))
}

#[derive(Debug, Deserialize)]
pub struct ListObjectsParams {
    /// Bucket to list. Required — objects are always addressed within a bucket.
    pub bucket: String,
    /// Restrict to keys starting with this prefix.
    pub prefix: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

/// GET /v1/s3/objects — cursor-paginated object names within a bucket, ordered
/// by key. The management counterpart to the S3 ListObjectsV2 data-plane call.
pub async fn list_objects(
    State(state): State<AppState>,
    _auth: RequireRead,
    Query(params): Query<ListObjectsParams>,
) -> Result<Json<Page<S3Object>>, AppError> {
    if params.bucket.is_empty() {
        return Err(AppError::BadRequest("bucket is required".into()));
    }
    let limit = clamp_limit(params.limit);
    let prefix = params.prefix.as_deref().unwrap_or("");
    let cursor = params.cursor.as_deref().filter(|s| !s.is_empty());
    let after = cursor_after(cursor)?;

    // Over-fetch one row so the page knows whether a next one exists.
    let rows = state
        .s3_index
        .list_objects(&params.bucket, prefix, after.as_deref(), limit + 1)
        .await?;

    Ok(Json(Page::from_overfetched(rows, limit, |o| o.key.clone())))
}

#[derive(Debug, Deserialize)]
pub struct DeleteObjectParams {
    pub bucket: String,
    pub key: String,
}

#[derive(Debug, Serialize)]
pub struct DeleteObjectResponse {
    /// Whether a name was removed (false if no such (bucket, key) existed).
    pub deleted: bool,
}

/// DELETE /v1/s3/objects?bucket=&key= — remove an object name. Intentionally
/// does **not** release the underlying file's ownership claim, matching the S3
/// gateway's DeleteObject: one owner may name identical content under several
/// keys, so per-name refcounting is deferred and content is never orphaned here.
pub async fn delete_object(
    State(state): State<AppState>,
    _auth: RequireWrite,
    Query(params): Query<DeleteObjectParams>,
) -> Result<Json<DeleteObjectResponse>, AppError> {
    if params.bucket.is_empty() || params.key.is_empty() {
        return Err(AppError::BadRequest("bucket and key are required".into()));
    }
    let removed = state
        .s3_index
        .delete_object(&params.bucket, &params.key)
        .await?;
    Ok(Json(DeleteObjectResponse {
        deleted: removed.is_some(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ListCredentialsResponse {
    pub items: Vec<S3CredentialSummary>,
}

/// GET /v1/s3/credentials — all minted credentials without their secrets.
pub async fn list_credentials(
    State(state): State<AppState>,
    _auth: RequireRead,
) -> Result<Json<ListCredentialsResponse>, AppError> {
    let items = state.s3_index.list_credentials().await?;
    Ok(Json(ListCredentialsResponse { items }))
}

#[derive(Debug, Serialize)]
pub struct DeleteCredentialResponse {
    pub deleted: bool,
}

/// DELETE /v1/s3/credentials/{access_key_id} — revoke a credential.
pub async fn delete_credential(
    State(state): State<AppState>,
    _auth: RequireWrite,
    Path(access_key_id): Path<String>,
) -> Result<Json<DeleteCredentialResponse>, AppError> {
    let deleted = state.s3_index.delete_credential(&access_key_id).await?;
    Ok(Json(DeleteCredentialResponse { deleted }))
}

/// `n` uppercase base32-ish characters (A–Z, 2–7), for an access-key id.
fn random_b32(n: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    (0..n)
        .map(|_| ALPHABET[rand::random::<usize>() % ALPHABET.len()] as char)
        .collect()
}

/// `n` characters from the SigV4 secret alphabet (base64-url without padding).
fn random_secret(n: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    (0..n)
        .map(|_| ALPHABET[rand::random::<usize>() % ALPHABET.len()] as char)
        .collect()
}

// ---- date formatting (no chrono dependency) -------------------------------

/// Civil date `(year, month, day)` from a count of days since the Unix epoch,
/// via Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Split a Unix timestamp into `(year, month, day, hour, min, sec, weekday)`
/// in UTC. `weekday` is 0=Sun..6=Sat.
fn civil_parts(unix: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hour = (secs / 3600) as u32;
    let min = (secs % 3600 / 60) as u32;
    let sec = (secs % 60) as u32;
    // 1970-01-01 was a Thursday (=4).
    let weekday = (days.rem_euclid(7) + 4).rem_euclid(7) as u32;
    (y, m, d, hour, min, sec, weekday)
}

/// RFC 7231 IMF-fixdate, e.g. `Wed, 21 Oct 2015 07:28:00 GMT`. Used for the
/// `Last-Modified` header.
pub(super) fn format_http_date(unix: i64) -> String {
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss, wd) = civil_parts(unix);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WD[wd as usize],
        d,
        MON[(m - 1) as usize],
        y,
        hh,
        mm,
        ss
    )
}

/// ISO 8601 / RFC 3339 UTC, e.g. `2015-10-21T07:28:00.000Z`. Used for
/// `<LastModified>` in ListObjectsV2 responses.
pub(super) fn format_iso8601(unix: i64) -> String {
    let (y, m, d, hh, mm, ss, _) = civil_parts(unix);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.000Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formats_known_timestamp() {
        // 1445412480 = 2015-10-21T07:28:00Z (a Wednesday).
        assert_eq!(
            format_http_date(1_445_412_480),
            "Wed, 21 Oct 2015 07:28:00 GMT"
        );
        assert_eq!(format_iso8601(1_445_412_480), "2015-10-21T07:28:00.000Z");
    }

    #[test]
    fn date_formats_epoch() {
        assert_eq!(format_http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }
}
