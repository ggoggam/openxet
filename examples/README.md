# OpenXet examples

Runnable examples showing how to use the OpenXet CAS server as the storage
layer for version control over large binary datasets.

## `git-integration/` — OpenXet as a git large-file backend

```bash
examples/git-integration/demo.sh
```

This is the demonstration that most closely matches how real tooling uses a
content store. It wires a **git clean/smudge filter** (`git-integration/git-openxet`)
to an OpenXet server, exactly the mechanism Git LFS — and HuggingFace's Xet —
use to keep large bytes out of git:

- `.gitattributes` declares `*.bin filter=openxet -text`.
- **clean** (on `git add`): the real file bytes are uploaded to OpenXet and git
  stores only a tiny **pointer file**:
  ```
  version https://openxet/spec/v1
  xet-file-hash <64-hex>
  size <bytes>
  ```
- **smudge** (on `git checkout`): the pointer is read back, the bytes are fetched
  from OpenXet (`GET /api/files/{hash}/content`), and the working tree gets the
  real file.

The demo commits a 130 MiB file, shows git stored a 118-byte pointer (not the
data), appends to it and commits again, and confirms the second commit grew the
CAS by far less than the full file (dedup). Finally it clones the repo into a
fresh tree and shows the smudge filter materializing the bytes byte-for-byte,
and recovers the older revision straight from git history.

### How this maps to HuggingFace

HuggingFace repos *are* git repos. Even on a Xet-backed repo, **git stores the
standard Git LFS pointer file** (`version` / `oid sha256:…` / `size`); Xet keeps
that format for backward compatibility and only adds a "Xet backed hash" in the
web UI. The bytes are swapped in/out by the client:

- **Xet-aware** (`huggingface_hub` + `hf_xet` / `xet-core`): chunks the file with
  CDC, builds xorbs + shards, uploads only *new* chunks, and downloads via CAS
  reconstruction info — byte-level dedup.
- **Legacy** (plain `git`/`git-lfs`, `curl`): uses the normal LFS path; uploads
  are migrated to Xet by a background job, and downloads go through a **Git LFS
  bridge** that reconstructs the file and returns a single URL.

Our `git-openxet` filter plays the role of the Xet-aware client, against the
OpenXet CAS instead of the HuggingFace Hub.

### Two filters: `/api` vs the real `/v1` protocol

There are two clean/smudge filters in `git-integration/`, so you can see the
difference dedup granularity makes:

| Filter | Backend | Dedup | Demo |
|--------|---------|-------|------|
| `git-openxet` | `/api/upload` (server chunks) | whole **xorb** (~60 MiB) | `demo.sh` |
| `git-openxet-protocol` | `/v1/*` via `openxet-client` (client chunks) | **chunk** (~64 KiB) | `demo-chunk-dedup.sh` |

```bash
examples/git-integration/demo-chunk-dedup.sh
```

The protocol demo is the faithful equivalent of `hf_xet`. It commits a **20 MiB**
file (a *single* xorb), then appends 1 MiB and commits again. Because dedup is
at the chunk level, the second commit grows the CAS by only ~1 MiB — the bytes
that actually changed. The whole-xorb `/api` path can't do this: appending to a
single-xorb file forces it to re-store the entire new xorb.

Under the hood `git-openxet-protocol` shells out to `openxet-client`
(`crates/client`), a reference Xet protocol client that:

1. content-defined chunks the file and hashes each chunk;
2. queries `GET /v1/chunks/default-merkledb/{hash}` to find which chunks already
   exist (matching its chunk hashes against the HMAC-protected dedup shard);
3. packs only the **new** chunks into xorbs and `POST /v1/xorbs/default/{hash}`;
4. registers a shard (`POST /v1/shards`) whose file reconstruction references
   both the new and the pre-existing xorbs;

and on download fetches `GET /v1/reconstructions/{file_id}`, pulls the byte
ranges in `fetch_info`, and concatenates the decompressed chunks. It reuses the
workspace's own hashing/chunking/cas-types crates, so it is wire-compatible with
the server by construction. Build it with `cargo build -p openxet-client`; it
mints its own JWT from `OPENXET_AUTH_SECRET` (must match the server).

## How OpenXet works as a dataset VCS

The core primitive is **content addressing**: a file's identity *is* the hash of
its content. There are no mutable "paths" in the store — every distinct dataset
revision has a distinct, reproducible hash, and the same bytes always map to the
same hash. That gives you, for free:

- **Immutable, verifiable revisions.** A hash pins exact bytes; downloads are
  integrity-checked by construction.
- **Global deduplication.** Identical content is stored once no matter how many
  times or under how many names it is committed.
- **Sub-file deduplication.** Files are split with content-defined chunking
  (CDC), so a new revision that shares most of its bytes with an older one only
  pays storage for the chunks that actually changed — even if the change shifts
  everything after it (CDC re-syncs chunk boundaries).

What OpenXet does **not** provide on its own is the *naming* layer — branches,
tags, commit history, `dataset@v2 -> <hash>`. That mapping lives in your VCS or
catalog (a git repo of pointer files, a database, the HuggingFace Hub, etc.).
OpenXet is the content store those pointers resolve against. This is exactly the
split HuggingFace uses: the Hub holds repo/branch/commit metadata, and Xet holds
the deduplicated bytes.

### Two ways to talk to the server

| Path | Endpoints | Dedup granularity | Who chunks |
|------|-----------|-------------------|------------|
| **Convenience API** (used by this demo and the web UI) | `POST /api/upload`, `POST /api/upload/init` + parts + `complete`, `GET /api/files/{hash}/content` | **xorb** (~60 MiB pack) | server |
| **Xet wire protocol** (real `xet-core` / `huggingface_hub` clients) | `POST /v1/xorbs/default/{hash}`, `POST /v1/shards`, `GET /v1/reconstructions/{file_id}`, `GET /v1/chunks/default-merkledb/{hash}` | **chunk** (~64 KiB) | client |

The convenience API is the simplest way to drive the store from a shell and is
what the demo uses. Two caveats it exercises:

- **Single-shot `POST /api/upload` is capped at 64 MiB** (the server body
  limit). Larger files must use the multipart session API
  (`init` → `PUT` parts → `complete`), which is what `commit()` in the demo
  does, with 32 MiB parts.
- **Dedup is at xorb granularity here**, because the server packs each upload
  into fresh xorbs and only dedups whole identical xorbs. So unchanged data only
  dedups once a dataset spans more than one ~60 MiB xorb (hence the 130 MiB file
  in the demo). It does *not* yet consult the global chunk index to reuse
  individual chunks across differently-packed uploads.

True **chunk-level, cross-revision** dedup is a property of the Xet wire
protocol: the client computes chunk hashes locally, asks the server
`GET /v1/chunks/default-merkledb/{hash}` which chunks already exist, builds xorbs
containing only the *new* chunks, uploads them, and then registers a shard whose
file reconstruction references chunks across both new and pre-existing xorbs.
OpenXet implements all of these `/v1/*` endpoints (see
[`docs/SPECIFICATION.md`](../docs/SPECIFICATION.md) and the
`crates/server/tests/xetcore_compat.rs` integration test), and this repo ships a
reference client that drives them — `openxet-client` (`crates/client`), used by
the `git-openxet-protocol` filter above to get real chunk-level dedup.

### Using a real Xet client

Because the server speaks the Xet wire protocol, you can point Xet-aware tooling
at it instead of HuggingFace's CAS. Authentication is a JWT signed with the
server's `OPENXET_AUTH_SECRET` (scope `read` or `write`); mint one with
`mise run jwt`. The reconstruction endpoint hands back `fetch_info` URLs that
point back at this server's `GET /v1/xorbs/default/{hash}` with byte ranges, so
downloads work without any external object store — though for production you'd
configure an S3/GCS/Azure backend (see `[storage]` in the config) so xorbs are
served from blob storage.
