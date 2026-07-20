//! Index for the S3-compatible read gateway: the path-addressed
//! `(bucket, key) → file_hash` mapping and the SigV4 credential store.
//!
//! Both live in the same index database (SQLite node-local, or shared Postgres)
//! as the file/chunk indexes, so a replica set sees one consistent set of S3
//! object names and credentials.

use sqlx::{PgPool, SqlitePool};

use super::super::error::StorageError;
use super::{S3Credential, S3Object};

fn err(e: sqlx::Error) -> StorageError {
    StorageError::Index(e.to_string())
}

/// Escape LIKE metacharacters in a user-supplied prefix so it matches
/// literally, then append `%`. Paired with `ESCAPE '\'` in the query. Without
/// this, a key prefix containing `%` or `_` would match unintended objects.
fn like_prefix_pattern(prefix: &str) -> String {
    let mut pat = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pat.push('\\');
        }
        pat.push(c);
    }
    pat.push('%');
    pat
}

/// SQLite-backed S3 gateway index. The node-local default.
pub struct SqliteS3Index {
    pool: SqlitePool,
}

impl SqliteS3Index {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<S3Object>, StorageError> {
        let row: Option<(String, i64, String, String, i64)> = sqlx::query_as(
            "SELECT file_hash, size, etag, owner_id, last_modified \
             FROM s3_objects WHERE bucket = $1 AND key = $2",
        )
        .bind(bucket)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?;
        Ok(row.map(
            |(file_hash, size, etag, owner_id, last_modified)| S3Object {
                bucket: bucket.to_string(),
                key: key.to_string(),
                file_hash,
                size: size as u64,
                etag,
                owner_id,
                last_modified,
            },
        ))
    }

    pub async fn put_object(&self, obj: &S3Object) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO s3_objects \
                 (bucket, key, file_hash, size, etag, owner_id, last_modified) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (bucket, key) DO UPDATE SET \
                 file_hash = EXCLUDED.file_hash, size = EXCLUDED.size, \
                 etag = EXCLUDED.etag, owner_id = EXCLUDED.owner_id, \
                 last_modified = EXCLUDED.last_modified",
        )
        .bind(&obj.bucket)
        .bind(&obj.key)
        .bind(&obj.file_hash)
        .bind(obj.size as i64)
        .bind(&obj.etag)
        .bind(&obj.owner_id)
        .bind(obj.last_modified)
        .execute(&self.pool)
        .await
        .map_err(err)?;
        Ok(())
    }

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<S3Object>, StorageError> {
        let pattern = like_prefix_pattern(prefix);
        let rows: Vec<(String, String, i64, String, String, i64)> = sqlx::query_as(
            "SELECT key, file_hash, size, etag, owner_id, last_modified \
             FROM s3_objects \
             WHERE bucket = $1 AND key LIKE $2 ESCAPE '\\' \
                   AND ($3 IS NULL OR key > $3) \
             ORDER BY key LIMIT $4",
        )
        .bind(bucket)
        .bind(&pattern)
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        Ok(rows
            .into_iter()
            .map(
                |(key, file_hash, size, etag, owner_id, last_modified)| S3Object {
                    bucket: bucket.to_string(),
                    key,
                    file_hash,
                    size: size as u64,
                    etag,
                    owner_id,
                    last_modified,
                },
            )
            .collect())
    }

    pub async fn get_credential(
        &self,
        access_key_id: &str,
    ) -> Result<Option<S3Credential>, StorageError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT secret_key, owner_id FROM s3_credentials WHERE access_key_id = $1",
        )
        .bind(access_key_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?;
        Ok(row.map(|(secret_key, owner_id)| S3Credential {
            access_key_id: access_key_id.to_string(),
            secret_key,
            owner_id,
        }))
    }

    pub async fn put_credential(&self, cred: &S3Credential) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO s3_credentials (access_key_id, secret_key, owner_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (access_key_id) DO UPDATE SET \
                 secret_key = EXCLUDED.secret_key, owner_id = EXCLUDED.owner_id",
        )
        .bind(&cred.access_key_id)
        .bind(&cred.secret_key)
        .bind(&cred.owner_id)
        .execute(&self.pool)
        .await
        .map_err(err)?;
        Ok(())
    }
}

/// Postgres-backed S3 gateway index, shared across replicas.
pub struct PostgresS3Index {
    pool: PgPool,
}

impl PostgresS3Index {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<S3Object>, StorageError> {
        let row: Option<(String, i64, String, String, i64)> = sqlx::query_as(
            "SELECT file_hash, size, etag, owner_id, last_modified \
             FROM s3_objects WHERE bucket = $1 AND key = $2",
        )
        .bind(bucket)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?;
        Ok(row.map(
            |(file_hash, size, etag, owner_id, last_modified)| S3Object {
                bucket: bucket.to_string(),
                key: key.to_string(),
                file_hash,
                size: size as u64,
                etag,
                owner_id,
                last_modified,
            },
        ))
    }

    pub async fn put_object(&self, obj: &S3Object) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO s3_objects \
                 (bucket, key, file_hash, size, etag, owner_id, last_modified) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (bucket, key) DO UPDATE SET \
                 file_hash = EXCLUDED.file_hash, size = EXCLUDED.size, \
                 etag = EXCLUDED.etag, owner_id = EXCLUDED.owner_id, \
                 last_modified = EXCLUDED.last_modified",
        )
        .bind(&obj.bucket)
        .bind(&obj.key)
        .bind(&obj.file_hash)
        .bind(obj.size as i64)
        .bind(&obj.etag)
        .bind(&obj.owner_id)
        .bind(obj.last_modified)
        .execute(&self.pool)
        .await
        .map_err(err)?;
        Ok(())
    }

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<S3Object>, StorageError> {
        let pattern = like_prefix_pattern(prefix);
        let rows: Vec<(String, String, i64, String, String, i64)> = sqlx::query_as(
            "SELECT key, file_hash, size, etag, owner_id, last_modified \
             FROM s3_objects \
             WHERE bucket = $1 AND key LIKE $2 ESCAPE '\\' \
                   AND ($3::text IS NULL OR key > $3) \
             ORDER BY key LIMIT $4",
        )
        .bind(bucket)
        .bind(&pattern)
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(err)?;
        Ok(rows
            .into_iter()
            .map(
                |(key, file_hash, size, etag, owner_id, last_modified)| S3Object {
                    bucket: bucket.to_string(),
                    key,
                    file_hash,
                    size: size as u64,
                    etag,
                    owner_id,
                    last_modified,
                },
            )
            .collect())
    }

    pub async fn get_credential(
        &self,
        access_key_id: &str,
    ) -> Result<Option<S3Credential>, StorageError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT secret_key, owner_id FROM s3_credentials WHERE access_key_id = $1",
        )
        .bind(access_key_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(err)?;
        Ok(row.map(|(secret_key, owner_id)| S3Credential {
            access_key_id: access_key_id.to_string(),
            secret_key,
            owner_id,
        }))
    }

    pub async fn put_credential(&self, cred: &S3Credential) -> Result<(), StorageError> {
        sqlx::query(
            "INSERT INTO s3_credentials (access_key_id, secret_key, owner_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (access_key_id) DO UPDATE SET \
                 secret_key = EXCLUDED.secret_key, owner_id = EXCLUDED.owner_id",
        )
        .bind(&cred.access_key_id)
        .bind(&cred.secret_key)
        .bind(&cred.owner_id)
        .execute(&self.pool)
        .await
        .map_err(err)?;
        Ok(())
    }
}
