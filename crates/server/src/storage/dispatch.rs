use bytes::Bytes;

use super::backend::StorageBackend;
use super::error::StorageError;
use super::filesystem::FilesystemBackend;
use super::object_store_backend::ObjectStoreBackend;

/// Runtime-dispatched storage backend.
///
/// Wraps concrete backends in an enum to avoid making `AppState` and all
/// route handlers generic over the storage type.
pub enum StorageDispatch {
    Filesystem(FilesystemBackend),
    ObjectStore(ObjectStoreBackend),
}

impl StorageBackend for StorageDispatch {
    async fn get_xorb(&self, hash: &str) -> Result<Bytes, StorageError> {
        match self {
            Self::Filesystem(b) => b.get_xorb(hash).await,
            Self::ObjectStore(b) => b.get_xorb(hash).await,
        }
    }

    async fn get_xorb_range(
        &self,
        hash: &str,
        start: u64,
        end: u64,
    ) -> Result<Bytes, StorageError> {
        match self {
            Self::Filesystem(b) => b.get_xorb_range(hash, start, end).await,
            Self::ObjectStore(b) => b.get_xorb_range(hash, start, end).await,
        }
    }

    async fn put_xorb(&self, hash: &str, data: Bytes) -> Result<bool, StorageError> {
        match self {
            Self::Filesystem(b) => b.put_xorb(hash, data).await,
            Self::ObjectStore(b) => b.put_xorb(hash, data).await,
        }
    }

    async fn xorb_exists(&self, hash: &str) -> Result<bool, StorageError> {
        match self {
            Self::Filesystem(b) => b.xorb_exists(hash).await,
            Self::ObjectStore(b) => b.xorb_exists(hash).await,
        }
    }

    async fn get_shard(&self, hash: &str) -> Result<Bytes, StorageError> {
        match self {
            Self::Filesystem(b) => b.get_shard(hash).await,
            Self::ObjectStore(b) => b.get_shard(hash).await,
        }
    }

    async fn put_shard(&self, hash: &str, data: Bytes) -> Result<bool, StorageError> {
        match self {
            Self::Filesystem(b) => b.put_shard(hash, data).await,
            Self::ObjectStore(b) => b.put_shard(hash, data).await,
        }
    }

    async fn list_xorbs(&self) -> Result<Vec<(String, u64)>, StorageError> {
        match self {
            Self::Filesystem(b) => b.list_xorbs().await,
            Self::ObjectStore(b) => b.list_xorbs().await,
        }
    }

    async fn list_shards(&self) -> Result<Vec<(String, u64)>, StorageError> {
        match self {
            Self::Filesystem(b) => b.list_shards().await,
            Self::ObjectStore(b) => b.list_shards().await,
        }
    }
}
