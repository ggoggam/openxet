# OpenXet: Xet Protocol CAS Server — Specification & Implementation Plan

## 1. Project Goal

Build a Rust server implementing the [Xet Protocol v1.0.0](https://huggingface.co/docs/xet/en/index) CAS (Content Addressable Storage) API, plus a React static frontend for browsing stored files. The server is a self-contained, independent CAS that speaks the same wire protocol as HuggingFace's Xet infrastructure, allowing any Xet-compatible client (xet-core, huggingface.js) to upload and download files.

---

## 2. Protocol Summary

The Xet protocol provides content-addressed storage with chunk-level deduplication. Files are split into variable-size chunks via deterministic content-defined chunking (Gearhash CDC), grouped into compressed containers called **xorbs**, and referenced by **shards** that contain file reconstruction metadata.

### Object Hierarchy

```
File → [Chunk, Chunk, ...] → [Xorb, Xorb, ...] → Shard
```

- **Chunk**: ~64 KiB slice of file data (min 8 KiB, max 128 KiB). Hash: blake3 keyed with DATA_KEY.
- **Xorb**: Sequence of compressed chunks (≤64 MiB serialized). Hash: Merkle tree root of chunk hashes.
- **Shard**: Binary object containing file reconstruction info + xorb metadata. Used for upload registration and global dedup responses.
- **File Reconstruction**: Ordered list of terms `(xorb_hash, chunk_range)` that reconstruct a file by concatenation.

---

## 3. CAS API Specification

Base URL: `{server_address}/v1`

### 3.1 GET /v1/reconstructions/{file_id}

Retrieve file reconstruction metadata for downloading a file.

**Path params:**
- `file_id`: 64-char lowercase hex string (hash with little-endian octet reversal)

**Headers:**
- `Authorization: Bearer <token>` (minimum scope: `read`)
- `Range: bytes={start}-{end}` (optional, inclusive end)

**Response (200):** `application/json`
```json
{
  "offset_into_first_range": <number>,
  "terms": [
    {
      "hash": "<xorb_hash_hex_64>",
      "unpacked_length": <number>,
      "range": { "start": <number>, "end": <number> }
    }
  ],
  "fetch_info": {
    "<xorb_hash>": [
      {
        "range": { "start": <number>, "end": <number> },
        "url": "<presigned_url>",
        "url_range": { "start": <number>, "end": <number> }
      }
    ]
  }
}
```

**Errors:** 400 (bad file_id), 401 (auth), 404 (file not found), 416 (range not satisfiable)

**Implementation notes:**
- Look up file_id in the file index → find the shard containing this file's reconstruction
- Deserialize the shard's FileInfo section to build the terms list
- For each xorb referenced in the terms, generate a presigned URL (or direct URL with auth) pointing to the xorb data with the correct byte range
- For range requests: compute which terms/chunks overlap the requested byte range, set `offset_into_first_range` accordingly, trim terms

### 3.2 GET /v1/chunks/default-merkledb/{hash}

Query global chunk deduplication.

**Path params:**
- `hash`: 64-char lowercase hex chunk hash

**Headers:**
- `Authorization: Bearer <token>` (minimum scope: `read`)

**Response (200):** `application/octet-stream` — shard binary (with footer, CAS info only, HMAC-protected chunk hashes)

**Response (404):** Chunk not tracked

**Implementation notes:**
- Look up chunk hash in the global chunk index
- If found, build a shard response containing the CAS info blocks for xorbs that contain this chunk (and nearby chunks)
- Generate a random HMAC key, HMAC-protect all chunk hashes in the response, include key in footer
- Set `shard_key_expiry` to current time + configurable TTL (e.g., 7 days)

### 3.3 POST /v1/xorbs/default/{hash}

Upload a serialized xorb.

**Path params:**
- `hash`: 64-char lowercase hex xorb hash

**Headers:**
- `Authorization: Bearer <token>` (minimum scope: `write`)
- `Content-Type: application/octet-stream`

**Body:** Serialized xorb bytes

**Response (200):** `application/json`
```json
{ "was_inserted": true }
```
(`was_inserted: false` if xorb already exists — idempotent)

**Errors:** 400 (bad hash, hash mismatch, bad format, >64 MiB), 401, 403

**Implementation notes:**
- Validate hash format
- Read body, verify serialized size ≤ 64 MiB
- Parse xorb: iterate chunk headers, decompress each chunk, compute chunk hashes, compute merkle tree → verify xorb hash matches path
- Store xorb bytes content-addressed
- Index each chunk hash → (xorb_hash, chunk_index) for dedup lookups

### 3.4 POST /v1/shards

Upload a shard (register files).

**Headers:**
- `Authorization: Bearer <token>` (minimum scope: `write`)
- `Content-Type: application/octet-stream`

**Body:** Serialized shard bytes (footer MUST be omitted)

**Response (200):** `application/json`
```json
{ "result": 0 | 1 }
```
(0 = already exists, 1 = registered)

**Errors:** 400 (bad format, referenced xorb missing, >64 MiB, verification failure), 401, 403

**Implementation notes:**
- Parse shard header (verify tag + version=2, footer_size=0)
- Parse FileInfo section: for each file, read FileDataSequenceHeader, entries, verification entries, metadata ext
- Validate verification entries: for each term, recompute verification hash from stored xorb chunk hashes and compare
- Validate that all referenced xorbs exist in storage
- Parse CAS Info section: register chunk→xorb mappings for future dedup queries
- Index file_hash → shard for future reconstruction lookups
- Store shard content-addressed

---

## 4. Binary Format Details

### 4.1 Hash String Encoding

32-byte hash → 64-char hex string with little-endian octet reversal per 8-byte segment:

```
bytes[0..8]  → reverse → hex (16 chars)
bytes[8..16] → reverse → hex (16 chars)
bytes[16..24] → reverse → hex (16 chars)
bytes[24..32] → reverse → hex (16 chars)
concatenate
```

Example: `[0,1,2,...,31]` → `"07060504030201000f0e0d0c0b0a09081716151413121110..."` (NOT direct hex)

### 4.2 Xorb Binary Format

Sequence of chunks, each: `[ChunkHeader (8 bytes)][CompressedData]`

**ChunkHeader:**
| Offset | Size | Field | Encoding |
|--------|------|-------|----------|
| 0 | 1 | version | u8 (currently 0) |
| 1 | 3 | compressed_size | u24 little-endian |
| 4 | 1 | compression_type | u8 (0=None, 1=LZ4, 2=BG4LZ4) |
| 5 | 3 | uncompressed_size | u24 little-endian |

**Compression types:**
- `0` (None): Raw bytes
- `1` (LZ4): Standard LZ4 block compression
- `2` (BG4LZ4): Byte-grouping by position within 4-byte groups, then LZ4. Groups bytes [A1,A2,A3,A4,B1,B2,B3,B4,...] → [A1,B1,...,A2,B2,...,A3,B3,...,A4,B4,...] then LZ4-compresses

### 4.3 Shard Binary Format

```
[Header 48B][FileInfo Section][CAS Info Section][Footer 200B (optional)]
```

**Header (48 bytes):**
| Offset | Size | Field |
|--------|------|-------|
| 0 | 32 | MDB_SHARD_HEADER_TAG magic bytes |
| 32 | 8 | version (u64 LE, must be 2) |
| 40 | 8 | footer_size (u64 LE, 0 if omitted) |

**FileInfo Section:** sequence of file blocks + bookend

Each file block:
1. `FileDataSequenceHeader` (48B): file_hash(32) + file_flags(u32) + num_entries(u32) + unused(8)
2. `num_entries` × `FileDataSequenceEntry` (48B each): cas_hash(32) + cas_flags(u32) + unpacked_segment_bytes(u32) + chunk_index_start(u32) + chunk_index_end(u32)
3. If flag `0x80000000`: `num_entries` × `FileVerificationEntry` (48B each): range_hash(32) + unused(16)
4. If flag `0x40000000`: 1 × `FileMetadataExt` (48B): sha256(32) + unused(16)

Bookend: 32 bytes `0xFF` + 16 bytes `0x00`

**CAS Info Section:** sequence of xorb blocks + bookend

Each xorb block:
1. `CASChunkSequenceHeader` (48B): cas_hash(32) + cas_flags(u32) + num_entries(u32) + num_bytes_in_cas(u32) + num_bytes_on_disk(u32)
2. `num_entries` × `CASChunkSequenceEntry` (48B each): chunk_hash(32) + chunk_byte_range_start(u32) + unpacked_segment_bytes(u32) + unused(8)

Bookend: 32 bytes `0xFF` + 16 bytes `0x00`

**Footer (200 bytes):** version(8) + file_info_offset(8) + cas_info_offset(8) + reserved(48) + hmac_key(32) + creation_timestamp(8) + key_expiry(8) + reserved(72) + footer_offset(8)

### 4.4 Hashing Functions

All use Blake3 keyed hash producing 32-byte output.

| Hash Type | Key | Input |
|-----------|-----|-------|
| Chunk | DATA_KEY `[102,151,245,119,91,149,80,222,49,53,203,172,165,151,24,28,157,228,33,16,155,235,43,88,180,208,176,75,147,173,242,41]` | Raw chunk bytes |
| Merkle Internal Node | INTERNAL_NODE_KEY `[1,126,197,199,165,71,41,150,253,148,102,102,180,138,2,230,93,221,83,111,55,199,109,210,248,99,82,230,74,83,113,63]` | `"{hash:x} : {size}\n"` for each child chunk |
| Xorb | (merkle tree root using above) | Merkle tree of chunk hashes |
| File | Zero key (32 × 0x00) | Blake3 keyed hash of merkle tree root bytes |
| Term Verification | VERIFICATION_KEY `[127,24,87,214,206,86,237,102,18,127,249,19,231,165,195,243,164,205,38,213,181,219,73,230,65,36,152,127,40,251,148,195]` | Concatenated raw chunk hash bytes for the term range |

### 4.5 Chunking Algorithm (Gearhash CDC)

```
Constants:
  MIN_CHUNK_SIZE = 8192 (8 KiB)
  MAX_CHUNK_SIZE = 131072 (128 KiB)
  MASK = 0xFFFF000000000000
  TABLE[256]: 64-bit constants from rust-gearhash

State: h = 0u64, start_offset = 0

For each byte b at position i:
  h = (h << 1).wrapping_add(TABLE[b])

  if (i - start_offset) < MIN_CHUNK_SIZE: continue
  if (i - start_offset) >= MAX_CHUNK_SIZE OR (h & MASK) == 0:
    emit chunk [start_offset, i+1)
    start_offset = i + 1
    h = 0

At EOF: emit remaining [start_offset, len) if start_offset < len

Optimization: skip hash testing for first (MIN_CHUNK_SIZE - 64 - 1) bytes of each chunk
```

---

## 5. Authentication Design

### Self-Hosted Token Model

Since this is a standalone server (not HuggingFace Hub), we implement our own token system:

**Token endpoint:** `POST /auth/token`
```json
Request:  { "scope": "read" | "write", "repo": "<repo_id>" }
Response: { "accessToken": "<jwt>", "exp": <unix_ts>, "casUrl": "<base_url>" }
```

- Tokens are JWTs signed with a server-side secret
- Claims: `scope` (read/write), `repo`, `exp`
- Write scope supersedes read
- Middleware validates Bearer token on all `/v1/*` routes

For HuggingFace Hub compatibility, clients can also use tokens obtained from `https://huggingface.co/api/{repo_type}s/{repo_id}/xet-{token_type}-token/{revision}` if the server is configured as a HF-compatible backend.

---

## 6. Storage Backend

### Trait Definition

```rust
#[async_trait]
trait StorageBackend: Send + Sync {
    async fn get_xorb(&self, hash: &str) -> Result<Bytes>;
    async fn get_xorb_range(&self, hash: &str, start: u64, end: u64) -> Result<Bytes>;
    async fn put_xorb(&self, hash: &str, data: Bytes) -> Result<bool>; // true = inserted
    async fn xorb_exists(&self, hash: &str) -> Result<bool>;
    async fn get_shard(&self, hash: &str) -> Result<Bytes>;
    async fn put_shard(&self, hash: &str, data: Bytes) -> Result<bool>;
}
```

### Filesystem Backend (Primary)

```
{data_dir}/
├── xorbs/
│   └── default/
│       └── {hash}           # Raw xorb bytes
├── shards/
│   └── {hash}               # Raw shard bytes
└── index/
    ├── files/
    │   └── {file_hash}      # → shard_hash (file → shard lookup)
    └── chunks/
        └── {chunk_hash}     # → (xorb_hash, chunk_index) JSON
```

### S3 Backend (Optional/Future)

Same key structure, uses `aws-sdk-s3` with presigned URL generation for fetch_info URLs.

---

## 7. Index Design

### File Index

Maps `file_hash → shard_hash` for reconstruction lookups.

On shard upload, for each FileInfo block, extract `file_hash` and store the mapping.

### Chunk Index

Maps `chunk_hash → Vec<(xorb_hash, chunk_index)>` for deduplication.

On xorb upload, parse each chunk, compute hash, index it. On shard upload, also index from CAS Info section.

### Implementation

Phase 1: File-based index (JSON files in `{data_dir}/index/`)
Phase 2: Embedded database (SQLite via `rusqlite` or sled/redb) for performance at scale

---

## 8. React Frontend

### Purpose

Simple web UI for browsing and managing files stored in the CAS server. Served as static files by the Rust server.

### Tech Stack

- React 18+ with TypeScript
- Vite for build tooling
- TailwindCSS for styling
- React Router for client-side routing

### Pages

1. **Dashboard** (`/`): Overview of stored files count, total storage, recent uploads
2. **Files** (`/files`): List all stored files with hash, size, upload date. Search/filter support
3. **File Detail** (`/files/:hash`): Show reconstruction terms, referenced xorbs, file metadata (SHA256, size)
4. **Xorbs** (`/xorbs`): Browse stored xorbs with chunk counts and sizes
5. **Upload** (`/upload`): Drag-and-drop file upload that performs client-side chunking → xorb creation → shard upload (demonstrates the full upload protocol)

### Frontend API

The server exposes additional management endpoints (prefixed `/api/`) for the frontend:

```
GET  /api/files                    # List all files
GET  /api/files/:hash              # File detail with reconstruction
GET  /api/xorbs                    # List all xorbs
GET  /api/stats                    # Storage statistics
POST /api/upload                   # Simple file upload (server performs chunking)
```

### Static File Serving

The Rust server serves `frontend/dist/` at `/` with SPA fallback (all non-API/non-v1 routes serve `index.html`).

---

## 9. Implementation Plan

### Phase 1: Core Types & Hashing (crates: `cas_types`, `hashing`, `chunking`)

1. **cas_types**: Define `MerkleHash` type with little-endian octet reversal hex encoding/decoding
2. **hashing**: Implement all 4 hash functions (chunk, merkle internal node, file, verification) using `blake3` crate with keyed hashing
3. **chunking**: Implement Gearhash CDC with TABLE[256], MASK, min/max enforcement, skip-ahead optimization
4. **cas_types**: Xorb format serializer/deserializer (chunk header parsing, LZ4 decompression via `lz4_flex`, BG4LZ4)
5. **cas_types**: Shard format serializer/deserializer (header, file info, CAS info, footer, bookend detection)
6. **Validation**: Test against reference files from `xet-team/xet-spec-reference-files`

### Phase 2: Storage & Index (`crates/server/src/storage/`)

7. Implement `StorageBackend` trait + filesystem backend
  a. Storage backend should support blob stores, most notably S3, MinIO, GCS, and Azure Blob. It should share the same interface.
8. File index (file_hash → shard_hash)
9. Chunk index (chunk_hash → xorb locations)

### Phase 3: Server & API (`crates/server/`)

10. Axum server scaffold with config, graceful shutdown, logging (tracing)
11. Auth middleware (JWT generation + validation)
12. `POST /v1/xorbs/default/{hash}` — xorb upload with validation
13. `POST /v1/shards` — shard upload with xorb existence checks and verification hash validation
14. `GET /v1/reconstructions/{file_id}` — file reconstruction with range support and presigned/direct URLs
15. `GET /v1/chunks/default-merkledb/{hash}` — global dedup with HMAC-protected shard responses
16. Error handling (retryable vs non-retryable), rate limiting

### Phase 4: Frontend (`frontend/`)

17. Vite + React + TypeScript + TailwindCSS scaffold
  a. Stick with commonly used UI lib such as ShadCN.
  b. Use Tanstack router for file-based routing.
18. Dashboard, Files list, File detail pages
  a. Should bear similarities with Github UI.
19. Xorbs browser page
20. Upload page with client-side chunking demonstration
21. Static file serving integration in Rust server

### Phase 5: Testing & Polish

22. Integration tests: full upload→download round-trip
23. Integration tests: deduplication flow
24. Integration tests: range downloads
25. Performance testing with large files
26. Configuration (CLI args, env vars, config file)
27. Docker support

---

## 10. Dependencies

### Rust

| Crate | Purpose |
|-------|---------|
| `axum` | HTTP framework |
| `tokio` | Async runtime |
| `blake3` | Keyed hashing (chunk, merkle, file, verification) |
| `lz4_flex` | LZ4 compression/decompression |
| `serde` + `serde_json` | JSON serialization |
| `jsonwebtoken` | JWT auth tokens |
| `bytes` | Efficient byte buffer handling |
| `tracing` + `tracing-subscriber` | Structured logging |
| `tower-http` | CORS, compression, static file serving |
| `clap` | CLI argument parsing |
| `thiserror` | Error types |
| `anyhow` | Application-level errors |
| `hmac` + `sha2` | HMAC for dedup response protection |
| `hex` | Hex encoding/decoding |
| `uuid` | Request IDs |

### Frontend

| Package | Purpose |
|---------|---------|
| `react` + `react-dom` | UI framework |
| `react-router-dom` | Client-side routing |
| `vite` | Build tool |
| `tailwindcss` | Styling |
| `@tanstack/react-query` | Data fetching + caching |
| `typescript` | Type safety |

---

## 11. Configuration

```toml
# openxet.toml
[server]
host = "0.0.0.0"
port = 8080
frontend_dir = "./frontend/dist"

[storage]
backend = "filesystem"       # "filesystem" | "s3"
data_dir = "./.data"

[auth]
secret = "change-me-in-production"
token_ttl_seconds = 3600

[s3]  # only if backend = "s3"
bucket = "openxet-data"
region = "us-east-1"
endpoint = ""               # custom endpoint for MinIO etc.
```

---

## 12. Key Correctness Requirements

These are the critical invariants from the spec that the server MUST enforce:

1. **All xorbs referenced by a shard MUST exist before shard upload succeeds** (400 if any missing)
2. **Xorb hash in URL MUST match computed hash from body** (400 if mismatch)
3. **Serialized xorb size MUST be ≤ 64 MiB** (400 if exceeded)
4. **Shard upload body MUST NOT contain footer** (footer_size in header must be 0)
5. **Verification hashes in shard MUST match** (recompute from stored chunk hashes)
6. **All uploads are idempotent** — re-uploading existing xorb/shard returns success
7. **Hash string encoding uses LE octet reversal** — not direct byte-to-hex
8. **Chunk boundaries are deterministic** — same input → same boundaries everywhere
9. **FileMetadataExt is REQUIRED per file in shard uploads** (contains SHA256)
10. **FileVerificationEntry is REQUIRED for all files in shard uploads**
