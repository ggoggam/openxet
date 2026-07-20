-- S3-compatible read gateway (Phase 1): a path-addressed mapping layer
-- `(bucket, key) → file_hash` laid over the content-addressed CAS, plus the
-- credentials used to verify SigV4-signed S3 requests.
--
-- Never edit this file once it has run anywhere — sqlx checksums applied
-- migrations and fails startup on a mismatch. Schema changes go in new files.

-- One row per S3 object: a friendly (bucket, key) name pointing at an
-- already-uploaded file. `size`/`etag` are captured at registration so
-- HeadObject/GetObject can answer without reparsing the shard.
CREATE TABLE s3_objects (
    bucket        TEXT    NOT NULL,
    key           TEXT    NOT NULL,
    file_hash     TEXT    NOT NULL,   -- references file_index.file_hash in spirit
    size          INTEGER NOT NULL,   -- logical bytes, for Content-Length
    etag          TEXT    NOT NULL,   -- = file_hash for now (see plan)
    owner_id      TEXT    NOT NULL,
    last_modified INTEGER NOT NULL,   -- unix seconds
    PRIMARY KEY (bucket, key)
);

-- ListObjectsV2 keyset scan:
-- WHERE bucket = ? AND key > ? AND key LIKE prefix% ORDER BY key.
-- The primary key already leads with (bucket, key) and serves this, but an
-- explicit index documents the access pattern and survives PK changes.
CREATE INDEX s3_objects_bucket_key ON s3_objects (bucket, key);

-- SigV4 credentials: access-key-id → secret + accounting owner. Populated out
-- of band (admin/seed); Phase 1 has no self-service credential creation.
CREATE TABLE s3_credentials (
    access_key_id TEXT PRIMARY KEY,
    secret_key    TEXT NOT NULL,
    owner_id      TEXT NOT NULL
);
