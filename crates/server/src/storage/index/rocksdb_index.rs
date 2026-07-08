use std::path::Path;
use std::sync::Arc;

use rocksdb::DB;

use super::super::backend::validate_hash;
use super::super::error::StorageError;
use super::{
    ChunkIndex, ChunkLocation, FileIndex, FileListEntry, OwnerClaim, OwnerUsage, OwnershipClaim,
    UsageReport, XorbLayout, XorbSummary,
};

fn rocks_err(e: rocksdb::Error) -> StorageError {
    StorageError::Index(e.to_string())
}

/// RocksDB-backed chunk index: `chunk_hash → JSON Vec<ChunkLocation>`.
///
/// Replaces the one-JSON-file-per-chunk filesystem index, whose per-upload
/// inode storm (thousands of tiny files, written serially) dominated xorb
/// upload latency.
// ponytail: rocksdb calls run inline on the async executor — point ops are
// µs-scale and batch commits don't fsync per write. Move to spawn_blocking
// if p99 handler latency ever shows stalls.
pub struct RocksDbChunkIndex {
    db: Arc<DB>,
    /// `xorb_hash → JSON XorbLayout`, kept in its own database so xorb-keyed
    /// layout lookups don't scan the chunk keyspace.
    layouts: Arc<DB>,
}

impl RocksDbChunkIndex {
    pub fn new(data_dir: &Path) -> Result<Self, StorageError> {
        let index_dir = data_dir.join("index");

        let chunks_path = index_dir.join("chunks.rocksdb");
        std::fs::create_dir_all(&chunks_path).map_err(|e| StorageError::io(e, &chunks_path))?;
        let db = DB::open_default(&chunks_path).map_err(rocks_err)?;

        let layouts_path = index_dir.join("xorb_layouts.rocksdb");
        std::fs::create_dir_all(&layouts_path).map_err(|e| StorageError::io(e, &layouts_path))?;
        let layouts = DB::open_default(&layouts_path).map_err(rocks_err)?;

        Ok(Self {
            db: Arc::new(db),
            layouts: Arc::new(layouts),
        })
    }

    fn read_locations(&self, chunk_hash: &str) -> Result<Vec<ChunkLocation>, StorageError> {
        match self.db.get(chunk_hash.as_bytes()).map_err(rocks_err)? {
            Some(data) => serde_json::from_slice(&data)
                .map_err(|e| StorageError::Index(format!("corrupt index entry: {e}"))),
            None => Ok(Vec::new()),
        }
    }
}

impl ChunkIndex for RocksDbChunkIndex {
    async fn get(&self, chunk_hash: &str) -> Result<Vec<ChunkLocation>, StorageError> {
        validate_hash(chunk_hash)?;
        self.read_locations(chunk_hash)
    }

    async fn put(&self, chunk_hash: &str, location: ChunkLocation) -> Result<(), StorageError> {
        self.put_batch(vec![(chunk_hash.to_string(), location)])
            .await
    }

    /// Insert many `chunk_hash → location` entries in one RocksDB write batch.
    /// One atomic commit instead of one write per chunk.
    async fn put_batch(&self, entries: Vec<(String, ChunkLocation)>) -> Result<(), StorageError> {
        let mut batch = rocksdb::WriteBatch::default();
        for (chunk_hash, location) in entries {
            validate_hash(&chunk_hash)?;
            let mut locations = self.read_locations(&chunk_hash)?;
            if !locations.contains(&location) {
                locations.push(location);
                let json = serde_json::to_vec(&locations)
                    .map_err(|e| StorageError::Index(e.to_string()))?;
                batch.put(chunk_hash.as_bytes(), &json);
            }
        }
        self.db.write(batch).map_err(rocks_err)
    }

    async fn get_xorb_layout(&self, xorb_hash: &str) -> Result<Option<XorbLayout>, StorageError> {
        validate_hash(xorb_hash)?;
        match self.layouts.get(xorb_hash.as_bytes()).map_err(rocks_err)? {
            Some(data) => serde_json::from_slice(&data)
                .map(Some)
                .map_err(|e| StorageError::Index(format!("corrupt xorb layout: {e}"))),
            None => Ok(None),
        }
    }

    async fn put_xorb_layout(
        &self,
        xorb_hash: &str,
        layout: XorbLayout,
    ) -> Result<(), StorageError> {
        validate_hash(xorb_hash)?;
        let json = serde_json::to_vec(&layout).map_err(|e| StorageError::Index(e.to_string()))?;
        self.layouts
            .put(xorb_hash.as_bytes(), &json)
            .map_err(rocks_err)
    }

    async fn remove_xorb(&self, xorb_hash: &str) -> Result<(), StorageError> {
        validate_hash(xorb_hash)?;

        // The layout records exactly which chunk hashes the xorb carries, so
        // point-delete those entries. Without a layout (a xorb indexed before
        // layouts existed) fall back to scanning the whole chunk keyspace.
        let chunk_hashes: Option<Vec<String>> = self
            .get_xorb_layout(xorb_hash)
            .await?
            .map(|l| l.chunks.into_iter().map(|c| c.chunk_hash).collect());

        let mut batch = rocksdb::WriteBatch::default();
        let mut retain = |chunk_hash: &str, locations: Vec<ChunkLocation>| {
            let kept: Vec<ChunkLocation> = locations
                .into_iter()
                .filter(|l| l.xorb_hash != xorb_hash)
                .collect();
            if kept.is_empty() {
                batch.delete(chunk_hash.as_bytes());
                Ok(())
            } else {
                let json =
                    serde_json::to_vec(&kept).map_err(|e| StorageError::Index(e.to_string()))?;
                batch.put(chunk_hash.as_bytes(), &json);
                Ok::<(), StorageError>(())
            }
        };

        match chunk_hashes {
            Some(hashes) => {
                for chunk_hash in hashes {
                    let locations = self.read_locations(&chunk_hash)?;
                    if locations.iter().any(|l| l.xorb_hash == xorb_hash) {
                        retain(&chunk_hash, locations)?;
                    }
                }
            }
            None => {
                for item in self.db.iterator(rocksdb::IteratorMode::Start) {
                    let (k, v) = item.map_err(rocks_err)?;
                    let locations: Vec<ChunkLocation> = serde_json::from_slice(&v)
                        .map_err(|e| StorageError::Index(format!("corrupt index entry: {e}")))?;
                    if locations.iter().any(|l| l.xorb_hash == xorb_hash) {
                        retain(&String::from_utf8_lossy(&k), locations)?;
                    }
                }
            }
        }

        self.db.write(batch).map_err(rocks_err)?;
        self.layouts.delete(xorb_hash.as_bytes()).map_err(rocks_err)
    }

    async fn list_xorb_summaries(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<XorbSummary>, StorageError> {
        let mode = match after {
            Some(a) => rocksdb::IteratorMode::From(a.as_bytes(), rocksdb::Direction::Forward),
            None => rocksdb::IteratorMode::Start,
        };
        let mut out = Vec::new();
        for item in self.layouts.iterator(mode) {
            let (k, v) = item.map_err(rocks_err)?;
            let xorb_hash = String::from_utf8_lossy(&k).into_owned();
            if Some(xorb_hash.as_str()) == after {
                continue; // cursor is exclusive
            }
            let layout: XorbLayout = serde_json::from_slice(&v)
                .map_err(|e| StorageError::Index(format!("corrupt xorb layout: {e}")))?;
            out.push(XorbSummary {
                xorb_hash,
                num_bytes_on_disk: layout.num_bytes_on_disk as u64,
                chunk_count: layout.chunks.len() as u64,
            });
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }
}

/// RocksDB-backed file index: `file_hash → shard_hash` (plain UTF-8 value).
///
/// Ownership claims live in a separate database keyed `{file_hash}:{owner}`
/// (JSON [`OwnershipClaim`] values). The file hash is a fixed 64 hex chars, so
/// a `{file_hash}:` prefix scan finds every claim on one file, and a full scan
/// groups claims of the same file contiguously for [`FileIndex::usage`].
pub struct RocksDbFileIndex {
    db: Arc<DB>,
    ownership: Arc<DB>,
}

impl RocksDbFileIndex {
    pub fn new(data_dir: &Path) -> Result<Self, StorageError> {
        let path = data_dir.join("index").join("files.rocksdb");
        std::fs::create_dir_all(&path).map_err(|e| StorageError::io(e, &path))?;
        let db = DB::open_default(&path).map_err(rocks_err)?;

        let ownership_path = data_dir.join("index").join("ownership.rocksdb");
        std::fs::create_dir_all(&ownership_path)
            .map_err(|e| StorageError::io(e, &ownership_path))?;
        let ownership = DB::open_default(&ownership_path).map_err(rocks_err)?;

        Ok(Self {
            db: Arc::new(db),
            ownership: Arc::new(ownership),
        })
    }

    fn claim_key(file_hash: &str, owner: &str) -> Vec<u8> {
        format!("{file_hash}:{owner}").into_bytes()
    }

    /// All `(owner, claim)` pairs for one file, via prefix scan.
    fn claims_for(&self, file_hash: &str) -> Result<Vec<(String, OwnershipClaim)>, StorageError> {
        let prefix = format!("{file_hash}:");
        let mut out = Vec::new();
        for item in self.ownership.iterator(rocksdb::IteratorMode::From(
            prefix.as_bytes(),
            rocksdb::Direction::Forward,
        )) {
            let (k, v) = item.map_err(rocks_err)?;
            let key = String::from_utf8_lossy(&k);
            let Some(owner) = key.strip_prefix(&prefix) else {
                break;
            };
            let claim: OwnershipClaim = serde_json::from_slice(&v)
                .map_err(|e| StorageError::Index(format!("corrupt ownership entry: {e}")))?;
            out.push((owner.to_string(), claim));
        }
        Ok(out)
    }
}

impl FileIndex for RocksDbFileIndex {
    async fn get(&self, file_hash: &str) -> Result<Option<String>, StorageError> {
        validate_hash(file_hash)?;
        match self.db.get(file_hash.as_bytes()).map_err(rocks_err)? {
            Some(data) => Ok(Some(String::from_utf8_lossy(&data).into_owned())),
            None => Ok(None),
        }
    }

    async fn put(&self, file_hash: &str, shard_hash: &str) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        validate_hash(shard_hash)?;
        self.db
            .put(file_hash.as_bytes(), shard_hash.as_bytes())
            .map_err(rocks_err)
    }

    async fn list_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        let mut out = Vec::new();
        for item in self.db.iterator(rocksdb::IteratorMode::Start) {
            let (k, v) = item.map_err(rocks_err)?;
            out.push((
                String::from_utf8_lossy(&k).into_owned(),
                String::from_utf8_lossy(&v).into_owned(),
            ));
        }
        Ok(out)
    }

    async fn remove(&self, file_hash: &str) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        let mut batch = rocksdb::WriteBatch::default();
        for (owner, _) in self.claims_for(file_hash)? {
            batch.delete(Self::claim_key(file_hash, &owner));
        }
        self.ownership.write(batch).map_err(rocks_err)?;
        self.db.delete(file_hash.as_bytes()).map_err(rocks_err)
    }

    async fn claim(
        &self,
        owner: &str,
        file_hash: &str,
        claim: OwnershipClaim,
    ) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        let key = Self::claim_key(file_hash, owner);
        // Keep the original claim time on re-upload.
        let claim = match self.ownership.get(&key).map_err(rocks_err)? {
            Some(existing) => {
                let existing: OwnershipClaim = serde_json::from_slice(&existing)
                    .map_err(|e| StorageError::Index(format!("corrupt ownership entry: {e}")))?;
                OwnershipClaim {
                    created_at_unix: existing.created_at_unix,
                    ..claim
                }
            }
            None => claim,
        };
        let json = serde_json::to_vec(&claim).map_err(|e| StorageError::Index(e.to_string()))?;
        self.ownership.put(&key, &json).map_err(rocks_err)
    }

    async fn release(&self, owner: &str, file_hash: &str) -> Result<bool, StorageError> {
        validate_hash(file_hash)?;
        let key = Self::claim_key(file_hash, owner);
        let existed = self.ownership.get(&key).map_err(rocks_err)?.is_some();
        if existed {
            self.ownership.delete(&key).map_err(rocks_err)?;
        }
        Ok(existed)
    }

    async fn file_claims(&self, file_hash: &str) -> Result<Vec<OwnerClaim>, StorageError> {
        validate_hash(file_hash)?;
        Ok(self
            .claims_for(file_hash)?
            .into_iter()
            .map(|(owner, claim)| OwnerClaim {
                owner,
                logical_bytes: claim.logical_bytes,
                created_at_unix: claim.created_at_unix,
            })
            .collect())
    }

    async fn list_files(
        &self,
        after: Option<&str>,
        owner: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FileListEntry>, StorageError> {
        let mode = match after {
            Some(a) => rocksdb::IteratorMode::From(a.as_bytes(), rocksdb::Direction::Forward),
            None => rocksdb::IteratorMode::Start,
        };
        let mut out = Vec::new();

        match owner {
            // Scan the file index directly, ordered by file hash.
            None => {
                for item in self.db.iterator(mode) {
                    let (k, v) = item.map_err(rocks_err)?;
                    let file_hash = String::from_utf8_lossy(&k).into_owned();
                    if Some(file_hash.as_str()) == after {
                        continue; // cursor is exclusive
                    }
                    let shard_hash = String::from_utf8_lossy(&v).into_owned();
                    let logical_bytes = self
                        .claims_for(&file_hash)?
                        .first()
                        .map(|(_, c)| c.logical_bytes)
                        .unwrap_or(0);
                    out.push(FileListEntry {
                        file_hash,
                        shard_hash,
                        logical_bytes,
                    });
                    if out.len() == limit {
                        break;
                    }
                }
            }
            // Scan the ownership keyspace (keyed `{file_hash}:{owner}`, so
            // ordered by file hash) and keep only this owner's rows.
            Some(owner) => {
                for item in self.ownership.iterator(mode) {
                    let (k, v) = item.map_err(rocks_err)?;
                    let key = String::from_utf8_lossy(&k);
                    let Some((file_hash, row_owner)) = key.split_once(':') else {
                        return Err(StorageError::Index(format!(
                            "malformed ownership key: {key}"
                        )));
                    };
                    if let Some(a) = after
                        && file_hash <= a
                    {
                        continue; // cursor is exclusive on file hash
                    }
                    if row_owner != owner {
                        continue;
                    }
                    let Some(shard) = self.db.get(file_hash.as_bytes()).map_err(rocks_err)? else {
                        continue; // claim without a live file entry
                    };
                    let claim: OwnershipClaim = serde_json::from_slice(&v).map_err(|e| {
                        StorageError::Index(format!("corrupt ownership entry: {e}"))
                    })?;
                    out.push(FileListEntry {
                        file_hash: file_hash.to_string(),
                        shard_hash: String::from_utf8_lossy(&shard).into_owned(),
                        logical_bytes: claim.logical_bytes,
                    });
                    if out.len() == limit {
                        break;
                    }
                }
            }
        }

        Ok(out)
    }

    async fn usage(&self) -> Result<UsageReport, StorageError> {
        use std::collections::BTreeMap;

        let mut per_owner: BTreeMap<String, OwnerUsage> = BTreeMap::new();
        let mut claimed_files = 0u64;
        let mut unique_file_bytes = 0u64;
        // Keys sort by file_hash first, so claims on the same file are
        // contiguous: count each file once when its hash changes.
        let mut current_file: Option<String> = None;

        for item in self.ownership.iterator(rocksdb::IteratorMode::Start) {
            let (k, v) = item.map_err(rocks_err)?;
            let key = String::from_utf8_lossy(&k);
            let Some((file_hash, owner)) = key.split_once(':') else {
                return Err(StorageError::Index(format!(
                    "malformed ownership key: {key}"
                )));
            };
            let claim: OwnershipClaim = serde_json::from_slice(&v)
                .map_err(|e| StorageError::Index(format!("corrupt ownership entry: {e}")))?;

            if current_file.as_deref() != Some(file_hash) {
                current_file = Some(file_hash.to_string());
                claimed_files += 1;
                unique_file_bytes += claim.logical_bytes;
            }

            let entry = per_owner
                .entry(owner.to_string())
                .or_insert_with(|| OwnerUsage {
                    owner: owner.to_string(),
                    file_count: 0,
                    logical_bytes: 0,
                });
            entry.file_count += 1;
            entry.logical_bytes += claim.logical_bytes;
        }

        Ok(UsageReport {
            owners: per_owner.into_values().collect(),
            claimed_files,
            unique_file_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_A: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HASH_B: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn loc(xorb: &str, i: u32) -> ChunkLocation {
        ChunkLocation {
            xorb_hash: xorb.to_string(),
            chunk_index: i,
        }
    }

    #[tokio::test]
    async fn chunk_roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let index = RocksDbChunkIndex::new(dir.path()).unwrap();

        assert!(index.get(CHUNK_HASH).await.unwrap().is_empty());

        index.put(CHUNK_HASH, loc(HASH_A, 0)).await.unwrap();
        index.put(CHUNK_HASH, loc(HASH_A, 0)).await.unwrap(); // duplicate: no-op
        index.put(CHUNK_HASH, loc(HASH_B, 3)).await.unwrap();

        let locations = index.get(CHUNK_HASH).await.unwrap();
        assert_eq!(locations, vec![loc(HASH_A, 0), loc(HASH_B, 3)]);
    }

    #[tokio::test]
    async fn chunk_put_batch() {
        let dir = tempfile::tempdir().unwrap();
        let index = RocksDbChunkIndex::new(dir.path()).unwrap();

        index
            .put_batch(vec![
                (CHUNK_HASH.to_string(), loc(HASH_A, 0)),
                (HASH_B.to_string(), loc(HASH_A, 1)),
            ])
            .await
            .unwrap();

        assert_eq!(index.get(CHUNK_HASH).await.unwrap(), vec![loc(HASH_A, 0)]);
        assert_eq!(index.get(HASH_B).await.unwrap(), vec![loc(HASH_A, 1)]);
    }

    #[tokio::test]
    async fn chunk_rejects_bad_hash() {
        let dir = tempfile::tempdir().unwrap();
        let index = RocksDbChunkIndex::new(dir.path()).unwrap();
        assert!(index.put("bad", loc(HASH_A, 0)).await.is_err());
        assert!(index.get("bad").await.is_err());
    }

    #[tokio::test]
    async fn file_roundtrip_overwrite_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let index = RocksDbFileIndex::new(dir.path()).unwrap();

        assert!(index.get(HASH_A).await.unwrap().is_none());

        index.put(HASH_A, HASH_B).await.unwrap();
        assert_eq!(index.get(HASH_A).await.unwrap().as_deref(), Some(HASH_B));

        index.put(HASH_A, CHUNK_HASH).await.unwrap(); // overwrite
        assert_eq!(
            index.get(HASH_A).await.unwrap().as_deref(),
            Some(CHUNK_HASH)
        );

        assert_eq!(
            index.list_all().await.unwrap(),
            vec![(HASH_A.to_string(), CHUNK_HASH.to_string())]
        );

        assert!(index.put("bad", HASH_A).await.is_err());
        assert!(index.put(HASH_A, "bad").await.is_err());
    }
}
