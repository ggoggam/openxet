use std::path::{Path, PathBuf};

use super::super::backend::validate_hash;
use super::super::error::StorageError;
use super::FileIndex;

/// File-based file index storing `file_hash → shard_hash` mappings as plain text files.
///
/// Layout: `{index_dir}/files/{file_hash}` contains the shard hash as a plain string.
pub struct FilesystemFileIndex {
    dir: PathBuf,
}

impl FilesystemFileIndex {
    /// Create a new file index, creating the directory if it doesn't exist.
    pub async fn new(data_dir: &Path) -> Result<Self, StorageError> {
        let dir = data_dir.join("index").join("files");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| StorageError::io(e, &dir))?;
        Ok(Self { dir })
    }
}

impl FileIndex for FilesystemFileIndex {
    async fn get(&self, file_hash: &str) -> Result<Option<String>, StorageError> {
        validate_hash(file_hash)?;
        let path = self.dir.join(file_hash);
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => Ok(Some(contents.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::io(e, path)),
        }
    }

    async fn put(&self, file_hash: &str, shard_hash: &str) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        validate_hash(shard_hash)?;
        let path = self.dir.join(file_hash);
        super::super::filesystem::atomic_write(&path, shard_hash.as_bytes()).await
    }

    async fn list_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(&self.dir)
            .await
            .map_err(|e| StorageError::io(e, &self.dir))?;

        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|e| StorageError::io(e, &self.dir))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if let Ok(contents) = tokio::fs::read_to_string(entry.path()).await {
                entries.push((name, contents.trim().to_string()));
            }
        }

        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
    const HASH_B: &str = "b1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
    const HASH_C: &str = "c1b2c3d4e5f60708091011121314151617181920212223242526272829303132";

    #[tokio::test]
    async fn test_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemFileIndex::new(dir.path()).await.unwrap();

        index.put(HASH_A, HASH_B).await.unwrap();
        let result = index.get(HASH_A).await.unwrap();
        assert_eq!(result, Some(HASH_B.to_string()));
    }

    #[tokio::test]
    async fn test_get_unknown_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemFileIndex::new(dir.path()).await.unwrap();

        let result = index.get(HASH_A).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemFileIndex::new(dir.path()).await.unwrap();

        index.put(HASH_A, HASH_B).await.unwrap();
        index.put(HASH_A, HASH_C).await.unwrap();

        let result = index.get(HASH_A).await.unwrap();
        assert_eq!(result, Some(HASH_C.to_string()));
    }

    #[tokio::test]
    async fn test_invalid_hash_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemFileIndex::new(dir.path()).await.unwrap();

        assert!(matches!(
            index.get("bad").await,
            Err(StorageError::InvalidHash(_))
        ));
        assert!(matches!(
            index.put("bad", HASH_A).await,
            Err(StorageError::InvalidHash(_))
        ));
        assert!(matches!(
            index.put(HASH_A, "bad").await,
            Err(StorageError::InvalidHash(_))
        ));
    }
}
