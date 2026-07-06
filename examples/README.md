# OpenXet examples

Runnable examples showing how to use the OpenXet CAS server as the storage
layer for version control over large binary datasets.

## `gitea-integration/` — self-hosted Gitea with OpenXet as the Xet CAS

```bash
examples/gitea-integration/demo.sh
```

The full self-hosted stack via `docker/compose.gitea.yaml`: **Gitea** hosts the
git repos, **OpenXet** stores the file bytes, and **RustFS** is OpenXet's
S3-compatible storage backend. Gitea needs no plugins or configuration — it
only ever sees ~118-byte pointer files, exactly like the HuggingFace Hub does
on Xet-backed repos. The demo creates a Gitea user and repo over the API,
pushes a 20 MiB file through the `git-openxet-protocol` clean/smudge filter
(chunk-level dedup via the `/v1` Xet wire protocol), shows Gitea holds only the
pointer, appends 1 MiB to prove the second push costs only ~1 MiB of CAS
growth, and fresh-clones to verify the smudge filter restores the bytes.

## `hf-xet-client/` — the official HuggingFace client against OpenXet

```bash
examples/hf-xet-client/demo.sh
```

Proof of wire compatibility: the stock [`hf_xet`](https://pypi.org/project/hf-xet/)
Python package (the exact client `huggingface_hub` uses) uploads and downloads
against OpenXet unmodified, via its `endpoint` parameter. The only piece the
official client leaves to the server operator is token *issuance* — on
huggingface.co that's the Hub's `xet-{read,write}-token` endpoint — so the demo
mints an OpenXet JWT locally with PyJWT and hands it to `hf_xet.XetSession`.

## `git-integration/` — OpenXet as a git large-file backend

```bash
examples/git-integration/demo-chunk-dedup.sh
```

This is the demonstration that most closely matches how real tooling uses a
content store. It wires a **git clean/smudge filter**
(`git-integration/git-openxet-protocol`) to an OpenXet server, exactly the
mechanism Git LFS — and HuggingFace's Xet — use to keep large bytes out of git:

- `.gitattributes` declares `*.bin filter=openxet -text`.
- **clean** (on `git add`): the real file bytes are uploaded to OpenXet over the
  `/v1` Xet wire protocol (`openxet-client put`) and git stores only a tiny
  **pointer file**:
  ```
  version https://openxet/spec/v1
  xet-file-hash <64-hex>
  size <bytes>
  ```
- **smudge** (on `git checkout`): the pointer is read back, the bytes are fetched
  from OpenXet via CAS reconstruction (`openxet-client get`), and the working
  tree gets the real file.

The demo commits a 20 MiB file (a *single* xorb), shows git stored a 118-byte
pointer (not the data), appends 1 MiB and commits again, and confirms the second
commit grew the CAS by only ~1 MiB — chunk-level dedup, the bytes that actually
changed. Finally it clones the repo into a fresh tree and shows the smudge
filter materializing the bytes byte-for-byte, and recovers the older revision
straight from git history.

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

Our `git-openxet-protocol` filter plays the role of the Xet-aware client,
against the OpenXet CAS instead of the HuggingFace Hub.

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

### The wire protocol

The server speaks only the Xet wire protocol (the same `/v1/*` endpoints real
`xet-core` / `huggingface_hub` clients use): `POST /v1/xorbs/default/{hash}`,
`POST /v1/shards`, `GET /v1/reconstructions/{file_id}`, and
`GET /v1/chunks/default-merkledb/{hash}`. Chunking happens client-side, so dedup
is at **chunk** granularity (~64 KiB).

**Chunk-level, cross-revision** dedup works like this: the client computes chunk
hashes locally, asks the server
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
