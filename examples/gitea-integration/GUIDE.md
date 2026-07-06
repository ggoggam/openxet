# End-to-end guide: Gitea + OpenXet

A hands-on walkthrough of using a self-hosted **Gitea** as the "hub" (names,
branches, history) with **OpenXet** as the Xet CAS backend (deduplicated file
bytes) on **RustFS** (S3 storage) — the same split HuggingFace uses between the
Hub and Xet.

You'll: bring up the stack, log into Gitea, clone a repo, create a Parquet file
(or pull ImageNet-mini from HuggingFace), commit + push it through the Xet
clean/smudge filter, branch, make a small change and push again (watch
chunk-level dedup), then **view the file's chunks/xorbs in the web UI**.

> Prefer to watch it run non-interactively first? `examples/gitea-integration/demo.sh`
> does steps 1–6 automatically. This guide is the manual version so you can poke
> at each piece.

Requirements: `docker compose`, `git`, `cargo`, `curl`, `python3` (for Parquet).

---

## 1. Bring up the stack

From the repo root:

```bash
docker compose -f docker/compose.gitea.yaml up -d --build
```

This starts three services:

| Service | URL | What it is |
|---------|-----|------------|
| Gitea   | http://localhost:3000 | Git host + web UI |
| OpenXet | http://localhost:8080 | Xet CAS API **+ web UI** |
| RustFS  | http://localhost:9001 | S3 storage console (`rustfsadmin`/`rustfsadmin`) |

Wait until both answer:

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:3000/api/healthz   # 200
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:8080/v1/shards      # 405 = up
```

The compose file's `gitea-init` service auto-creates an admin user:

- **username:** `xet`
- **password:** `xetpass123`

Open http://localhost:3000 and log in with those to confirm.

## 2. Build the client + wire up the filter

The git filter shells out to `openxet-client`. Build it once (release):

```bash
cargo build --release -p openxet-client
```

Export the config the filter and client read (the secret **must** match
`OPENXET_AUTH_SECRET` in `docker/compose.rustfs.yaml`):

```bash
export OPENXET_URL="http://localhost:8080"
export OPENXET_AUTH_SECRET="change-me-in-production"
export OPENXET_CLIENT="$(pwd)/target/release/openxet-client"
```

## 3. Create a repo and clone it

Create the repo over the Gitea API (or click **+ → New Repository** in the UI):

```bash
curl -fsS -u xet:xetpass123 -X POST http://localhost:3000/api/v1/user/repos \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-dataset","private":false}'
```

Clone it (credentials in the URL for convenience) and install the filter:

```bash
git clone http://xet:xetpass123@localhost:3000/xet/my-dataset.git
cd my-dataset
git config user.name "OpenXet Demo" && git config user.email demo@example.com

# installs filter.openxet.{clean,smudge} into THIS repo's .git/config
"$OLDPWD/examples/git-integration/git-openxet-protocol" install
```

Declare which files go through OpenXet instead of into git. We'll track
Parquet:

```bash
echo '*.parquet filter=openxet -text' > .gitattributes
git add .gitattributes && git commit -m "track *.parquet via openxet"
```

## 4. Get a dataset file

Pick **one**.

### Option A — generate a Parquet file locally

```bash
uv run --with pyarrow --with numpy - <<'PY'
import numpy as np, pyarrow as pa, pyarrow.parquet as pq
n = 500_000
t = pa.table({
    "id":    np.arange(n, dtype=np.int64),
    "label": np.random.randint(0, 1000, n, dtype=np.int32),
    "score": np.random.rand(n),
    "note":  pa.array([f"row-{i}" for i in range(n)]),
})
pq.write_table(t, "data.parquet")
print("wrote data.parquet")
PY
```

### Option B — download ImageNet-mini from HuggingFace

```bash
pip install huggingface_hub
# grab one parquet shard from a mini imagenet dataset
huggingface-cli download --repo-type dataset \
  timm/mini-imagenet --include "*.parquet" --local-dir ./hf-imagenet
cp hf-imagenet/data/train-00000-*.parquet data.parquet   # adjust to the actual filename
```

Either way you now have `data.parquet` in the working tree.

## 5. Commit and push (bytes go to OpenXet, not Gitea)

```bash
git add data.parquet && git commit -m "add data.parquet"
git push origin main
```

On `git add`, the **clean** filter uploaded the bytes to OpenXet and git stored
only a ~118-byte pointer. Verify Gitea holds the pointer, not the data:

```bash
curl -fsS -u xet:xetpass123 \
  "http://localhost:3000/api/v1/repos/xet/my-dataset/raw/data.parquet?ref=main"
```

You'll see:

```
version https://openxet/spec/v1
xet-file-hash <64-hex>          # <-- this hash is your handle in the web UI
size <bytes>
```

**Copy that `xet-file-hash`** — you'll paste it into the web UI in step 8.

## 6. Branch, make a small change, push again

```bash
git switch -c add-rows

# append a few rows — most chunks are unchanged, so dedup kicks in
uv run --with pyarrow --with numpy - <<'PY'
import pyarrow.parquet as pq, pyarrow as pa, numpy as np
t = pq.read_table("data.parquet")
extra = pa.table({c.name: (pa.array(np.random.rand(1000)) if c.name=="score"
    else pa.array([f"row-extra-{i}" for i in range(1000)]) if c.name=="note"
    else pa.array(np.random.randint(0,1000,1000)).cast(c.type)) for c in t.schema})
pq.write_table(pa.concat_tables([t, extra]), "data.parquet")
print("appended 1000 rows")
PY

git add data.parquet && git commit -m "append 1000 rows"
git push -u origin add-rows
```

### See the dedup

Upload the same file directly with the client's `--stats` flag to see how few
chunks are actually new on the second version:

```bash
"$OPENXET_CLIENT" put data.parquet --stats
```

`--stats` prints (to stderr) how many chunks already existed in the CAS vs. how
many new bytes were uploaded. You can also eyeball total CAS growth in the
RustFS console at http://localhost:9001.

## 7. Verify a fresh clone restores the bytes

```bash
cd .. && git clone http://xet:xetpass123@localhost:3000/xet/my-dataset.git verify
cd verify
"$OLDPWD/examples/git-integration/git-openxet-protocol" install
git checkout main        # smudge filter fetches bytes from OpenXet
ls -l data.parquet       # real bytes, not the pointer
```

## 8. View the file's chunks in the web UI

Open the **OpenXet** web UI (not Gitea): **http://localhost:8080**

1. In the header, paste the auth secret `change-me-in-production` into the
   secret field. The UI mints a short-lived JWT locally (there's no login
   endpoint — on huggingface.co the Hub issues the token).
2. Go to **Files**, then **open by hash**, and paste the `xet-file-hash` from
   step 5 (or navigate directly to `http://localhost:8080/files/<hash>`).

The file-detail page shows the reconstruction, i.e. the **chunk layout**:

- **Reconstruction Terms** — the ordered list of chunk ranges that make up the
  file: each term's chunk `hash`, its `[start, end)` chunk range, and unpacked
  byte length.
- **Referenced Xorbs** — which xorb archives those chunks live in, and for each,
  the chunk range `[start, end)` and the byte range `bytes X–Y` fetched from
  that xorb.
- **Content preview** — because it's Parquet, the UI queries it in-browser with
  DuckDB WASM (run SQL over the file). Text/image/PDF/CSV also preview here.

Compare the two versions: open the version-1 hash and the version-2 hash side by
side — most chunk hashes are identical, which is exactly the chunk-level dedup
that kept the second push cheap.

## 9. Tear down

```bash
docker compose -f docker/compose.gitea.yaml down -v
```

---

### How it fits together

```
git add data.parquet
   └─ clean filter ─ openxet-client put ─▶ /v1/xorbs + /v1/shards ─▶ RustFS (S3)
                                            (chunk, dedup-query, upload new only)
git commit          → git stores a ~118-byte pointer file
git checkout
   └─ smudge filter ─ openxet-client get ─▶ /v1/reconstructions ─▶ real bytes
```

Gitea never sees the large bytes — only pointers — so no Gitea plugin or config
is required. This is byte-for-byte the same `/v1` wire protocol HuggingFace's
`hf_xet` speaks; see [`../hf-xet-client/`](../hf-xet-client/) for proof.
