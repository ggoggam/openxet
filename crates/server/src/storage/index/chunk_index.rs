use std::path::{Path, PathBuf};

use super::super::backend::validate_hash;
use super::super::error::StorageError;
use super::{ChunkIndex, ChunkLocation};

/// File-based chunk index storing `chunk_hash → Vec<ChunkLocation>` mappings as JSON files.
///
/// Layout: `{index_dir}/chunks/{chunk_hash}` contains a JSON array of `ChunkLocation`.
pub struct FilesystemChunkIndex {
    dir: PathBuf,
}

impl FilesystemChunkIndex {
    /// Create a new chunk index, creating the directory if it doesn't exist.
    pub async fn new(data_dir: &Path) -> Result<Self, StorageError> {
        let dir = data_dir.join("index").join("chunks");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| StorageError::io(e, &dir))?;
        Ok(Self { dir })
    }
}

impl ChunkIndex for FilesystemChunkIndex {
    async fn get(&self, chunk_hash: &str) -> Result<Vec<ChunkLocation>, StorageError> {
        validate_hash(chunk_hash)?;
        let path = self.dir.join(chunk_hash);
        match tokio::fs::read(&path).await {
            Ok(data) => {
                let locations: Vec<ChunkLocation> = serde_json::from_slice(&data).map_err(|e| {
                    StorageError::io(
                        std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                        &path,
                    )
                })?;
                Ok(locations)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(StorageError::io(e, path)),
        }
    }

    async fn put(&self, chunk_hash: &str, location: ChunkLocation) -> Result<(), StorageError> {
        validate_hash(chunk_hash)?;
        let path = self.dir.join(chunk_hash);

        // Read existing locations (if any)
        let mut locations = match tokio::fs::read(&path).await {
            Ok(data) => serde_json::from_slice::<Vec<ChunkLocation>>(&data).map_err(|e| {
                StorageError::io(
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                    &path,
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(StorageError::io(e, &path)),
        };

        // Deduplicate: don't add if already present
        if !locations.contains(&location) {
            locations.push(location);
        }

        let json = serde_json::to_vec(&locations).map_err(|e| {
            StorageError::io(
                std::io::Error::new(std::io::ErrorKind::InvalidData, e),
                &path,
            )
        })?;

        super::super::filesystem::atomic_write(&path, &json).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK_HASH: &str = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
    const XORB_HASH_1: &str = "b1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
    const XORB_HASH_2: &str = "c1b2c3d4e5f60708091011121314151617181920212223242526272829303132";

    #[tokio::test]
    async fn test_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemChunkIndex::new(dir.path()).await.unwrap();

        let loc = ChunkLocation {
            xorb_hash: XORB_HASH_1.to_string(),
            chunk_index: 5,
        };
        index.put(CHUNK_HASH, loc.clone()).await.unwrap();

        let result = index.get(CHUNK_HASH).await.unwrap();
        assert_eq!(result, vec![loc]);
    }

    #[tokio::test]
    async fn test_get_unknown_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemChunkIndex::new(dir.path()).await.unwrap();

        let result = index.get(CHUNK_HASH).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_multiple_locations() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemChunkIndex::new(dir.path()).await.unwrap();

        let loc1 = ChunkLocation {
            xorb_hash: XORB_HASH_1.to_string(),
            chunk_index: 0,
        };
        let loc2 = ChunkLocation {
            xorb_hash: XORB_HASH_2.to_string(),
            chunk_index: 3,
        };

        index.put(CHUNK_HASH, loc1.clone()).await.unwrap();
        index.put(CHUNK_HASH, loc2.clone()).await.unwrap();

        let result = index.get(CHUNK_HASH).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.contains(&loc1));
        assert!(result.contains(&loc2));
    }

    #[tokio::test]
    async fn test_dedup_same_location() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemChunkIndex::new(dir.path()).await.unwrap();

        let loc = ChunkLocation {
            xorb_hash: XORB_HASH_1.to_string(),
            chunk_index: 0,
        };

        index.put(CHUNK_HASH, loc.clone()).await.unwrap();
        index.put(CHUNK_HASH, loc.clone()).await.unwrap();

        let result = index.get(CHUNK_HASH).await.unwrap();
        assert_eq!(result, vec![loc]);
    }

    #[tokio::test]
    async fn test_invalid_hash_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let index = FilesystemChunkIndex::new(dir.path()).await.unwrap();

        assert!(matches!(
            index.get("bad").await,
            Err(StorageError::InvalidHash(_))
        ));

        let loc = ChunkLocation {
            xorb_hash: XORB_HASH_1.to_string(),
            chunk_index: 0,
        };
        assert!(matches!(
            index.put("bad", loc).await,
            Err(StorageError::InvalidHash(_))
        ));
    }
}
