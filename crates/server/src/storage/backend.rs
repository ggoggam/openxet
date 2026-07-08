use std::time::Duration;

use bytes::Bytes;

use super::error::StorageError;

/// Validates that a hash string is exactly 64 lowercase hex characters.
///
/// This also prevents path traversal attacks since hex characters cannot form `../`.
pub fn validate_hash(hash: &str) -> Result<(), StorageError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(StorageError::InvalidHash(hash.to_string()));
    }
    Ok(())
}

/// Metadata for one stored xorb or shard, as reported by the backing store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    /// 64-hex content hash (the object's filename).
    pub hash: String,
    /// Stored (serialized) size in bytes.
    pub size: u64,
    /// Upload time in unix seconds, from the store's own metadata. GC uses
    /// this for its grace period, so it must reflect when the object landed
    /// in *this* store, not any client-side timestamp.
    pub last_modified_unix: i64,
}

/// Trait for storing and retrieving xorb and shard blobs.
///
/// Implementations must be safe for concurrent use.
pub trait StorageBackend: Send + Sync {
    /// Retrieve a complete xorb by its hash.
    fn get_xorb(&self, hash: &str) -> impl Future<Output = Result<Bytes, StorageError>> + Send;

    /// Retrieve a byte range `[start, end)` from a xorb.
    fn get_xorb_range(
        &self,
        hash: &str,
        start: u64,
        end: u64,
    ) -> impl Future<Output = Result<Bytes, StorageError>> + Send;

    /// Store a xorb. Returns `true` if newly inserted, `false` if it already existed.
    fn put_xorb(
        &self,
        hash: &str,
        data: Bytes,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// Generate a presigned URL for downloading a xorb directly from the
    /// underlying object store, valid for `expires_in`. Cloud backends
    /// (S3/GCS/Azure) return `Some(url)`; backends that cannot presign (e.g.
    /// the local filesystem) return `None`, in which case the caller serves the
    /// xorb through the server's own public download route instead.
    fn presigned_xorb_url(
        &self,
        hash: &str,
        expires_in: Duration,
    ) -> impl Future<Output = Result<Option<String>, StorageError>> + Send;

    /// Check whether a xorb exists.
    fn xorb_exists(&self, hash: &str) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// Retrieve a complete shard by its hash.
    fn get_shard(&self, hash: &str) -> impl Future<Output = Result<Bytes, StorageError>> + Send;

    /// Store a shard. Returns `true` if newly inserted, `false` if it already existed.
    fn put_shard(
        &self,
        hash: &str,
        data: Bytes,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// Delete a xorb. Deleting a missing xorb is not an error.
    fn delete_xorb(&self, hash: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Delete a shard. Deleting a missing shard is not an error.
    fn delete_shard(&self, hash: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// List all xorbs with their stored size and upload time.
    fn list_xorbs(&self) -> impl Future<Output = Result<Vec<StoredObject>, StorageError>> + Send;

    /// List all shards with their stored size and upload time.
    fn list_shards(&self) -> impl Future<Output = Result<Vec<StoredObject>, StorageError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_hash_valid() {
        let valid = "a1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
        assert!(validate_hash(valid).is_ok());
    }

    #[test]
    fn test_validate_hash_all_digits() {
        let valid = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(validate_hash(valid).is_ok());
    }

    #[test]
    fn test_validate_hash_all_hex_letters() {
        let valid = "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd";
        assert!(validate_hash(valid).is_ok());
    }

    #[test]
    fn test_validate_hash_too_short() {
        assert!(validate_hash("abc123").is_err());
    }

    #[test]
    fn test_validate_hash_too_long() {
        let long = "a".repeat(65);
        assert!(validate_hash(&long).is_err());
    }

    #[test]
    fn test_validate_hash_uppercase() {
        let upper = "A1B2C3D4E5F60708091011121314151617181920212223242526272829303132";
        assert!(validate_hash(upper).is_err());
    }

    #[test]
    fn test_validate_hash_non_hex() {
        let bad = "g1b2c3d4e5f60708091011121314151617181920212223242526272829303132";
        assert!(validate_hash(bad).is_err());
    }

    #[test]
    fn test_validate_hash_empty() {
        assert!(validate_hash("").is_err());
    }

    #[test]
    fn test_validate_hash_path_traversal() {
        assert!(validate_hash("../../../etc/passwd").is_err());
    }
}
