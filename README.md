# OpenXet

A Rust implementation of a [Xet Protocol](https://huggingface.co/docs/xet/en/index)-compatible Content Addressable Storage (CAS) server with a browser upload pipeline and web UI. OpenXet provides content-addressed data storage with chunk-level deduplication, following the Xet Protocol Specification v1.0.0. It speaks the same `/v1` wire protocol as HuggingFace's `xet-core` / `hf_xet`, so those clients work against it unmodified.

The binary formats, hashing, and chunking come directly from HuggingFace's own [`xet-core`](https://github.com/huggingface/xet-core) crates (`xet-core-structures`, `xet-data`) — the same code real `hf_xet` clients run — so wire compatibility holds by construction. The crates are pinned exactly (they are published as packaging for `hf_xet`, without a semver promise); upgrades are validated by the reference-file and client-compat test suites.

## Overview

OpenXet breaks files into content-defined chunks using a Gearhash CDC algorithm, hashes them with Blake3, and stores them in deduplicated xorb archives. Files are reconstructed by looking up chunk references stored in shard metadata. This enables efficient storage and transfer of large files with automatic deduplication at the chunk level.

### Key Features

- **Content-Defined Chunking** -- Gearhash-based CDC (64 KiB target) via xet-core's own chunker, for stable chunk boundaries across file revisions
- **Content-Addressed Storage** -- Blake3 keyed hashing with aggregated merkle trees for xorb and file identification
- **Chunk-Level Deduplication** -- Global dedup via blake3-keyed-HMAC chunk hash queries, matching real xet-core clients
- **Binary Formats** -- Xorb (chunk archive) and Shard (file metadata) serialization straight from `xet-core-structures`
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

### Running the dev server

The server runs on the host in both flavors and serves the API + static
frontend at `http://localhost:8080`. Pick a backend based on what you're doing:

```bash
mise run dev       # local:  filesystem storage — single-node local dev
mise run dev:s3    # rustfs: S3 storage — for distributed / multi-replica setups
```

**`dev` (local, filesystem)** — the default for everyday local work. Chunks and
metadata are written under `OPENXET_DATA_DIR` on the local filesystem, and
reconstruction serves xorb bytes directly from the server. No external services;
nothing to bring up or tear down. State lives on your disk between runs.

**`dev:s3` (rustfs, distributed)** — mirrors a production-shaped deployment.
Xorbs live in an S3-compatible object store (RustFS) and reconstruction returns
**presigned S3 URLs** so clients fetch bytes straight from object storage
instead of through the server. Because the object store (and the Postgres index
in the Docker variant) is shared, this is the layout that scales to multiple
server replicas. The task auto-manages the storage services for you:

- `depends = ["rustfs:up"]` brings up RustFS + Postgres via Docker before the
  server starts.
- `depends_post = ["rustfs:down"]` tears them back down when the server exits.

> The S3 endpoint is `localhost:9000`, **not** `rustfs:9000`: the server runs on
> the host, so the presigned URLs it mints must resolve from the host too. The
> port-mapped RustFS makes `localhost:9000` reachable from both server and
> client.

Configuration is entirely env-driven. `mise run dev:s3` sets these for you; the
same variables apply if you run the binary directly:

| Variable | `dev` (local) | `dev:s3` (rustfs) |
|----------|---------------|-------------------|
| `OPENXET_STORAGE_BACKEND` | *(filesystem, default)* | `s3` |
| `OPENXET_DATA_DIR` | local data dir | local data dir (shards/index) |
| `OPENXET_S3_BUCKET` | — | `openxet` |
| `OPENXET_S3_REGION` | — | `us-east-1` |
| `OPENXET_S3_ENDPOINT` | — | `http://localhost:9000` |
| `OPENXET_S3_ACCESS_KEY_ID` | — | `rustfsadmin` |
| `OPENXET_S3_SECRET_ACCESS_KEY` | — | `rustfsadmin` |
| `OPENXET_S3_ALLOW_HTTP` | — | `true` |

For authentication, configure OIDC issuers (`OPENXET_OIDC_ISSUERS`); clients then
present bearer tokens from that provider. Leave `OPENXET_AUTH_ENABLED=false`
(the mise default) for local/dev use.

### Running with Docker

The Docker Compose stacks run the server *inside* a container. The RustFS stack
additionally wires in a Postgres index (`OPENXET_INDEX_BACKEND=postgres`) so the
server can scale to multiple replicas behind a shared object store.

```bash
# Local filesystem backend
docker compose -f docker/compose.local.yaml up -d --build

# S3-compatible backend via RustFS (S3 API on :9000, console on :9001) + Postgres index
docker compose -f docker/compose.rustfs.yaml up -d --build   # or: mise run up
```

### Testing

```bash
cargo test         # Run all tests
cargo test --lib   # Unit tests only
cargo clippy       # Lint
cargo fmt --check  # Check formatting
```

## Architecture

OpenXet is organized as a Cargo workspace with three crates on top of the
pinned HuggingFace `xet-core` crates:

```
openxet/
├── crates/
│   ├── cas_types/     # /v1 HTTP wire types (reconstruction JSON)
│   ├── server/        # HTTP server (axum) with auth and storage — the /v1 CAS protocol
│   └── wasm/          # openxet-wasm: chunk/hash/pack pipeline compiled to WebAssembly
├── web/               # React frontend (TypeScript, Vite, TailwindCSS)
├── examples/          # git / Gitea / hf_xet integration demos
├── docker/            # Dockerfile and Docker Compose
└── docs/              # Protocol specification
```

### Crate Dependency Graph

```
server ── cas_types
   │
   ├── xet-core-structures   (hashing, xorb + shard formats; pinned upstream)
wasm ┤
   └── xet-data              (Gearhash CDC chunker; pinned upstream)
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
  browser* by `openxet-wasm` (HuggingFace's xet-core crates compiled to
  WebAssembly), then POSTed to `/v1/xorbs` + `/v1/shards`
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

## Examples

An example lives in [`examples/`](examples/README.md):

- **`hf-xet-client/`** — the stock [`hf_xet`](https://pypi.org/project/hf-xet/)
  Python client uploading/downloading against OpenXet unmodified (wire-compatibility proof)

## Protocol Details

OpenXet implements several non-trivial aspects of the Xet protocol:

- **Hash encoding** -- 32-byte hashes are hex-encoded with LE octet reversal per 8-byte segment
- **Merkle tree** -- Variable-branching aggregated tree (mean branching factor 4), not a flat hash
- **Chunk compression** -- Per-chunk LZ4 frame compression (not raw blocks)
- **Shard format** -- Magic tag `"HFRepoMetaData\0"` + sentinel bytes; upload shards omit the footer

See [`docs/SPECIFICATION.md`](docs/SPECIFICATION.md) for the full protocol specification.

## Development

### Mise Tasks

The project uses [mise](https://mise.jdx.dev/) for both toolchain management
and task automation. `mise install` provisions everything except Rust itself
(bun, uv, prek, wasm-pack, cargo-nextest — Rust comes from rustup). Run any task
with `mise run <task>`; list them all with `mise tasks`. Tasks declaring
`sources`/`outputs` are cached and skip re-running when inputs are unchanged.

**Run**

| Task (alias) | What it does |
|------|--------------|
| `dev` | Build server + frontend, run the server with the **filesystem** backend (local dev). |
| `dev:s3` | Build server + frontend, bring up RustFS + Postgres, run the server against **S3** (presigned URLs); tears the services down on exit. |

**Build**

| Task | What it does |
|------|--------------|
| `build` | Build all crates (debug). |
| `build:release` | Build all crates (release). |
| `clean` | `cargo clean` — remove build artifacts. |

**Test**

| Task | What it does |
|------|--------------|
| `test` | Run all tests via `cargo nextest` (`RUST_BACKTRACE=1`). |
| `test:unit` | Unit tests only (`--lib`). |
| `test:integration` | Integration tests only (`--test '*'`). |

**Lint & Format**

| Task | What it does |
|------|--------------|
| `lint` | `cargo clippy -- -D warnings`. |
| `fmt` | Format all Rust code (`cargo fmt --all`). |
| `fmt:check` | Check formatting without writing. |
| `check` | Aggregate gate: depends on `fmt:check`, `lint`, `test`. |
| `pre-commit` | Run all pre-commit hooks via `prek` against every file. |

**Frontend** (all run in `web/`)

| Task | What it does |
|------|--------------|
| `fe:install` | Install frontend deps (`bun install`). |
| `fe:wasm` | Build the `openxet-wasm` package used by the upload page (adds the `wasm32-unknown-unknown` target, runs `wasm-pack`). |
| `fe:build` | Production frontend build (depends on `fe:install`, `fe:wasm`). |
| `fe:dev` | Frontend dev server with HMR, proxies `/v1` to `localhost:8080` (depends on `fe:install`, `fe:wasm`). |
| `fe:lint` | Lint frontend code (ESLint). |

**Docker**

| Task (alias) | What it does |
|------|--------------|
| `docker:up` (`up`) | Bring up the full Docker Compose stack (RustFS backend + Postgres + dockerized server). |
| `docker:down` (`down`) | Bring the stack down. |
| `docker:log` (`log`) | Tail the last 100 lines of compose logs. |
| `rustfs:up` (`rustfs`) | Bring up **only** the storage services (RustFS + Postgres), for a host-run server — used as a dependency of `dev:s3`. |
| `rustfs:down` | Bring the storage services down. |

Typical loops:

```bash
mise run dev                       # backend on :8080 (filesystem)
mise run fe:dev                    # frontend HMR, proxying /v1 → :8080
mise run check                     # fmt + clippy + tests before pushing
mise run dev:s3                    # exercise the S3 / presigned-URL path
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
