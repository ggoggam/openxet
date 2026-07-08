//! Mark-and-sweep garbage collection.
//!
//! The file index is the root set: a file that has a `file_hash → shard_hash`
//! entry is live (entries are removed when a file's last ownership claim is
//! released). Marking parses every live shard and records the xorbs referenced
//! by live files' reconstruction terms. Sweeping deletes every stored xorb and
//! shard that was not marked — and, for xorbs, purges their chunk-index
//! entries and layouts so dedup responses stop advertising them.
//!
//! Objects newer than the grace period are never deleted: clients upload
//! xorbs first and register them in a shard afterwards, so a young
//! unreferenced xorb is usually an upload in flight, not garbage. The one
//! race this does not cover: a client that receives a dedup hit against a
//! xorb whose only referencing file is deleted mid-upload will fail shard
//! validation ("referenced xorb not found") and must re-upload without dedup.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;

use crate::state::AppState;
use crate::storage::{ChunkIndex, FileIndex, StorageBackend, StorageError};

/// What one GC pass found and did.
#[derive(Debug, Default, Serialize)]
pub struct GcReport {
    /// Files in the index (the root set).
    pub live_files: u64,
    /// Shards referenced by at least one live file.
    pub live_shards: u64,
    /// Xorbs referenced by live files' reconstruction terms.
    pub live_xorbs: u64,
    pub deleted_xorbs: u64,
    pub freed_xorb_bytes: u64,
    pub deleted_shards: u64,
    pub freed_shard_bytes: u64,
    /// Unreferenced objects left alone because they are younger than the
    /// grace period (likely uploads in flight).
    pub skipped_in_grace: u64,
}

/// Run one mark-and-sweep pass, deleting unreachable xorbs and shards older
/// than `grace`.
pub async fn run_gc(state: &AppState, grace: Duration) -> Result<GcReport, StorageError> {
    let mut report = GcReport::default();

    // -- Mark --------------------------------------------------------------
    let file_to_shard: HashMap<String, String> =
        state.file_index.list_all().await?.into_iter().collect();
    report.live_files = file_to_shard.len() as u64;

    let live_shards: HashSet<&String> = file_to_shard.values().collect();
    report.live_shards = live_shards.len() as u64;

    let mut reachable_xorbs: HashSet<String> = HashSet::new();
    for shard_hash in &live_shards {
        let bytes = match state.storage.get_shard(shard_hash).await {
            Ok(bytes) => bytes,
            Err(StorageError::NotFound(_)) => {
                // Dangling file entry: its shard is gone, so its files are
                // already unreconstructable. Skip rather than abort the pass;
                // nothing gets marked, nothing extra gets deleted.
                tracing::warn!(shard = %shard_hash, "gc: live file references missing shard");
                continue;
            }
            Err(e) => return Err(e),
        };
        let shard = MDBMinimalShard::from_reader(&mut Cursor::new(&bytes[..]), true, true)
            .map_err(|e| StorageError::Index(format!("corrupt stored shard {shard_hash}: {e}")))?;

        for file_idx in 0..shard.num_files() {
            let file_view = shard.file(file_idx).expect("index in range");
            let file_hash_hex = file_view.file_hash().hex();

            // Only entries of files that currently resolve to *this* shard
            // keep xorbs alive. A stale entry (file deleted, or re-registered
            // through a newer shard) must not pin storage.
            if file_to_shard.get(&file_hash_hex).map(String::as_str)
                != Some(shard_hash.as_str())
            {
                continue;
            }

            for term_idx in 0..file_view.num_entries() {
                reachable_xorbs.insert(file_view.entry(term_idx).xorb_hash.hex());
            }
        }
    }
    report.live_xorbs = reachable_xorbs.len() as u64;

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let in_grace =
        |last_modified_unix: i64| now_unix.saturating_sub(last_modified_unix) < grace.as_secs() as i64;

    // -- Sweep xorbs --------------------------------------------------------
    for object in state.storage.list_xorbs().await? {
        if reachable_xorbs.contains(&object.hash) {
            continue;
        }
        if in_grace(object.last_modified_unix) {
            report.skipped_in_grace += 1;
            continue;
        }
        // Index first: once the entries are gone no new shard can pass
        // validation against this xorb, so deleting the blob afterwards
        // cannot strand a fresh reference.
        state.chunk_index.remove_xorb(&object.hash).await?;
        state.storage.delete_xorb(&object.hash).await?;
        report.deleted_xorbs += 1;
        report.freed_xorb_bytes += object.size;
        tracing::info!(xorb = %object.hash, size = object.size, "gc: deleted unreachable xorb");
    }

    // -- Sweep shards -------------------------------------------------------
    for object in state.storage.list_shards().await? {
        if live_shards.contains(&object.hash) {
            continue;
        }
        if in_grace(object.last_modified_unix) {
            report.skipped_in_grace += 1;
            continue;
        }
        state.storage.delete_shard(&object.hash).await?;
        report.deleted_shards += 1;
        report.freed_shard_bytes += object.size;
        tracing::info!(shard = %object.hash, size = object.size, "gc: deleted unreachable shard");
    }

    tracing::info!(
        live_files = report.live_files,
        deleted_xorbs = report.deleted_xorbs,
        freed_xorb_bytes = report.freed_xorb_bytes,
        deleted_shards = report.deleted_shards,
        freed_shard_bytes = report.freed_shard_bytes,
        skipped_in_grace = report.skipped_in_grace,
        "gc pass complete"
    );
    Ok(report)
}
