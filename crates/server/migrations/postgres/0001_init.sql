-- Baseline schema, moved verbatim from the inline DDL that init_schema()
-- used to run. IF NOT EXISTS is load-bearing: on a database provisioned by a
-- pre-migration build the statements are no-ops and the migrator simply
-- records this version as applied, baselining the deployment.
--
-- Never edit this file once it has run anywhere — sqlx checksums applied
-- migrations and fails startup on a mismatch. Schema changes go in new files.

CREATE TABLE IF NOT EXISTS file_index (
    file_hash  TEXT PRIMARY KEY,
    shard_hash TEXT NOT NULL
);

-- `seq` preserves insertion order for `get`, matching the SQLite backend's
-- rowid ordering; the composite primary key enforces dedup (same `put_batch`
-- semantics).
CREATE TABLE IF NOT EXISTS chunk_index (
    seq        BIGSERIAL,
    chunk_hash TEXT   NOT NULL,
    xorb_hash  TEXT   NOT NULL,
    chunk_idx  BIGINT NOT NULL,
    PRIMARY KEY (chunk_hash, xorb_hash, chunk_idx)
);

CREATE INDEX IF NOT EXISTS chunk_index_hash_seq ON chunk_index (chunk_hash, seq);

-- GC removes a deleted xorb's entries by xorb_hash, which the primary key
-- (led by chunk_hash) cannot serve.
CREATE INDEX IF NOT EXISTS chunk_index_xorb ON chunk_index (xorb_hash);

-- Ownership claims: the accounting unit. One row per (file, owner); the
-- file itself stays in file_index until its last claim is released.
CREATE TABLE IF NOT EXISTS file_ownership (
    file_hash       TEXT   NOT NULL,
    owner_id        TEXT   NOT NULL,
    logical_bytes   BIGINT NOT NULL,
    created_at_unix BIGINT NOT NULL,
    PRIMARY KEY (file_hash, owner_id)
);

-- Composite (owner_id, file_hash): serves both the per-owner usage
-- aggregation and the keyset-paginated owner-filtered file listing
-- (WHERE owner_id = $ AND file_hash > $ ORDER BY file_hash).
CREATE INDEX IF NOT EXISTS file_ownership_owner_file
    ON file_ownership (owner_id, file_hash);

-- Xorb layouts are stored as one JSON row per xorb: dedup responses need the
-- whole layout at once and never query it by chunk, so a serialized blob is
-- simpler than a normalized table and reads in a single round-trip.
CREATE TABLE IF NOT EXISTS xorb_layout (
    xorb_hash TEXT PRIMARY KEY,
    layout    TEXT NOT NULL
);
