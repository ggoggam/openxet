use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use xet_core_structures::merklehash::MerkleHash;
use xet_core_structures::xorb_object::constants::MAX_XORB_BYTES;

use crate::auth::fetch_url::{XorbUrlParams, now_unix, verify_xorb_url};
use crate::auth::middleware::authorize;
use crate::auth::{RequireRead, RequireWrite, Scope};
use crate::error::AppError;
use crate::pagination::{Page, clamp_limit, cursor_after};
use crate::routes::reconstruction::parse_range_header;
use crate::routes::xorb_meta::{layout_from_info, validated_xorb_info};
use crate::state::AppState;
use crate::storage::index::ChunkLocation;
use crate::storage::{ChunkIndex, StorageBackend, XorbSummary, validate_hash};

#[derive(Debug, Deserialize)]
pub struct ListXorbsParams {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

/// GET /v1/xorbs — cursor-paginated listing of indexed xorbs (those with a
/// recorded layout, i.e. every xorb that participates in dedup), ordered by
/// xorb hash. Unlike an object-store scan this is an ordered index range, so
/// pages are stable and each costs only its own rows.
pub async fn list_xorbs(
    State(state): State<AppState>,
    _auth: RequireRead,
    Query(params): Query<ListXorbsParams>,
) -> Result<Json<Page<XorbSummary>>, AppError> {
    let limit = clamp_limit(params.limit);
    let cursor = params.cursor.as_deref().filter(|s| !s.is_empty());
    let after = cursor_after(cursor)?;

    let rows = state
        .chunk_index
        .list_xorb_summaries(after.as_deref(), limit + 1)
        .await?;

    Ok(Json(Page::from_overfetched(rows, limit, |x| {
        x.xorb_hash.clone()
    })))
}

/// Serialized uploads may exceed the raw 64 MiB xorb content cap slightly:
/// per-chunk headers and the trailing metadata footer add overhead on top of
/// the (compressed) chunk payloads.
pub(crate) fn max_serialized_xorb_size() -> usize {
    *MAX_XORB_BYTES + 4 * 1024 * 1024
}

#[derive(Debug, Serialize)]
pub struct XorbUploadResponse {
    pub was_inserted: bool,
}

pub async fn post_xorb(
    State(state): State<AppState>,
    _auth: RequireWrite,
    Path(hash): Path<String>,
    body: Bytes,
) -> Result<Json<XorbUploadResponse>, AppError> {
    validate_hash(&hash)?;

    if body.len() > max_serialized_xorb_size() {
        return Err(AppError::PayloadTooLarge);
    }

    // Idempotent: if xorb already exists, return early
    if state.storage.xorb_exists(&hash).await? {
        return Ok(Json(XorbUploadResponse {
            was_inserted: false,
        }));
    }

    let expected = MerkleHash::from_hex(&hash)
        .map_err(|e| AppError::BadRequest(format!("invalid xorb hash: {e}")))?;

    // Recomputes every chunk hash from the (decompressed) data and checks the
    // aggregate against the URL hash — the core CAS invariant.
    let info = validated_xorb_info(&body, &expected)?;

    if info.num_chunks == 0 {
        return Err(AppError::BadRequest("xorb contains no chunks".to_string()));
    }

    // Record the layout before `body` moves into storage, so dedup and
    // reconstruction responses can be built from the index instead of
    // re-reading and re-parsing the xorb.
    let layout = layout_from_info(&info, body.len() as u32);

    // Store the xorb
    state.storage.put_xorb(&hash, body).await?;

    // Index all chunks in one batched write.
    let entries: Vec<(String, ChunkLocation)> = info
        .chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, chunk_hash)| {
            (
                chunk_hash.hex(),
                ChunkLocation {
                    xorb_hash: hash.clone(),
                    chunk_index: i as u32,
                },
            )
        })
        .collect();
    state.chunk_index.put_batch(entries).await?;

    state.chunk_index.put_xorb_layout(&hash, layout).await?;

    Ok(Json(XorbUploadResponse { was_inserted: true }))
}

/// GET /v1/xorbs/default/{hash} — download xorb data with optional Range header.
///
/// xet-core's download flow fetches xorb data from the URLs in the
/// reconstruction response's fetch_info entries **without** an Authorization
/// header (it treats them like presigned URLs). On backends that can't presign
/// (local filesystem), reconstruction therefore hands out this route with an
/// HMAC-signed, expiring query string scoped to one byte span; see
/// [`crate::auth::fetch_url`]. With auth enabled a request must carry either a
/// valid signature (and a Range inside the signed span) or a bearer token with
/// read scope. Cloud backends hand out presigned URLs that bypass this route
/// entirely, so on those backends it serves nothing and 404s.
pub async fn get_xorb(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Query(params): Query<XorbUrlParams>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    // On presign-capable backends clients fetch directly from object storage;
    // this route is only the filesystem fallback, so refuse to serve otherwise.
    if state.storage.supports_presigned_urls() {
        return Err(AppError::NotFound(format!(
            "xorb download not served by this backend: {hash}"
        )));
    }

    validate_hash(&hash)?;
    let range = parse_range_header(&headers)?;

    if state.config.auth.enabled {
        if params.is_signed() {
            let (signed_start, signed_end) =
                verify_xorb_url(&state.xorb_url_key, &hash, &params, now_unix())?;
            match range {
                Some((start, end)) if start >= signed_start && end <= signed_end => {}
                _ => {
                    return Err(AppError::Forbidden(format!(
                        "signed URL covers bytes {signed_start}-{signed_end} only; \
                         send a Range header within that span"
                    )));
                }
            }
        } else {
            authorize(&headers, &state, Scope::Read).await?;
        }
    }

    if let Some((start, end)) = range {
        // get_xorb_range uses exclusive end
        let data = state.storage.get_xorb_range(&hash, start, end + 1).await?;

        Ok((
            axum::http::StatusCode::PARTIAL_CONTENT,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        ))
    } else {
        let data = state.storage.get_xorb(&hash).await?;
        Ok((
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        ))
    }
}
