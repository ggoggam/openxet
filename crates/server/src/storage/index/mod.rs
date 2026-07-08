pub mod postgres_index;
pub mod rocksdb_index;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;

use crate::config::StorageConfig;

use super::error::StorageError;

pub use postgres_index::{PostgresChunkIndex, PostgresFileIndex};
pub use rocksdb_index::{RocksDbChunkIndex, RocksDbFileIndex};

/// A location where a chunk can be found: which xorb and at what index within it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkLocation {
    pub xorb_hash: String,
    pub chunk_index: u32,
}

/// One chunk within a stored xorb, in xorb order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XorbChunk {
    /// 64-hex chunk hash.
    pub chunk_hash: String,
    /// Uncompressed size of the chunk in bytes.
    pub unpacked_size: u32,
    /// Compressed on-disk size of the chunk's data in bytes, excluding the
    /// 8-byte xorb chunk header. Together with the header size this lets
    /// reconstruction compute a chunk's byte range within the serialized xorb
    /// without re-downloading it.
    ///
    /// Defaults to `0` for layouts written before this field existed; a `0`
    /// signals "unknown", and reconstruction falls back to reading the xorb.
    #[serde(default)]
    pub compressed_size: u32,
}

/// The recorded layout of a xorb: its on-disk size and its chunks in order.
///
/// Persisted at xorb upload so global-dedup responses can be built without
/// re-downloading the xorb from the object store and decompressing every chunk
/// to recover its hashes and byte ranges. Byte offsets are the running sum of
/// `unpacked_size`, so they are recomputed on read rather than stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XorbLayout {
    /// Total compressed size of the xorb as stored (`num_bytes_on_disk`).
    pub num_bytes_on_disk: u32,
    pub chunks: Vec<XorbChunk>,
}

/// One ownership claim on a file: who registered it and how large it is.
///
/// Claims are the accounting unit. The same file uploaded by two owners
/// yields two claims on one `file_index` row — each owner is charged the
/// file's full logical size, and the file only becomes garbage once every
/// claim is released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipClaim {
    /// Logical (pre-dedup, pre-compression) file size in bytes.
    pub logical_bytes: u64,
    /// When the claim was first recorded, unix seconds.
    pub created_at_unix: i64,
}

/// Aggregated usage for one owner across all their claimed files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerUsage {
    pub owner: String,
    pub file_count: u64,
    pub logical_bytes: u64,
}

/// One ownership claim with its holder, for the file-detail view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerClaim {
    pub owner: String,
    pub logical_bytes: u64,
    pub created_at_unix: i64,
}

/// One row of the paginated file listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListEntry {
    pub file_hash: String,
    pub shard_hash: String,
    /// Logical size from an ownership claim, or `0` for a pre-accounting file
    /// that has no claims.
    pub logical_bytes: u64,
}

/// One row of the paginated xorb listing, sourced from the layout index (the
/// set of xorbs known to dedup), not an object-store scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct XorbSummary {
    pub xorb_hash: String,
    /// Stored (compressed) size on disk.
    pub num_bytes_on_disk: u64,
    pub chunk_count: u64,
}

/// A full accounting snapshot derived from the ownership claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Per-owner totals, sorted by owner for stable output.
    pub owners: Vec<OwnerUsage>,
    /// Number of distinct claimed files.
    pub claimed_files: u64,
    /// Logical bytes counting each distinct file once (unlike the per-owner
    /// totals, which charge every claimant). Comparing this against physical
    /// xorb bytes gives the dedup ratio.
    pub unique_file_bytes: u64,
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

    /// List files ordered by file hash, starting strictly after `after`
    /// (a keyset cursor), returning at most `limit` rows. When `owner` is set,
    /// only files that owner claims are returned. This is the paginated
    /// management listing; `list_all` remains for full in-process scans (GC).
    fn list_files(
        &self,
        after: Option<&str>,
        owner: Option<&str>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<FileListEntry>, StorageError>> + Send;

    /// Remove a file's index entry and any remaining ownership claims on it.
    /// The file becomes unreachable and its exclusive storage is reclaimed by
    /// the next GC sweep. Removing an unknown file is a no-op.
    fn remove(&self, file_hash: &str) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Record (or refresh) `owner`'s claim on a file. Claiming an
    /// already-claimed file updates the size but keeps the original
    /// `created_at_unix`.
    fn claim(
        &self,
        owner: &str,
        file_hash: &str,
        claim: OwnershipClaim,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Release `owner`'s claim on a file. Returns `true` if a claim existed.
    fn release(
        &self,
        owner: &str,
        file_hash: &str,
    ) -> impl Future<Output = Result<bool, StorageError>> + Send;

    /// List the claims currently held on a file, ordered by owner. An empty
    /// result means either the file is unknown or it predates accounting.
    fn file_claims(
        &self,
        file_hash: &str,
    ) -> impl Future<Output = Result<Vec<OwnerClaim>, StorageError>> + Send;

    /// Aggregate all ownership claims into an accounting snapshot.
    fn usage(&self) -> impl Future<Output = Result<UsageReport, StorageError>> + Send;
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

    /// Look up a xorb's recorded chunk layout, or `None` if it was never stored
    /// (e.g. a xorb uploaded before layouts were recorded — the dedup handler
    /// then falls back to reading the xorb from the object store).
    fn get_xorb_layout(
        &self,
        xorb_hash: &str,
    ) -> impl Future<Output = Result<Option<XorbLayout>, StorageError>> + Send;

    /// Record a xorb's chunk layout for use in dedup responses. Idempotent:
    /// re-recording the same xorb overwrites with identical data.
    fn put_xorb_layout(
        &self,
        xorb_hash: &str,
        layout: XorbLayout,
    ) -> impl Future<Output = Result<(), StorageError>> + Send;

    /// Remove a deleted xorb from the index: its layout and every
    /// `chunk_hash → location` entry pointing at it, so dedup responses stop
    /// directing clients to a xorb that no longer exists. Idempotent.
    fn remove_xorb(&self, xorb_hash: &str)
    -> impl Future<Output = Result<(), StorageError>> + Send;

    /// List indexed xorbs (those with a recorded layout) ordered by xorb hash,
    /// starting strictly after `after`, returning at most `limit` summaries.
    fn list_xorb_summaries(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<XorbSummary>, StorageError>> + Send;
}

/// A [`FileIndex`] selected at startup from configuration.
///
/// The `FileIndex` trait uses `-> impl Future` (RPITIT), which is not
/// `dyn`-compatible, so we dispatch over a closed set of backends with an enum
/// rather than `Arc<dyn FileIndex>`.
pub enum FileIndexBackend {
    RocksDb(RocksDbFileIndex),
    Postgres(PostgresFileIndex),
}

impl FileIndex for FileIndexBackend {
    async fn get(&self, file_hash: &str) -> Result<Option<String>, StorageError> {
        match self {
            Self::RocksDb(i) => i.get(file_hash).await,
            Self::Postgres(i) => i.get(file_hash).await,
        }
    }

    async fn put(&self, file_hash: &str, shard_hash: &str) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.put(file_hash, shard_hash).await,
            Self::Postgres(i) => i.put(file_hash, shard_hash).await,
        }
    }

    async fn list_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        match self {
            Self::RocksDb(i) => i.list_all().await,
            Self::Postgres(i) => i.list_all().await,
        }
    }

    async fn list_files(
        &self,
        after: Option<&str>,
        owner: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FileListEntry>, StorageError> {
        match self {
            Self::RocksDb(i) => i.list_files(after, owner, limit).await,
            Self::Postgres(i) => i.list_files(after, owner, limit).await,
        }
    }

    async fn remove(&self, file_hash: &str) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.remove(file_hash).await,
            Self::Postgres(i) => i.remove(file_hash).await,
        }
    }

    async fn claim(
        &self,
        owner: &str,
        file_hash: &str,
        claim: OwnershipClaim,
    ) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.claim(owner, file_hash, claim).await,
            Self::Postgres(i) => i.claim(owner, file_hash, claim).await,
        }
    }

    async fn release(&self, owner: &str, file_hash: &str) -> Result<bool, StorageError> {
        match self {
            Self::RocksDb(i) => i.release(owner, file_hash).await,
            Self::Postgres(i) => i.release(owner, file_hash).await,
        }
    }

    async fn file_claims(&self, file_hash: &str) -> Result<Vec<OwnerClaim>, StorageError> {
        match self {
            Self::RocksDb(i) => i.file_claims(file_hash).await,
            Self::Postgres(i) => i.file_claims(file_hash).await,
        }
    }

    async fn usage(&self) -> Result<UsageReport, StorageError> {
        match self {
            Self::RocksDb(i) => i.usage().await,
            Self::Postgres(i) => i.usage().await,
        }
    }
}

/// A [`ChunkIndex`] selected at startup from configuration. See
/// [`FileIndexBackend`] for why this is an enum rather than a trait object.
pub enum ChunkIndexBackend {
    RocksDb(RocksDbChunkIndex),
    Postgres(PostgresChunkIndex),
}

impl ChunkIndex for ChunkIndexBackend {
    async fn get(&self, chunk_hash: &str) -> Result<Vec<ChunkLocation>, StorageError> {
        match self {
            Self::RocksDb(i) => i.get(chunk_hash).await,
            Self::Postgres(i) => i.get(chunk_hash).await,
        }
    }

    async fn put(&self, chunk_hash: &str, location: ChunkLocation) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.put(chunk_hash, location).await,
            Self::Postgres(i) => i.put(chunk_hash, location).await,
        }
    }

    async fn put_batch(&self, entries: Vec<(String, ChunkLocation)>) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.put_batch(entries).await,
            Self::Postgres(i) => i.put_batch(entries).await,
        }
    }

    async fn get_xorb_layout(&self, xorb_hash: &str) -> Result<Option<XorbLayout>, StorageError> {
        match self {
            Self::RocksDb(i) => i.get_xorb_layout(xorb_hash).await,
            Self::Postgres(i) => i.get_xorb_layout(xorb_hash).await,
        }
    }

    async fn put_xorb_layout(
        &self,
        xorb_hash: &str,
        layout: XorbLayout,
    ) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.put_xorb_layout(xorb_hash, layout).await,
            Self::Postgres(i) => i.put_xorb_layout(xorb_hash, layout).await,
        }
    }

    async fn remove_xorb(&self, xorb_hash: &str) -> Result<(), StorageError> {
        match self {
            Self::RocksDb(i) => i.remove_xorb(xorb_hash).await,
            Self::Postgres(i) => i.remove_xorb(xorb_hash).await,
        }
    }

    async fn list_xorb_summaries(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<XorbSummary>, StorageError> {
        match self {
            Self::RocksDb(i) => i.list_xorb_summaries(after, limit).await,
            Self::Postgres(i) => i.list_xorb_summaries(after, limit).await,
        }
    }
}

/// Build the file and chunk indexes from configuration.
///
/// `rocksdb` (the default) keeps a node-local index and is fine for a single
/// instance; `postgres` shares one index across replicas so dedup and
/// reconstruction stay consistent when the server is scaled out. Both indexes
/// share a single `PgPool` on the Postgres path.
pub async fn build_index(
    config: &StorageConfig,
) -> anyhow::Result<(FileIndexBackend, ChunkIndexBackend)> {
    match config.index_backend.as_str() {
        "rocksdb" => {
            let data_dir = &config.data_dir;
            let file = RocksDbFileIndex::new(data_dir)?;
            let chunk = RocksDbChunkIndex::new(data_dir)?;
            Ok((
                FileIndexBackend::RocksDb(file),
                ChunkIndexBackend::RocksDb(chunk),
            ))
        }
        "postgres" => {
            let url = config
                .postgres_url
                .as_deref()
                .context("postgres_url is required for the postgres index backend")?;
            let pool = PgPoolOptions::new()
                .max_connections(config.postgres_max_connections)
                .connect(url)
                .await
                .context("failed to connect to postgres")?;
            postgres_index::init_schema(&pool).await?;
            Ok((
                FileIndexBackend::Postgres(PostgresFileIndex::new(pool.clone())),
                ChunkIndexBackend::Postgres(PostgresChunkIndex::new(pool)),
            ))
        }
        other => bail!("unknown index backend: {other}"),
    }
}
