use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::io::AsyncReadExt;

use super::backend::{StorageBackend, validate_hash};
use super::error::StorageError;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Filesystem-backed storage for xorbs and shards.
///
/// Layout:
/// ```text
/// {data_dir}/
/// ├── xorbs/default/{hash}
/// └── shards/{hash}
/// ```
pub struct FilesystemBackend {
    xorb_dir: PathBuf,
    shard_dir: PathBuf,
}

impl FilesystemBackend {
    /// Create a new filesystem backend, creating directories if they don't exist.
    pub async fn new(data_dir: &Path) -> Result<Self, StorageError> {
        let xorb_dir = data_dir.join("xorbs").join("default");
        let shard_dir = data_dir.join("shards");

        tokio::fs::create_dir_all(&xorb_dir)
            .await
            .map_err(|e| StorageError::io(e, &xorb_dir))?;
        tokio::fs::create_dir_all(&shard_dir)
            .await
            .map_err(|e| StorageError::io(e, &shard_dir))?;

        Ok(Self {
            xorb_dir,
            shard_dir,
        })
    }
}

/// Atomically write data to `path` using a temp file + rename.
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

/// Read a complete file as `Bytes`.
async fn read_file(path: &Path) -> Result<Bytes, StorageError> {
    let data = tokio::fs::read(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StorageError::NotFound(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
            )
        } else {
            StorageError::io(e, path)
        }
    })?;
    Ok(Bytes::from(data))
}

/// Read a byte range `[start, end)` from a file.
async fn read_file_range(path: &Path, start: u64, end: u64) -> Result<Bytes, StorageError> {
    use tokio::io::AsyncSeekExt;

    let mut file = tokio::fs::File::open(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StorageError::NotFound(
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string(),
            )
        } else {
            StorageError::io(e, path)
        }
    })?;

    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| StorageError::io(e, path))?;

    let len = (end - start) as usize;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)
        .await
        .map_err(|e| StorageError::io(e, path))?;

    Ok(Bytes::from(buf))
}

/// Write data to `path` if the file doesn't already exist.
/// Returns `true` if newly written, `false` if it already existed.
async fn put_if_absent(path: &Path, data: &Bytes) -> Result<bool, StorageError> {
    if path.exists() {
        return Ok(false);
    }
    atomic_write(path, data).await?;
    Ok(true)
}

/// List files in a directory, returning (filename, file_size) pairs.
/// Skips hidden files and directories.
async fn list_dir_entries(dir: &Path) -> Result<Vec<(String, u64)>, StorageError> {
    let mut entries = Vec::new();
    let mut read_dir = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| StorageError::io(e, dir))?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| StorageError::io(e, dir))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            entries.push((name, meta.len()));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

impl StorageBackend for FilesystemBackend {
    async fn get_xorb(&self, hash: &str) -> Result<Bytes, StorageError> {
        validate_hash(hash)?;
        read_file(&self.xorb_dir.join(hash)).await
    }

    async fn get_xorb_range(
        &self,
        hash: &str,
        start: u64,
        end: u64,
    ) -> Result<Bytes, StorageError> {
        validate_hash(hash)?;
        read_file_range(&self.xorb_dir.join(hash), start, end).await
    }

    async fn put_xorb(&self, hash: &str, data: Bytes) -> Result<bool, StorageError> {
        validate_hash(hash)?;
        put_if_absent(&self.xorb_dir.join(hash), &data).await
    }

    async fn xorb_exists(&self, hash: &str) -> Result<bool, StorageError> {
        validate_hash(hash)?;
        Ok(self.xorb_dir.join(hash).exists())
    }

    async fn get_shard(&self, hash: &str) -> Result<Bytes, StorageError> {
        validate_hash(hash)?;
        read_file(&self.shard_dir.join(hash)).await
    }

    async fn put_shard(&self, hash: &str, data: Bytes) -> Result<bool, StorageError> {
        validate_hash(hash)?;
        put_if_absent(&self.shard_dir.join(hash), &data).await
    }

    async fn list_xorbs(&self) -> Result<Vec<(String, u64)>, StorageError> {
        list_dir_entries(&self.xorb_dir).await
    }

    async fn list_shards(&self) -> Result<Vec<(String, u64)>, StorageError> {
        list_dir_entries(&self.shard_dir).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HASH: &str = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";

    async fn make_backend() -> (tempfile::TempDir, FilesystemBackend) {
        let dir = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(dir.path()).await.unwrap();
        (dir, backend)
    }

    #[tokio::test]
    async fn test_put_get_xorb_roundtrip() {
        let (_dir, backend) = make_backend().await;
        let data = Bytes::from_static(b"hello xorb");

        let inserted = backend.put_xorb(TEST_HASH, data.clone()).await.unwrap();
        assert!(inserted);

        let retrieved = backend.get_xorb(TEST_HASH).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_put_xorb_idempotent() {
        let (_dir, backend) = make_backend().await;
        let data = Bytes::from_static(b"hello xorb");

        assert!(backend.put_xorb(TEST_HASH, data.clone()).await.unwrap());
        assert!(!backend.put_xorb(TEST_HASH, data).await.unwrap());
    }

    #[tokio::test]
    async fn test_xorb_exists() {
        let (_dir, backend) = make_backend().await;

        assert!(!backend.xorb_exists(TEST_HASH).await.unwrap());
        backend
            .put_xorb(TEST_HASH, Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert!(backend.xorb_exists(TEST_HASH).await.unwrap());
    }

    #[tokio::test]
    async fn test_get_xorb_not_found() {
        let (_dir, backend) = make_backend().await;
        let result = backend.get_xorb(TEST_HASH).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_get_xorb_range() {
        let (_dir, backend) = make_backend().await;
        let data = Bytes::from_static(b"0123456789abcdef");

        backend.put_xorb(TEST_HASH, data).await.unwrap();

        let range = backend.get_xorb_range(TEST_HASH, 4, 10).await.unwrap();
        assert_eq!(range.as_ref(), b"456789");
    }

    #[tokio::test]
    async fn test_put_get_shard_roundtrip() {
        let (_dir, backend) = make_backend().await;
        let data = Bytes::from_static(b"shard data");

        let inserted = backend.put_shard(TEST_HASH, data.clone()).await.unwrap();
        assert!(inserted);

        let retrieved = backend.get_shard(TEST_HASH).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_invalid_hash_rejected() {
        let (_dir, backend) = make_backend().await;

        assert!(matches!(
            backend.get_xorb("too_short").await,
            Err(StorageError::InvalidHash(_))
        ));
        assert!(matches!(
            backend.put_xorb("../etc/passwd", Bytes::new()).await,
            Err(StorageError::InvalidHash(_))
        ));
    }

    #[tokio::test]
    async fn test_directory_creation() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("nested").join("deep");

        let backend = FilesystemBackend::new(&data_dir).await.unwrap();
        assert!(data_dir.join("xorbs").join("default").exists());
        assert!(data_dir.join("shards").exists());

        // Should be functional
        backend
            .put_xorb(TEST_HASH, Bytes::from_static(b"test"))
            .await
            .unwrap();
        let result = backend.get_xorb(TEST_HASH).await.unwrap();
        assert_eq!(result.as_ref(), b"test");
    }
}
