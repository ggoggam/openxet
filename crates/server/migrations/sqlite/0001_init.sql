-- Baseline schema for the node-local SQLite index. Unlike the Postgres
-- baseline, there is no IF NOT EXISTS: SQLite databases are always created by
-- the migrator, never by older inline DDL.
--
-- Never edit this file once it has run anywhere — sqlx checksums applied
-- migrations and fails startup on a mismatch. Schema changes go in new files.

CREATE TABLE file_index (
    file_hash  TEXT PRIMARY KEY,
    shard_hash TEXT NOT NULL
);

-- The implicit rowid stands in for the Postgres schema's BIGSERIAL `seq`:
-- queries ORDER BY rowid to preserve insertion order for `get`. (Do not make
-- this a WITHOUT ROWID table.) The composite primary key enforces dedup
-- (same `put_batch` semantics).
CREATE TABLE chunk_index (
    chunk_hash TEXT    NOT NULL,
    xorb_hash  TEXT    NOT NULL,
    chunk_idx  INTEGER NOT NULL,
    PRIMARY KEY (chunk_hash, xorb_hash, chunk_idx)
);

-- GC removes a deleted xorb's entries by xorb_hash, which the primary key
-- (led by chunk_hash) cannot serve.
CREATE INDEX chunk_index_xorb ON chunk_index (xorb_hash);

-- Ownership claims: the accounting unit. One row per (file, owner); the
-- file itself stays in file_index until its last claim is released.
CREATE TABLE file_ownership (
    file_hash       TEXT    NOT NULL,
    owner_id        TEXT    NOT NULL,
    logical_bytes   INTEGER NOT NULL,
    created_at_unix INTEGER NOT NULL,
    PRIMARY KEY (file_hash, owner_id)
);

-- Composite (owner_id, file_hash): serves both the per-owner usage
-- aggregation and the keyset-paginated owner-filtered file listing
-- (WHERE owner_id = $ AND file_hash > $ ORDER BY file_hash).
CREATE INDEX file_ownership_owner_file
    ON file_ownership (owner_id, file_hash);

-- Xorb layouts are stored as one JSON row per xorb: dedup responses need the
-- whole layout at once and never query it by chunk, so a serialized blob is
-- simpler than a normalized table and reads in a single round-trip.
CREATE TABLE xorb_layout (
    xorb_hash TEXT PRIMARY KEY,
    layout    TEXT NOT NULL
);
