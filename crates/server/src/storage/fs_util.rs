use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::error::StorageError;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically write data to `path` using a temp file + rename.
///
/// Used by the local-filesystem metadata indexes (which are always backed by
/// the local disk regardless of the configured object-store backend).
pub(crate) async fn atomic_write(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_name = format!(".tmp.{}.{}", pid, counter);
    let tmp_path = path.with_file_name(tmp_name);

    tokio::fs::write(&tmp_path, data)
        .await
        .map_err(|e| StorageError::io(e, &tmp_path))?;

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| StorageError::io(e, path))?;

    Ok(())
}
