use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;

use super::backend::{StorageBackend, validate_hash};
use super::error::StorageError;

/// Object-store-backed storage for xorbs and shards.
///
/// Uses the `object_store` crate to support S3, GCS, Azure Blob Storage, and
/// any other backend that implements the `ObjectStore` trait.
///
/// Layout (object key prefixes):
/// ```text
/// xorbs/default/{hash}
/// shards/{hash}
/// ```
pub struct ObjectStoreBackend {
    store: Arc<dyn ObjectStore>,
}

impl ObjectStoreBackend {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self { store }
    }

    fn xorb_path(hash: &str) -> ObjectPath {
        ObjectPath::from(format!("xorbs/default/{hash}"))
    }

    fn shard_path(hash: &str) -> ObjectPath {
        ObjectPath::from(format!("shards/{hash}"))
    }
}

fn map_error(err: object_store::Error) -> StorageError {
    match err {
        object_store::Error::NotFound { path, .. } => StorageError::NotFound(path),
        other => StorageError::ObjectStore(other.to_string()),
    }
}

/// List objects under a prefix, returning (filename, size) pairs.
async fn list_prefix(
    store: &dyn ObjectStore,
    prefix: &ObjectPath,
) -> Result<Vec<(String, u64)>, StorageError> {
    use futures::TryStreamExt;

    let mut entries = Vec::new();
    let mut stream = store.list(Some(prefix));

    while let Some(meta) = stream.try_next().await.map_err(map_error)? {
        let name = meta.location.filename().unwrap_or_default().to_string();
        if !name.is_empty() && !name.starts_with('.') {
            entries.push((name, meta.size as u64));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

impl StorageBackend for ObjectStoreBackend {
    async fn get_xorb(&self, hash: &str) -> Result<Bytes, StorageError> {
        validate_hash(hash)?;
        let result = self
            .store
            .get(&Self::xorb_path(hash))
            .await
            .map_err(map_error)?;
        result.bytes().await.map_err(map_error)
    }

    async fn get_xorb_range(
        &self,
        hash: &str,
        start: u64,
        end: u64,
    ) -> Result<Bytes, StorageError> {
        validate_hash(hash)?;
        let range = Range {
            start: start as usize,
            end: end as usize,
        };
        self.store
            .get_range(&Self::xorb_path(hash), range)
            .await
            .map_err(map_error)
    }

    async fn put_xorb(&self, hash: &str, data: Bytes) -> Result<bool, StorageError> {
        validate_hash(hash)?;
        let path = Self::xorb_path(hash);
        let exists = self.store.head(&path).await.is_ok();
        if exists {
            return Ok(false);
        }
        self.store
            .put(&path, data.into())
            .await
            .map_err(map_error)?;
        Ok(true)
    }

    async fn xorb_exists(&self, hash: &str) -> Result<bool, StorageError> {
        validate_hash(hash)?;
        match self.store.head(&Self::xorb_path(hash)).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(map_error(e)),
        }
    }

    async fn get_shard(&self, hash: &str) -> Result<Bytes, StorageError> {
        validate_hash(hash)?;
        let result = self
            .store
            .get(&Self::shard_path(hash))
            .await
            .map_err(map_error)?;
        result.bytes().await.map_err(map_error)
    }

    async fn put_shard(&self, hash: &str, data: Bytes) -> Result<bool, StorageError> {
        validate_hash(hash)?;
        let path = Self::shard_path(hash);
        let exists = self.store.head(&path).await.is_ok();
        if exists {
            return Ok(false);
        }
        self.store
            .put(&path, data.into())
            .await
            .map_err(map_error)?;
        Ok(true)
    }

    async fn list_xorbs(&self) -> Result<Vec<(String, u64)>, StorageError> {
        list_prefix(&*self.store, &ObjectPath::from("xorbs/default")).await
    }

    async fn list_shards(&self) -> Result<Vec<(String, u64)>, StorageError> {
        list_prefix(&*self.store, &ObjectPath::from("shards")).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    const TEST_HASH: &str = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
    const TEST_HASH_2: &str = "b1b2c3d4e5f60708091011121314151617181920212223242526272829303132";

    fn make_backend() -> ObjectStoreBackend {
        ObjectStoreBackend::new(Arc::new(InMemory::new()))
    }

    #[tokio::test]
    async fn test_put_get_xorb_roundtrip() {
        let backend = make_backend();
        let data = Bytes::from_static(b"hello xorb");

        let inserted = backend.put_xorb(TEST_HASH, data.clone()).await.unwrap();
        assert!(inserted);

        let retrieved = backend.get_xorb(TEST_HASH).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_put_xorb_idempotent() {
        let backend = make_backend();
        let data = Bytes::from_static(b"hello xorb");

        assert!(backend.put_xorb(TEST_HASH, data.clone()).await.unwrap());
        assert!(!backend.put_xorb(TEST_HASH, data).await.unwrap());
    }

    #[tokio::test]
    async fn test_xorb_exists() {
        let backend = make_backend();

        assert!(!backend.xorb_exists(TEST_HASH).await.unwrap());
        backend
            .put_xorb(TEST_HASH, Bytes::from_static(b"data"))
            .await
            .unwrap();
        assert!(backend.xorb_exists(TEST_HASH).await.unwrap());
    }

    #[tokio::test]
    async fn test_get_xorb_not_found() {
        let backend = make_backend();
        let result = backend.get_xorb(TEST_HASH).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_get_xorb_range() {
        let backend = make_backend();
        let data = Bytes::from_static(b"0123456789abcdef");

        backend.put_xorb(TEST_HASH, data).await.unwrap();

        let range = backend.get_xorb_range(TEST_HASH, 4, 10).await.unwrap();
        assert_eq!(range.as_ref(), b"456789");
    }

    #[tokio::test]
    async fn test_put_get_shard_roundtrip() {
        let backend = make_backend();
        let data = Bytes::from_static(b"shard data");

        let inserted = backend.put_shard(TEST_HASH, data.clone()).await.unwrap();
        assert!(inserted);

        let retrieved = backend.get_shard(TEST_HASH).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_invalid_hash_rejected() {
        let backend = make_backend();

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
    async fn test_list_xorbs() {
        let backend = make_backend();

        backend
            .put_xorb(TEST_HASH, Bytes::from_static(b"data1"))
            .await
            .unwrap();
        backend
            .put_xorb(TEST_HASH_2, Bytes::from_static(b"data2"))
            .await
            .unwrap();

        let xorbs = backend.list_xorbs().await.unwrap();
        assert_eq!(xorbs.len(), 2);
        assert_eq!(xorbs[0].0, TEST_HASH);
        assert_eq!(xorbs[0].1, 5);
        assert_eq!(xorbs[1].0, TEST_HASH_2);
        assert_eq!(xorbs[1].1, 5);
    }

    #[tokio::test]
    async fn test_list_shards() {
        let backend = make_backend();

        backend
            .put_shard(TEST_HASH, Bytes::from_static(b"shard1"))
            .await
            .unwrap();

        let shards = backend.list_shards().await.unwrap();
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].0, TEST_HASH);
        assert_eq!(shards[0].1, 6);
    }
}
