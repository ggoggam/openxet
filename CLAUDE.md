# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

OpenXet is a Rust implementation of a Xet protocol-compatible Content Addressable Storage (CAS) server with a React static frontend. It implements the [Xet Protocol Specification v1.0.0](https://huggingface.co/docs/xet/en/index) for content-addressed data storage with chunk-level deduplication.

## Build & Development Commands

### Prerequisites

- Rust toolchain (managed via `mise`): `mise install`
- Bun (for frontend): `mise install`

### Rust Server

```bash
cargo build                          # Build debug
cargo build --release                # Build release
cargo run                            # Run server (debug)
cargo test                           # Run all tests
cargo test --lib                     # Unit tests only
cargo test --test <name>             # Single integration test
cargo test <test_fn_name>            # Single test by name
cargo clippy                         # Lint
cargo fmt --check                    # Format check
cargo fmt                            # Auto-format
```

### React Frontend

```bash
cd web
bun install                          # Install dependencies
bun run dev                          # Dev server with HMR
bun run build                        # Production build (output: web/dist/)
bun run lint                         # ESLint
bun run preview                      # Preview production build
```

### Full Stack

```bash
cargo run                            # Serves API + static frontend from web/dist/
```

## Architecture

### Crate Structure

```
openxet/
├── Cargo.toml                       # Workspace root
├── crates/
│   ├── server/                      # HTTP server (axum) - binary crate
│   ├── cas_types/                   # Shared types - library crate
│   │   └── src/
│   │       ├── xorb.rs              # Xorb serialization/deserialization
│   │       ├── shard.rs             # Shard binary format
│   │       ├── chunk.rs             # Chunk header, compression (None/LZ4/BG4LZ4)
│   │       └── reconstruction.rs    # QueryReconstructionResponse types
│   ├── chunking/                    # CDC algorithm - library crate
│   │   └── src/gearhash.rs          # Gearhash CDC with TABLE[256]
│   └── hashing/                     # Hashing functions - library crate
│       └── src/
│           ├── merkle_hash.rs       # MerkleHash type with LE octet reversal hex encoding
│           ├── chunk_hash.rs        # Blake3 keyed hash with DATA_KEY
│           ├── merkle_tree.rs       # Variable-branching aggregated merkle tree
│           ├── file_hash.rs         # File hash (merkle root + blake3 with zero key)
│           └── verification.rs      # Term verification hash with VERIFICATION_KEY
├── web/                             # React frontend (TypeScript, Vite, TailwindCSS)
├── docker/                          # Dockerfile and Docker Compose
├── test_data/                       # Reference files from xet-spec-reference-files
└── spec/SPECIFICATION.md            # Full protocol specification
```

### Critical Implementation Details

These are non-obvious details discovered during implementation and validated against reference files:

1. **Hash string encoding**: Each 8-byte segment of the 32-byte hash is treated as a little-endian u64 (bytes reversed within each segment) before hex encoding. `[0,1,2,3,4,5,6,7,...]` → `"0706050403020100..."`. See `hashing::merkle_hash::MerkleHash::to_hex()`.

2. **Merkle tree is NOT flat**: The xorb/file hash uses a variable-branching aggregated merkle tree (not a single flat hash). Groups of 2-8 entries are merged per level, with cut points determined by `hash_bytes[24..32] as u64 % 4 == 0`. See `hashing::merkle_tree`. This matches xet-core's `AGGREGATED_HASHES_MEAN_TREE_BRANCHING_FACTOR = 4`.

3. **LZ4 uses framed format**: Chunk compression in xorbs uses LZ4 **frame** format (`lz4_flex::frame::FrameDecoder`), NOT raw LZ4 blocks. The `compressed_size` in the chunk header includes the frame overhead.

4. **Shard magic tag**: `"HFRepoMetaData\0"` + 17 sentinel bytes. NOT `"\0MDBShardFile"`.

5. **Shard footer omission**: Upload shards (`POST /v1/shards`) MUST have `footer_size = 0` in header and no footer bytes.

6. **Chunk compression per-chunk**: Each chunk in a xorb has its own 8-byte header specifying its compression type independently.

7. **Global dedup HMAC**: Dedup response shards have HMAC-protected chunk hashes (key in footer). Clients must HMAC their own hash to find matches.

### CAS API Endpoints

| Method | Path | Auth Scope | Description |
|--------|------|-----------|-------------|
| GET | `/v1/reconstructions/{file_id}` | read | File reconstruction with optional Range header |
| GET | `/v1/chunks/default-merkledb/{hash}` | read | Global chunk deduplication query |
| POST | `/v1/xorbs/default/{hash}` | write | Upload serialized xorb |
| POST | `/v1/shards` | write | Upload shard (registers files) |

### Storage Layout

- Xorbs: `{storage_root}/xorbs/default/{hash}`
- Shards: `{storage_root}/shards/{hash}`
- File index: maps file_hash → shard location for reconstruction lookups
- Chunk index: maps chunk_hash → (xorb_hash, chunk_index) for dedup lookups

### Hashing Constants (Blake3 Keyed)

- **DATA_KEY** (chunk hashing): `[102, 151, 245, 119, 91, 149, 80, 222, ...]`
- **INTERNAL_NODE_KEY** (merkle tree): `[1, 126, 197, 199, 165, 71, 41, 150, ...]`
- **VERIFICATION_KEY** (term verification): `[127, 24, 87, 214, 206, 86, 237, 102, ...]`
- **File hash key**: 32 zero bytes

### Chunking Parameters (Gearhash CDC)

- Target: 64 KiB, Min: 8 KiB, Max: 128 KiB
- Mask: `0xFFFF000000000000` (boundary probability 1/2^16)
- Skip-ahead: skip first `MIN_CHUNK_SIZE - 64 - 1` bytes before testing boundaries

## Code Conventions

- Use `thiserror` for error types, `anyhow` for application errors
- Async runtime: `tokio`
- HTTP framework: `axum`
- All binary formats use little-endian byte order
- Entry sizes are fixed at 48 bytes for both FileInfo and CASInfo entries in shards
- Bookend entries: 32 bytes of `0xFF` + 16 bytes of `0x00`
