use std::collections::BTreeSet;
use std::io::Cursor;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;

use crate::auth::{RequireRead, RequireWrite};
use crate::error::AppError;
use crate::pagination::{Page, clamp_limit, cursor_after};
use crate::state::AppState;
use crate::storage::{FileIndex, FileListEntry, OwnerClaim, StorageBackend, validate_hash};

#[derive(Debug, Deserialize)]
pub struct ListFilesParams {
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Restrict the listing to files this owner claims.
    pub owner: Option<String>,
}

/// GET /v1/files — cursor-paginated file listing, optionally filtered to one
/// owner's files. Ordered by file hash.
pub async fn list_files(
    State(state): State<AppState>,
    _auth: RequireRead,
    Query(params): Query<ListFilesParams>,
) -> Result<Json<Page<FileListEntry>>, AppError> {
    let limit = clamp_limit(params.limit);
    // Treat empty query params (`?owner=`, `?cursor=`) as absent, not as a
    // filter on the empty string.
    let cursor = params.cursor.as_deref().filter(|s| !s.is_empty());
    let owner = params.owner.as_deref().filter(|s| !s.is_empty());
    let after = cursor_after(cursor)?;

    // Over-fetch one row so the page knows whether a next one exists.
    let rows = state
        .file_index
        .list_files(after.as_deref(), owner, limit + 1)
        .await?;

    Ok(Json(Page::from_overfetched(rows, limit, |e| {
        e.file_hash.clone()
    })))
}

#[derive(Debug, Serialize)]
pub struct FileDetail {
    pub file_hash: String,
    pub shard_hash: String,
    /// Logical size from an ownership claim, or `0` if unclaimed.
    pub logical_bytes: u64,
    pub owners: Vec<OwnerClaim>,
    /// Distinct xorbs this file's reconstruction terms reference.
    pub xorbs: Vec<String>,
}

/// GET /v1/files/{file_id} — full detail for one file: its shard, ownership
/// claims, and the xorbs it depends on.
pub async fn get_file(
    State(state): State<AppState>,
    _auth: RequireRead,
    Path(file_id): Path<String>,
) -> Result<Json<FileDetail>, AppError> {
    validate_hash(&file_id)?;

    let shard_hash = state
        .file_index
        .get(&file_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("file not indexed: {file_id}")))?;

    let owners = state.file_index.file_claims(&file_id).await?;
    let logical_bytes = owners.first().map(|c| c.logical_bytes).unwrap_or(0);

    // Parse the shard to list the xorbs this specific file references.
    let bytes = state.storage.get_shard(&shard_hash).await?;
    let shard = MDBMinimalShard::from_reader(&mut Cursor::new(&bytes[..]), true, true)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt stored shard: {e}")))?;

    let mut xorbs = BTreeSet::new();
    for file_idx in 0..shard.num_files() {
        let file_view = shard.file(file_idx).expect("index in range");
        if file_view.file_hash().hex() != file_id {
            continue;
        }
        for term_idx in 0..file_view.num_entries() {
            xorbs.insert(file_view.entry(term_idx).xorb_hash.hex());
        }
    }

    Ok(Json(FileDetail {
        file_hash: file_id,
        shard_hash,
        logical_bytes,
        owners,
        xorbs: xorbs.into_iter().collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct DeleteFileResponse {
    /// Whether the file's index entry was removed (its last claim released).
    /// Bytes are reclaimed by the next GC pass, not immediately: chunks may be
    /// shared with other files.
    pub deleted: bool,
    /// Ownership claims still outstanding after this call.
    pub remaining_owners: u64,
}

/// DELETE /v1/files/{file_id} — release the caller's ownership claim on a
/// file. The file stays reconstructable while other owners still claim it;
/// releasing the last claim removes it from the index, making its exclusive
/// storage garbage for the next GC pass.
pub async fn delete_file(
    State(state): State<AppState>,
    RequireWrite(claims): RequireWrite,
    Path(file_id): Path<String>,
) -> Result<Json<DeleteFileResponse>, AppError> {
    validate_hash(&file_id)?;

    if state.file_index.get(&file_id).await?.is_none() {
        return Err(AppError::NotFound(format!("file not indexed: {file_id}")));
    }

    let owners = state.file_index.file_claims(&file_id).await?;

    // A file with no claims predates ownership accounting; any writer may
    // remove it (there is no owner to arbitrate).
    if owners.is_empty() {
        state.file_index.remove(&file_id).await?;
        return Ok(Json(DeleteFileResponse {
            deleted: true,
            remaining_owners: 0,
        }));
    }

    let released = state.file_index.release(claims.owner(), &file_id).await?;
    if !released {
        return Err(AppError::Forbidden(format!(
            "'{}' holds no claim on this file",
            claims.owner()
        )));
    }

    let remaining_owners = owners.len() as u64 - 1;
    if remaining_owners == 0 {
        state.file_index.remove(&file_id).await?;
    }

    Ok(Json(DeleteFileResponse {
        deleted: remaining_owners == 0,
        remaining_owners,
    }))
}
