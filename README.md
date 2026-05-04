# OpenXet

A Rust implementation of a [Xet Protocol](https://huggingface.co/docs/xet/en/index)-compatible Content Addressable Storage (CAS) server with a web-based management UI. OpenXet provides content-addressed data storage with chunk-level deduplication, following the Xet Protocol Specification v1.0.0.

## Overview

OpenXet breaks files into content-defined chunks using a Gearhash CDC algorithm, hashes them with Blake3, and stores them in deduplicated xorb archives. Files are reconstructed by looking up chunk references stored in shard metadata. This enables efficient storage and transfer of large files with automatic deduplication at the chunk level.

### Key Features

- **Content-Defined Chunking** -- Gearhash-based CDC (8--128 KiB chunks, 64 KiB target) for stable chunk boundaries across file revisions
- **Content-Addressed Storage** -- Blake3 keyed hashing with aggregated merkle trees for xorb and file identification
- **Chunk-Level Deduplication** -- Global dedup via HMAC-protected chunk hash queries
- **Binary Formats** -- Xorb (chunk archive) and Shard (file metadata) serialization with LZ4 frame compression
- **Web UI** -- React dashboard for browsing files, inspecting xorbs, uploading data, and querying tabular files with DuckDB WASM
- **Docker Support** -- Multi-stage Dockerfile and Docker Compose for single-command deployment

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (latest stable)
- [mise](https://mise.jdx.dev/) (recommended for toolchain management)
- [bun](https://bun.sh/) (for frontend)

### Setup

```bash
# Clone the repository
git clone https://github.com/ggoggam/openxet.git
cd openxet

# Install toolchain via mise (optional but recommended)
mise trust
mise install

# Build server and frontend
cargo build
cd web && bun install && bun run build && cd ..
```

### Running

```bash
cargo run          # Run the server (serves API + static frontend on port 8080)
```

### Running with Docker

```bash
docker compose -f docker/compose.yaml up -d --build
```

The server will be available at `http://localhost:8080`. Set `OPENXET_AUTH_SECRET` in the compose file for JWT authentication.

### Testing

```bash
cargo test         # Run all tests
cargo test --lib   # Unit tests only
cargo clippy       # Lint
cargo fmt --check  # Check formatting
```

## Architecture

OpenXet is organized as a Cargo workspace with four crates:

```
openxet/
├── crates/
│   ├── hashing/       # MerkleHash, Blake3 keyed hashing, aggregated merkle tree
│   ├── chunking/      # Gearhash content-defined chunking (CDC)
│   ├── cas_types/     # Xorb/Shard binary formats, chunk compression, reconstruction types
│   └── server/        # HTTP server (axum) with auth, storage, and management API
├── web/               # React frontend (TypeScript, Vite, TailwindCSS)
├── docker/            # Dockerfile and Docker Compose
├── spec/              # Protocol specification
└── test_data/         # Reference files from xet-spec-reference-files
```

### Crate Dependency Graph

```
server
  ├── cas_types
  │     └── hashing
  ├── chunking
  └── hashing
```

### API

#### CAS Protocol Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/reconstructions/{file_id}` | File reconstruction (supports Range header) |
| `GET` | `/v1/chunks/default-merkledb/{hash}` | Global chunk deduplication query |
| `GET` | `/v1/xorbs/default/{hash}` | Download a xorb |
| `POST` | `/v1/xorbs/default/{hash}` | Upload a serialized xorb |
| `POST` | `/v1/shards` | Upload shard metadata (registers files) |

#### Management API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/stats` | Storage statistics (file/xorb/shard counts, total size) |
| `GET` | `/api/files` | List all stored files |
| `GET` | `/api/files/{hash}` | File detail with reconstruction info |
| `GET` | `/api/files/{hash}/content` | Download reconstructed file content |
| `GET` | `/api/xorbs` | List all xorbs with chunk counts |
| `POST` | `/api/upload` | Single-shot file upload (chunks, hashes, stores) |

#### Multipart Upload API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/upload/init` | Initialize an upload session |
| `PUT` | `/api/upload/{session_id}/{part_index}` | Upload a part |
| `POST` | `/api/upload/{session_id}/complete` | Finalize the upload |
| `DELETE` | `/api/upload/{session_id}` | Abort the upload |

### Storage Layout

```
{data_dir}/
├── xorbs/default/{hash}    # Chunk archives
├── shards/{hash}           # File metadata
├── index/
│   ├── files/              # file_hash → shard_hash mapping
│   └── chunks/             # chunk_hash → (xorb_hash, chunk_index) mapping
└── uploads/tmp/            # Temporary multipart upload files
```

## Frontend

The web UI is a React SPA built with TypeScript, Vite, and TailwindCSS. It provides:

- **Dashboard** -- Storage statistics overview
- **File browser** -- List files, view reconstruction details, download content
- **Xorb inspector** -- Browse xorb archives and chunk metadata
- **File upload** -- Drag-and-drop upload with automatic chunking and deduplication
- **Table preview** -- Query CSV/Parquet files in-browser using DuckDB WASM with a SQL editor

### Frontend Stack

React 19, TypeScript, Vite 7, TailwindCSS 4, TanStack Router + Query, Radix UI / shadcn, DuckDB WASM, CodeMirror (SQL editor)

### Development

```bash
cd web
bun install        # Install dependencies
bun run dev        # Dev server with HMR
bun run build      # Production build (output: web/dist/)
bun run lint       # ESLint
```

## Protocol Details

OpenXet implements several non-trivial aspects of the Xet protocol:

- **Hash encoding** -- 32-byte hashes are hex-encoded with LE octet reversal per 8-byte segment
- **Merkle tree** -- Variable-branching aggregated tree (mean branching factor 4), not a flat hash
- **Chunk compression** -- Per-chunk LZ4 frame compression (not raw blocks)
- **Shard format** -- Magic tag `"HFRepoMetaData\0"` + sentinel bytes; upload shards omit the footer

See [`spec/SPECIFICATION.md`](spec/SPECIFICATION.md) for the full protocol specification.

## Development

### Mise Tasks

The project uses [mise](https://mise.jdx.dev/) for task automation:

```bash
mise run build           # Build all crates (debug)
mise run build-release   # Build all crates (release)
mise run test            # Run all tests
mise run lint            # Run clippy
mise run check           # Format check + clippy + tests
mise run dev             # Build everything and run the server
mise run fe-build        # Build frontend
mise run jwt             # Generate a JWT token for testing
mise run up              # Docker compose up
mise run down            # Docker compose down
```

### Code Conventions

- Error types via `thiserror`; application errors via `anyhow`
- Async runtime: `tokio`; HTTP framework: `axum`
- All binary formats use little-endian byte order
- Shard entries are fixed at 48 bytes (FileInfo and CASInfo)

### Reference Test Data

Integration tests validate against official reference files from the [xet-spec-reference-files](https://huggingface.co/datasets/xet-team/xet-spec-reference-files) dataset on HuggingFace. These live in `test_data/` and cover chunk hashing, file hashing, merkle tree construction, and xorb/shard deserialization.

## License

This project is licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0).
