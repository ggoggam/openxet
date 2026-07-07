# OpenXet

A Rust implementation of a [Xet Protocol](https://huggingface.co/docs/xet/en/index)-compatible Content Addressable Storage (CAS) server, plus a reference client, a browser upload pipeline, and a web UI. OpenXet provides content-addressed data storage with chunk-level deduplication, following the Xet Protocol Specification v1.0.0. It speaks the same `/v1` wire protocol as HuggingFace's `xet-core` / `hf_xet`, so those clients work against it unmodified.

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
- [wasm-pack](https://rustwasm.github.io/wasm-pack/) + the `wasm32-unknown-unknown` target (for the frontend's upload pipeline)

### Setup

```bash
# Clone the repository
git clone https://github.com/ggoggam/openxet.git
cd openxet

# Install toolchain via mise (optional but recommended)
mise trust
mise install

# Build server and frontend (fe:build also compiles the wasm upload pipeline)
cargo build
mise run fe:build
```

### Running

```bash
cargo run          # Run the server (serves API + static frontend on port 8080)
```

### Running with Docker

```bash
# Local filesystem backend
docker compose -f docker/compose.local.yaml up -d --build

# S3-compatible backend via RustFS (S3 API on :9000, console on :9001)
docker compose -f docker/compose.rustfs.yaml up -d --build
```

The server will be available at `http://localhost:8080`. For authentication, configure OIDC issuers (`auth.oidc_issuers`); clients then present bearer tokens from that provider. Leave `auth.enabled = false` for local/dev use.

### Testing

```bash
cargo test         # Run all tests
cargo test --lib   # Unit tests only
cargo clippy       # Lint
cargo fmt --check  # Check formatting
```

## Architecture

OpenXet is organized as a Cargo workspace with six crates:

```
openxet/
├── crates/
│   ├── hashing/       # MerkleHash, Blake3 keyed hashing, aggregated merkle tree
│   ├── chunking/      # Gearhash content-defined chunking (CDC)
│   ├── cas_types/     # Xorb/Shard binary formats, chunk compression, reconstruction types
│   ├── server/        # HTTP server (axum) with auth and storage — the /v1 CAS protocol
│   ├── client/        # openxet-client: reference CLI that uploads/downloads via /v1
│   └── wasm/          # openxet-wasm: chunk/hash/pack pipeline compiled to WebAssembly
├── web/               # React frontend (TypeScript, Vite, TailwindCSS)
├── examples/          # git / Gitea / hf_xet integration demos
├── docker/            # Dockerfile and Docker Compose
└── docs/              # Protocol specification
```

### Crate Dependency Graph

```
server / client / wasm
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

These are the only API endpoints the server exposes — the same wire protocol
HuggingFace's `xet-core` / `hf_xet` clients speak. All uploads and downloads,
including the web UI's and the examples', go through them.

### Storage Layout

```
{data_dir}/
├── xorbs/default/{hash}    # Chunk archives
├── shards/{hash}           # File metadata
├── index/
│   ├── files/              # file_hash → shard_hash mapping
│   └── chunks/             # chunk_hash → (xorb_hash, chunk_index) mapping
```

## Frontend

The web UI is a React SPA built with TypeScript, Vite, and TailwindCSS. It
speaks only the Xet wire protocol, like any other client:

- **Upload** -- files are chunked, hashed, and packed into xorbs *in the
  browser* by `openxet-wasm` (the workspace's own chunking/hashing crates
  compiled to WebAssembly), then POSTed to `/v1/xorbs` + `/v1/shards`
- **Files** -- a local catalog (browser localStorage) of files uploaded from
  this browser, plus "open by hash" for anything else; the CAS itself is
  content-addressed and has no listing endpoint by design
- **File detail** -- reconstruction terms from `/v1/reconstructions`; content
  preview/download reassembles the file in-browser from ranged xorb fetches
  (text, images, PDF, hex dump, and CSV/Parquet querying with DuckDB WASM)
- **Auth** -- for an auth-enabled server, paste an OIDC bearer token in the
  header field and the UI sends it verbatim; against a dev server with auth
  disabled, leave it blank

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

## Reference Client

`openxet-client` is a CLI that talks the `/v1` protocol directly, reusing the
workspace's own hashing/chunking/cas-types crates so it is wire-compatible by
construction. It performs the same chunk-level, cross-revision dedup as
`xet-core`: query which chunks already exist, upload only the new ones, then
register a shard referencing both.

```bash
# Upload (chunks, dedups, packs xorbs, registers a shard) — prints the file hash
cargo run -p openxet-client -- put ./bigfile.bin --report

# Download by file hash
cargo run -p openxet-client -- get <file-hash> --out ./restored.bin

# Config via flags or env: --url/OPENXET_URL, --token/OPENXET_TOKEN (optional)
```

## Examples

Runnable end-to-end demos live in [`examples/`](examples/README.md):

- **`hf-xet-client/`** — the stock [`hf_xet`](https://pypi.org/project/hf-xet/)
  Python client uploading/downloading against OpenXet unmodified (wire-compat proof)
- **`gitea-integration/`** — self-hosted Gitea + OpenXet + RustFS, with a
  git clean/smudge filter pushing large files through the CAS
- **`git-integration/`** — OpenXet as a git large-file backend, demonstrating
  chunk-level dedup across revisions

## Protocol Details

OpenXet implements several non-trivial aspects of the Xet protocol:

- **Hash encoding** -- 32-byte hashes are hex-encoded with LE octet reversal per 8-byte segment
- **Merkle tree** -- Variable-branching aggregated tree (mean branching factor 4), not a flat hash
- **Chunk compression** -- Per-chunk LZ4 frame compression (not raw blocks)
- **Shard format** -- Magic tag `"HFRepoMetaData\0"` + sentinel bytes; upload shards omit the footer

See [`docs/SPECIFICATION.md`](docs/SPECIFICATION.md) for the full protocol specification.

## Development

### Mise Tasks

The project uses [mise](https://mise.jdx.dev/) for task automation:

```bash
mise run build           # Build all crates (debug)
mise run build:release   # Build all crates (release)
mise run test            # Run all tests
mise run lint            # Run clippy
mise run check           # Format check + clippy + tests
mise run dev             # Build everything and run the server
mise run fe:build        # Build frontend (installs deps + compiles the wasm pipeline)
mise run up              # Docker compose up (RustFS backend)
mise run down            # Docker compose down
```

### Code Conventions

- Error types via `thiserror`; application errors via `anyhow`
- Async runtime: `tokio`; HTTP framework: `axum`
- All binary formats use little-endian byte order
- Shard entries are fixed at 48 bytes (FileInfo and CASInfo)

### Reference Test Data

Integration tests validate against official reference files from the [xet-spec-reference-files](https://huggingface.co/datasets/xet-team/xet-spec-reference-files) dataset on HuggingFace. The files are downloaded on first run into a temp directory (override with `OPENXET_TEST_DATA_DIR`) and cover chunk hashing, file hashing, merkle tree construction, and xorb/shard deserialization.

## License

This project is licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0).
