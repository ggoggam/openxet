use std::path::Path;
use std::sync::Arc;

use rocksdb::DB;

use super::super::backend::validate_hash;
use super::super::error::StorageError;
use super::{ChunkIndex, ChunkLocation, FileIndex, XorbLayout};

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
}

/// RocksDB-backed file index: `file_hash → shard_hash` (plain UTF-8 value).
pub struct RocksDbFileIndex {
    db: Arc<DB>,
}

impl RocksDbFileIndex {
    pub fn new(data_dir: &Path) -> Result<Self, StorageError> {
        let path = data_dir.join("index").join("files.rocksdb");
        std::fs::create_dir_all(&path).map_err(|e| StorageError::io(e, &path))?;
        let db = DB::open_default(&path).map_err(rocks_err)?;
        Ok(Self { db: Arc::new(db) })
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
