pub mod rocksdb_index;

use serde::{Deserialize, Serialize};

use super::error::StorageError;

pub use rocksdb_index::{RocksDbChunkIndex, RocksDbFileIndex};

/// A location where a chunk can be found: which xorb and at what index within it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkLocation {
    pub xorb_hash: String,
    pub chunk_index: u32,
}

/// Index mapping file hashes to shard hashes.
///
/// Used for file reconstruction lookups: given a file hash, find the shard
/// that describes how to reconstruct it.
pub trait FileIndex: Send + Sync {
    /// Look up which shard contains the file's reconstruction info.
    /// Returns `None` if the file is not indexed.
    fn get(
        &self,
        file_hash: &str,
    ) -> impl Future<Output = Result<Option<String>, StorageError>> + Send;

    /// Record that `file_hash` can be reconstructed from `shard_hash`.
    fn put(
        &self,
        file_hash: &str,
        shard_hash: &str,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// List all file index entries as (file_hash, shard_hash) pairs.
    fn list_all(&self) -> impl Future<Output = Result<Vec<(String, String)>, StorageError>> + Send;
}

/// Index mapping chunk hashes to their locations in xorbs.
///
/// Used for global deduplication: given a chunk hash, find which xorb(s)
/// already contain that chunk data.
pub trait ChunkIndex: Send + Sync {
    /// Look up all known locations for a chunk.
    /// Returns an empty `Vec` if the chunk is not indexed.
    fn get(
        &self,
        chunk_hash: &str,
    ) -> impl Future<Output = Result<Vec<ChunkLocation>, StorageError>> + Send;

    /// Record that `chunk_hash` exists at `location`.
    /// Deduplicates: adding the same location twice is a no-op.
    fn put(
        &self,
        chunk_hash: &str,
        location: ChunkLocation,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Record many `chunk_hash → location` entries in one write.
    /// Same dedup semantics as `put`, but a single commit — use this on the
    /// upload paths, where a xorb carries ~1000 chunks.
    fn put_batch(
        &self,
        entries: Vec<(String, ChunkLocation)>,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;
}
