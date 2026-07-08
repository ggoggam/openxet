use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use crate::auth::{RequireRead, RequireWrite};
use crate::error::AppError;
use crate::gc::{GcReport, run_gc};
use crate::state::AppState;
use crate::storage::{FileIndex, OwnerUsage, StorageBackend};

#[derive(Debug, Deserialize)]
pub struct GcParams {
    /// Override the configured grace period for this pass. `0` collects
    /// everything unreachable regardless of age — only safe when no uploads
    /// are in flight.
    pub grace_seconds: Option<u64>,
}

/// POST /v1/gc — run one mark-and-sweep pass and return what it did.
pub async fn post_gc(
    State(state): State<AppState>,
    _auth: RequireWrite,
    Query(params): Query<GcParams>,
) -> Result<Json<GcReport>, AppError> {
    let grace = Duration::from_secs(
        params
            .grace_seconds
            .unwrap_or(state.config.gc.grace_seconds),
    );
    let report = run_gc(&state, grace).await?;
    Ok(Json(report))
}

#[derive(Debug, Serialize)]
pub struct AccountingResponse {
    /// Per-owner logical usage (every claimant is charged a file's full size).
    pub owners: Vec<OwnerUsage>,
    /// Files currently in the index, including unclaimed pre-accounting ones.
    pub files: u64,
    /// Distinct files with at least one ownership claim.
    pub claimed_files: u64,
    /// Logical bytes counting each claimed file once.
    pub unique_file_bytes: u64,
    pub xorb_count: u64,
    /// Stored (compressed, post-dedup) xorb bytes.
    pub physical_xorb_bytes: u64,
    pub shard_count: u64,
    pub physical_shard_bytes: u64,
    /// unique_file_bytes / physical_xorb_bytes — how much dedup+compression
    /// saves. 0 when nothing is stored.
    pub dedup_ratio: f64,
}

/// GET /v1/accounting — per-owner logical usage plus global physical stats.
pub async fn get_accounting(
    State(state): State<AppState>,
    _auth: RequireRead,
) -> Result<Json<AccountingResponse>, AppError> {
    let usage = state.file_index.usage().await?;
    let files = state.file_index.list_all().await?.len() as u64;
    let xorbs = state.storage.list_xorbs().await?;
    let shards = state.storage.list_shards().await?;

    let physical_xorb_bytes: u64 = xorbs.iter().map(|o| o.size).sum();
    let physical_shard_bytes: u64 = shards.iter().map(|o| o.size).sum();
    let dedup_ratio = if physical_xorb_bytes > 0 {
        usage.unique_file_bytes as f64 / physical_xorb_bytes as f64
    } else {
        0.0
    };

    Ok(Json(AccountingResponse {
        owners: usage.owners,
        files,
        claimed_files: usage.claimed_files,
        unique_file_bytes: usage.unique_file_bytes,
        xorb_count: xorbs.len() as u64,
        physical_xorb_bytes,
        shard_count: shards.len() as u64,
        physical_shard_bytes,
        dedup_ratio,
    }))
}
