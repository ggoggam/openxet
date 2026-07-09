use std::path::Path;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use super::super::backend::validate_hash;
use super::super::error::StorageError;
use super::{
    ChunkIndex, ChunkLocation, FileIndex, FileListEntry, OwnerClaim, OwnerUsage, OwnershipClaim,
    UsageReport, XorbLayout, XorbSummary,
};

fn sq_err(e: sqlx::Error) -> StorageError {
    StorageError::Index(e.to_string())
}

/// Rows per INSERT statement in `put_batch`: 3 bind parameters each, kept
/// well under SQLite's 32766-parameter statement limit.
const INSERT_BATCH_ROWS: usize = 4096;

/// Open (creating if needed) the node-local index database at
/// `{data_dir}/index/index.sqlite` and bring its schema up to date by running
/// the embedded migrations (compiled in from `crates/server/migrations/sqlite/`).
///
/// WAL keeps readers unblocked during writes, and the busy timeout serializes
/// the pool's writers instead of surfacing `SQLITE_BUSY` errors.
pub async fn connect(data_dir: &Path) -> Result<SqlitePool, StorageError> {
    let index_dir = data_dir.join("index");
    std::fs::create_dir_all(&index_dir).map_err(|e| StorageError::io(e, &index_dir))?;

    let options = SqliteConnectOptions::new()
        .filename(index_dir.join("index.sqlite"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(sq_err)?;

    sqlx::migrate!("./migrations/sqlite")
        .run(&pool)
        .await
        .map_err(|e| StorageError::Index(e.to_string()))?;

    Ok(pool)
}

/// SQLite-backed chunk index: `chunk_hash → Vec<ChunkLocation>`.
///
/// The node-local default, for single-instance deployments; use
/// [`super::PostgresChunkIndex`] when replicas must share one dedup state.
pub struct SqliteChunkIndex {
    pool: SqlitePool,
}

impl SqliteChunkIndex {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl ChunkIndex for SqliteChunkIndex {
    async fn get(&self, chunk_hash: &str) -> Result<Vec<ChunkLocation>, StorageError> {
        validate_hash(chunk_hash)?;
        // rowid stands in for the Postgres schema's `seq`: it preserves
        // insertion order across locations for the same chunk.
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT xorb_hash, chunk_idx FROM chunk_index WHERE chunk_hash = $1 ORDER BY rowid",
        )
        .bind(chunk_hash)
        .fetch_all(&self.pool)
        .await
        .map_err(sq_err)?;

        Ok(rows
            .into_iter()
            .map(|(xorb_hash, chunk_idx)| ChunkLocation {
                xorb_hash,
                chunk_index: chunk_idx as u32,
            })
            .collect())
    }

    async fn put(&self, chunk_hash: &str, location: ChunkLocation) -> Result<(), StorageError> {
        self.put_batch(vec![(chunk_hash.to_string(), location)])
            .await
    }

    /// Insert many `chunk_hash → location` entries in one transaction. The
    /// composite primary key makes `ON CONFLICT DO NOTHING` the dedup path.
    ///
    /// SQLite has no `UNNEST`, so rows go in as multi-row `VALUES` statements,
    /// chunked to respect the bind-parameter limit — still a single commit for
    /// a ~1000-chunk xorb rather than one fsync per chunk.
    async fn put_batch(&self, entries: Vec<(String, ChunkLocation)>) -> Result<(), StorageError> {
        if entries.is_empty() {
            return Ok(());
        }
        for (chunk_hash, _) in &entries {
            validate_hash(chunk_hash)?;
        }

        let mut tx = self.pool.begin().await.map_err(sq_err)?;
        for batch in entries.chunks(INSERT_BATCH_ROWS) {
            let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
                "INSERT INTO chunk_index (chunk_hash, xorb_hash, chunk_idx) ",
            );
            qb.push_values(batch, |mut row, (chunk_hash, location)| {
                row.push_bind(chunk_hash)
                    .push_bind(&location.xorb_hash)
                    .push_bind(location.chunk_index as i64);
            });
            qb.push(" ON CONFLICT DO NOTHING");
            qb.build().execute(&mut *tx).await.map_err(sq_err)?;
        }
        tx.commit().await.map_err(sq_err)
    }

    async fn get_xorb_layout(&self, xorb_hash: &str) -> Result<Option<XorbLayout>, StorageError> {
        validate_hash(xorb_hash)?;
        let json: Option<String> =
            sqlx::query_scalar("SELECT layout FROM xorb_layout WHERE xorb_hash = $1")
                .bind(xorb_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(sq_err)?;
        match json {
            Some(json) => serde_json::from_str(&json)
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
        let json =
            serde_json::to_string(&layout).map_err(|e| StorageError::Index(e.to_string()))?;
        sqlx::query(
            "INSERT INTO xorb_layout (xorb_hash, layout) VALUES ($1, $2) \
             ON CONFLICT (xorb_hash) DO UPDATE SET layout = EXCLUDED.layout",
        )
        .bind(xorb_hash)
        .bind(&json)
        .execute(&self.pool)
        .await
        .map_err(sq_err)?;
        Ok(())
    }

    async fn remove_xorb(&self, xorb_hash: &str) -> Result<(), StorageError> {
        validate_hash(xorb_hash)?;
        let mut tx = self.pool.begin().await.map_err(sq_err)?;
        sqlx::query("DELETE FROM chunk_index WHERE xorb_hash = $1")
            .bind(xorb_hash)
            .execute(&mut *tx)
            .await
            .map_err(sq_err)?;
        sqlx::query("DELETE FROM xorb_layout WHERE xorb_hash = $1")
            .bind(xorb_hash)
            .execute(&mut *tx)
            .await
            .map_err(sq_err)?;
        tx.commit().await.map_err(sq_err)
    }

    async fn list_xorb_summaries(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<XorbSummary>, StorageError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT xorb_hash, layout FROM xorb_layout \
             WHERE ($1 IS NULL OR xorb_hash > $1) \
             ORDER BY xorb_hash LIMIT $2",
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(sq_err)?;

        rows.into_iter()
            .map(|(xorb_hash, layout_json)| {
                let layout: XorbLayout = serde_json::from_str(&layout_json)
                    .map_err(|e| StorageError::Index(format!("corrupt xorb layout: {e}")))?;
                Ok(XorbSummary {
                    xorb_hash,
                    num_bytes_on_disk: layout.num_bytes_on_disk as u64,
                    chunk_count: layout.chunks.len() as u64,
                })
            })
            .collect()
    }
}

/// SQLite-backed file index: `file_hash → shard_hash`.
pub struct SqliteFileIndex {
    pool: SqlitePool,
}

impl SqliteFileIndex {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl FileIndex for SqliteFileIndex {
    async fn get(&self, file_hash: &str) -> Result<Option<String>, StorageError> {
        validate_hash(file_hash)?;
        let shard_hash: Option<String> =
            sqlx::query_scalar("SELECT shard_hash FROM file_index WHERE file_hash = $1")
                .bind(file_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(sq_err)?;
        Ok(shard_hash)
    }

    async fn put(&self, file_hash: &str, shard_hash: &str) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        validate_hash(shard_hash)?;
        sqlx::query(
            "INSERT INTO file_index (file_hash, shard_hash) VALUES ($1, $2) \
             ON CONFLICT (file_hash) DO UPDATE SET shard_hash = EXCLUDED.shard_hash",
        )
        .bind(file_hash)
        .bind(shard_hash)
        .execute(&self.pool)
        .await
        .map_err(sq_err)?;
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT file_hash, shard_hash FROM file_index")
                .fetch_all(&self.pool)
                .await
                .map_err(sq_err)?;
        Ok(rows)
    }

    async fn remove(&self, file_hash: &str) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        let mut tx = self.pool.begin().await.map_err(sq_err)?;
        sqlx::query("DELETE FROM file_ownership WHERE file_hash = $1")
            .bind(file_hash)
            .execute(&mut *tx)
            .await
            .map_err(sq_err)?;
        sqlx::query("DELETE FROM file_index WHERE file_hash = $1")
            .bind(file_hash)
            .execute(&mut *tx)
            .await
            .map_err(sq_err)?;
        tx.commit().await.map_err(sq_err)
    }

    async fn claim(
        &self,
        owner: &str,
        file_hash: &str,
        claim: OwnershipClaim,
    ) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        // Re-claiming refreshes the size but keeps the original claim time.
        sqlx::query(
            "INSERT INTO file_ownership (file_hash, owner_id, logical_bytes, created_at_unix) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (file_hash, owner_id) \
             DO UPDATE SET logical_bytes = EXCLUDED.logical_bytes",
        )
        .bind(file_hash)
        .bind(owner)
        .bind(claim.logical_bytes as i64)
        .bind(claim.created_at_unix)
        .execute(&self.pool)
        .await
        .map_err(sq_err)?;
        Ok(())
    }

    async fn release(&self, owner: &str, file_hash: &str) -> Result<bool, StorageError> {
        validate_hash(file_hash)?;
        let result =
            sqlx::query("DELETE FROM file_ownership WHERE file_hash = $1 AND owner_id = $2")
                .bind(file_hash)
                .bind(owner)
                .execute(&self.pool)
                .await
                .map_err(sq_err)?;
        Ok(result.rows_affected() > 0)
    }

    async fn file_claims(&self, file_hash: &str) -> Result<Vec<OwnerClaim>, StorageError> {
        validate_hash(file_hash)?;
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT owner_id, logical_bytes, created_at_unix \
             FROM file_ownership WHERE file_hash = $1 ORDER BY owner_id",
        )
        .bind(file_hash)
        .fetch_all(&self.pool)
        .await
        .map_err(sq_err)?;
        Ok(rows
            .into_iter()
            .map(|(owner, logical_bytes, created_at_unix)| OwnerClaim {
                owner,
                logical_bytes: logical_bytes as u64,
                created_at_unix,
            })
            .collect())
    }

    async fn list_files(
        &self,
        after: Option<&str>,
        owner: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FileListEntry>, StorageError> {
        // `$after IS NULL OR key > $after` folds the first-page and
        // continuation cases into one prepared statement.
        let rows: Vec<(String, String, i64)> = match owner {
            None => sqlx::query_as(
                "SELECT f.file_hash, f.shard_hash, \
                            COALESCE(MAX(o.logical_bytes), 0) \
                     FROM file_index f \
                     LEFT JOIN file_ownership o ON o.file_hash = f.file_hash \
                     WHERE ($1 IS NULL OR f.file_hash > $1) \
                     GROUP BY f.file_hash, f.shard_hash \
                     ORDER BY f.file_hash \
                     LIMIT $2",
            )
            .bind(after)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(sq_err)?,
            Some(owner) => sqlx::query_as(
                "SELECT o.file_hash, f.shard_hash, o.logical_bytes \
                     FROM file_ownership o \
                     JOIN file_index f ON f.file_hash = o.file_hash \
                     WHERE o.owner_id = $1 AND ($2 IS NULL OR o.file_hash > $2) \
                     ORDER BY o.file_hash \
                     LIMIT $3",
            )
            .bind(owner)
            .bind(after)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(sq_err)?,
        };
        Ok(rows
            .into_iter()
            .map(|(file_hash, shard_hash, logical_bytes)| FileListEntry {
                file_hash,
                shard_hash,
                logical_bytes: logical_bytes as u64,
            })
            .collect())
    }

    async fn usage(&self) -> Result<UsageReport, StorageError> {
        let owner_rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT owner_id, COUNT(*), COALESCE(SUM(logical_bytes), 0) \
             FROM file_ownership GROUP BY owner_id ORDER BY owner_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(sq_err)?;

        // Count each distinct file once; claims agree on the size (it is
        // derived from the file's content), so any row's value works.
        let totals: Option<(i64, i64)> = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(logical_bytes), 0) \
             FROM (SELECT file_hash, MAX(logical_bytes) AS logical_bytes \
                   FROM file_ownership GROUP BY file_hash) AS per_file",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(sq_err)?;
        let (claimed_files, unique_file_bytes) = totals.unwrap_or((0, 0));

        Ok(UsageReport {
            owners: owner_rows
                .into_iter()
                .map(|(owner, file_count, logical_bytes)| OwnerUsage {
                    owner,
                    file_count: file_count as u64,
                    logical_bytes: logical_bytes as u64,
                })
                .collect(),
            claimed_files: claimed_files as u64,
            unique_file_bytes: unique_file_bytes as u64,
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
        let index = SqliteChunkIndex::new(connect(dir.path()).await.unwrap());

        assert!(index.get(CHUNK_HASH).await.unwrap().is_empty());

        index.put(CHUNK_HASH, loc(HASH_A, 0)).await.unwrap();
        index.put(CHUNK_HASH, loc(HASH_A, 0)).await.unwrap(); // duplicate: no-op
        index.put(CHUNK_HASH, loc(HASH_B, 3)).await.unwrap();

        // rowid ordering: locations come back in insertion order.
        let locations = index.get(CHUNK_HASH).await.unwrap();
        assert_eq!(locations, vec![loc(HASH_A, 0), loc(HASH_B, 3)]);
    }

    #[tokio::test]
    async fn chunk_put_batch() {
        let dir = tempfile::tempdir().unwrap();
        let index = SqliteChunkIndex::new(connect(dir.path()).await.unwrap());

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
        let index = SqliteChunkIndex::new(connect(dir.path()).await.unwrap());
        assert!(index.put("bad", loc(HASH_A, 0)).await.is_err());
        assert!(index.get("bad").await.is_err());
    }

    #[tokio::test]
    async fn file_roundtrip_overwrite_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let index = SqliteFileIndex::new(connect(dir.path()).await.unwrap());

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
