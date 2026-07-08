use sqlx::PgPool;

use super::super::backend::validate_hash;
use super::super::error::StorageError;
use super::{
    ChunkIndex, ChunkLocation, FileIndex, FileListEntry, OwnerClaim, OwnerUsage, OwnershipClaim,
    UsageReport, XorbLayout, XorbSummary,
};

fn pg_err(e: sqlx::Error) -> StorageError {
    StorageError::Index(e.to_string())
}

/// Create the index tables if they do not already exist.
///
/// Both indexes are pure materialized views of uploaded shards, so a shared
/// Postgres schema lets every server replica see the same dedup and
/// reconstruction state — which node-local RocksDB cannot.
pub async fn init_schema(pool: &PgPool) -> Result<(), StorageError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS file_index (\
            file_hash  TEXT PRIMARY KEY,\
            shard_hash TEXT NOT NULL\
        )",
    )
    .execute(pool)
    .await
    .map_err(pg_err)?;

    // `seq` preserves insertion order for `get`, matching the RocksDB backend;
    // the composite primary key enforces dedup (same `put_batch` semantics).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chunk_index (\
            seq        BIGSERIAL,\
            chunk_hash TEXT   NOT NULL,\
            xorb_hash  TEXT   NOT NULL,\
            chunk_idx  BIGINT NOT NULL,\
            PRIMARY KEY (chunk_hash, xorb_hash, chunk_idx)\
        )",
    )
    .execute(pool)
    .await
    .map_err(pg_err)?;

    sqlx::query("CREATE INDEX IF NOT EXISTS chunk_index_hash_seq ON chunk_index (chunk_hash, seq)")
        .execute(pool)
        .await
        .map_err(pg_err)?;

    // GC removes a deleted xorb's entries by xorb_hash, which the primary key
    // (led by chunk_hash) cannot serve.
    sqlx::query("CREATE INDEX IF NOT EXISTS chunk_index_xorb ON chunk_index (xorb_hash)")
        .execute(pool)
        .await
        .map_err(pg_err)?;

    // Ownership claims: the accounting unit. One row per (file, owner); the
    // file itself stays in file_index until its last claim is released.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS file_ownership (\
            file_hash       TEXT   NOT NULL,\
            owner_id        TEXT   NOT NULL,\
            logical_bytes   BIGINT NOT NULL,\
            created_at_unix BIGINT NOT NULL,\
            PRIMARY KEY (file_hash, owner_id)\
        )",
    )
    .execute(pool)
    .await
    .map_err(pg_err)?;

    // Composite (owner_id, file_hash): serves both the per-owner usage
    // aggregation and the keyset-paginated owner-filtered file listing
    // (WHERE owner_id = $ AND file_hash > $ ORDER BY file_hash).
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS file_ownership_owner_file \
         ON file_ownership (owner_id, file_hash)",
    )
    .execute(pool)
    .await
    .map_err(pg_err)?;

    // Xorb layouts are stored as one JSON row per xorb: dedup responses need the
    // whole layout at once and never query it by chunk, so a serialized blob is
    // simpler than a normalized table and reads in a single round-trip.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS xorb_layout (\
            xorb_hash TEXT PRIMARY KEY,\
            layout    TEXT NOT NULL\
        )",
    )
    .execute(pool)
    .await
    .map_err(pg_err)?;

    Ok(())
}

/// Postgres-backed chunk index: `chunk_hash → Vec<ChunkLocation>`.
///
/// A shared alternative to [`super::RocksDbChunkIndex`] for multi-replica
/// deployments, where every instance must observe the same global dedup state.
pub struct PostgresChunkIndex {
    pool: PgPool,
}

impl PostgresChunkIndex {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ChunkIndex for PostgresChunkIndex {
    async fn get(&self, chunk_hash: &str) -> Result<Vec<ChunkLocation>, StorageError> {
        validate_hash(chunk_hash)?;
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT xorb_hash, chunk_idx FROM chunk_index WHERE chunk_hash = $1 ORDER BY seq",
        )
        .bind(chunk_hash)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;

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

    /// Insert many `chunk_hash → location` entries in a single statement. The
    /// composite primary key makes `ON CONFLICT DO NOTHING` the dedup path.
    ///
    /// Columns are passed as parallel arrays and expanded with `UNNEST`, so a
    /// ~1000-chunk xorb is one round-trip to Postgres rather than one INSERT per
    /// chunk — the difference between a fast upload and a very slow one.
    async fn put_batch(&self, entries: Vec<(String, ChunkLocation)>) -> Result<(), StorageError> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut chunk_hashes = Vec::with_capacity(entries.len());
        let mut xorb_hashes = Vec::with_capacity(entries.len());
        let mut chunk_idxs = Vec::with_capacity(entries.len());
        for (chunk_hash, location) in entries {
            validate_hash(&chunk_hash)?;
            chunk_hashes.push(chunk_hash);
            xorb_hashes.push(location.xorb_hash);
            chunk_idxs.push(location.chunk_index as i64);
        }

        sqlx::query(
            "INSERT INTO chunk_index (chunk_hash, xorb_hash, chunk_idx) \
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::bigint[]) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&chunk_hashes)
        .bind(&xorb_hashes)
        .bind(&chunk_idxs)
        .execute(&self.pool)
        .await
        .map_err(pg_err)?;

        Ok(())
    }

    async fn get_xorb_layout(&self, xorb_hash: &str) -> Result<Option<XorbLayout>, StorageError> {
        validate_hash(xorb_hash)?;
        let json: Option<String> =
            sqlx::query_scalar("SELECT layout FROM xorb_layout WHERE xorb_hash = $1")
                .bind(xorb_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(pg_err)?;
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
        .map_err(pg_err)?;
        Ok(())
    }

    async fn remove_xorb(&self, xorb_hash: &str) -> Result<(), StorageError> {
        validate_hash(xorb_hash)?;
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        sqlx::query("DELETE FROM chunk_index WHERE xorb_hash = $1")
            .bind(xorb_hash)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        sqlx::query("DELETE FROM xorb_layout WHERE xorb_hash = $1")
            .bind(xorb_hash)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)
    }

    async fn list_xorb_summaries(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<XorbSummary>, StorageError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT xorb_hash, layout FROM xorb_layout \
             WHERE ($1::text IS NULL OR xorb_hash > $1) \
             ORDER BY xorb_hash LIMIT $2",
        )
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;

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

/// Postgres-backed file index: `file_hash → shard_hash`.
pub struct PostgresFileIndex {
    pool: PgPool,
}

impl PostgresFileIndex {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl FileIndex for PostgresFileIndex {
    async fn get(&self, file_hash: &str) -> Result<Option<String>, StorageError> {
        validate_hash(file_hash)?;
        let shard_hash: Option<String> =
            sqlx::query_scalar("SELECT shard_hash FROM file_index WHERE file_hash = $1")
                .bind(file_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(pg_err)?;
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
        .map_err(pg_err)?;
        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<(String, String)>, StorageError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT file_hash, shard_hash FROM file_index")
                .fetch_all(&self.pool)
                .await
                .map_err(pg_err)?;
        Ok(rows)
    }

    async fn remove(&self, file_hash: &str) -> Result<(), StorageError> {
        validate_hash(file_hash)?;
        let mut tx = self.pool.begin().await.map_err(pg_err)?;
        sqlx::query("DELETE FROM file_ownership WHERE file_hash = $1")
            .bind(file_hash)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        sqlx::query("DELETE FROM file_index WHERE file_hash = $1")
            .bind(file_hash)
            .execute(&mut *tx)
            .await
            .map_err(pg_err)?;
        tx.commit().await.map_err(pg_err)
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
        .map_err(pg_err)?;
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
                .map_err(pg_err)?;
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
        .map_err(pg_err)?;
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
                            COALESCE(MAX(o.logical_bytes), 0)::BIGINT \
                     FROM file_index f \
                     LEFT JOIN file_ownership o ON o.file_hash = f.file_hash \
                     WHERE ($1::text IS NULL OR f.file_hash > $1) \
                     GROUP BY f.file_hash, f.shard_hash \
                     ORDER BY f.file_hash \
                     LIMIT $2",
            )
            .bind(after)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(pg_err)?,
            Some(owner) => sqlx::query_as(
                "SELECT o.file_hash, f.shard_hash, o.logical_bytes \
                     FROM file_ownership o \
                     JOIN file_index f ON f.file_hash = o.file_hash \
                     WHERE o.owner_id = $1 AND ($2::text IS NULL OR o.file_hash > $2) \
                     ORDER BY o.file_hash \
                     LIMIT $3",
            )
            .bind(owner)
            .bind(after)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(pg_err)?,
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
            "SELECT owner_id, COUNT(*), COALESCE(SUM(logical_bytes), 0)::BIGINT \
             FROM file_ownership GROUP BY owner_id ORDER BY owner_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(pg_err)?;

        // Count each distinct file once; claims agree on the size (it is
        // derived from the file's content), so any row's value works.
        let totals: Option<(i64, i64)> = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(logical_bytes), 0)::BIGINT \
             FROM (SELECT file_hash, MAX(logical_bytes) AS logical_bytes \
                   FROM file_ownership GROUP BY file_hash) AS per_file",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(pg_err)?;
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
