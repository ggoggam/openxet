#!/usr/bin/env bash
#
# Git + OpenXet integration demo — CHUNK-LEVEL dedup via the Xet wire protocol
# ===========================================================================
#
# Same git clean/smudge story as demo.sh, but the filter drives the real
# /v1/* Xet protocol through the `openxet-client` binary. This gives
# chunk-level, cross-revision dedup — exactly what HuggingFace's hf_xet does —
# instead of the whole-xorb dedup of the /api path.
#
# The tell: we use a SMALL (20 MiB) file that fits in a single xorb. Appending
# to it would force the /api path to re-store the entire new xorb; with chunk
# dedup the second commit costs only the bytes that actually changed.
#
# Requirements: bash, git, cargo, curl, python3.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FILTER="$SCRIPT_DIR/git-openxet-protocol"
cd "$REPO_ROOT"

PORT="${OPENXET_DEMO_PORT:-8098}"
export OPENXET_URL="http://127.0.0.1:${PORT}"
export OPENXET_AUTH_SECRET="git-xet-demo-secret"
export OPENXET_CLIENT="$REPO_ROOT/target/release/openxet-client"
WORK_DIR="$(mktemp -d)"
DATA_DIR="$WORK_DIR/cas-data"
SERVER_PID=""

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

cleanup() {
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

filesize() { wc -c < "$1" | tr -d ' '; }
sha256()   { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}' || sha256sum "$1" | awk '{print $1}'; }
stored_bytes() { curl -fsS "$OPENXET_URL/api/stats" | python3 -c "import sys,json;print(json.load(sys.stdin)['total_size_bytes'])"; }

# ─── build server + protocol client ──────────────────────────────────────────
log "Building openxet-server and openxet-client (release)…"
cargo build --release --quiet --bin openxet-server --bin openxet-client
mkdir -p "$DATA_DIR"
log "Launching OpenXet on $OPENXET_URL"
OPENXET_HOST=127.0.0.1 OPENXET_PORT="$PORT" OPENXET_DATA_DIR="$DATA_DIR" \
OPENXET_STORAGE_BACKEND=filesystem OPENXET_AUTH_SECRET="$OPENXET_AUTH_SECRET" \
  ./target/release/openxet-server >"$WORK_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do curl -fsS "$OPENXET_URL/api/stats" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS "$OPENXET_URL/api/stats" >/dev/null 2>&1 || { cat "$WORK_DIR/server.log"; fail "server did not start"; }
ok "Server is up"

# ─── git repo wired to the protocol filter ───────────────────────────────────
REPO="$WORK_DIR/my-dataset"
git init -q "$REPO"; cd "$REPO"
git config user.email demo@openxet.local; git config user.name "OpenXet Demo"
"$FILTER" install >/dev/null
echo '*.bin filter=openxet -text' > .gitattributes
git add .gitattributes && git commit -q -m "Track *.bin via the Xet protocol"
ok "Filter installed (chunk-level dedup via /v1 protocol)"

# ─── commit v1 (20 MiB — a SINGLE xorb) ───────────────────────────────────────
log "Committing v1 (20 MiB — fits in one xorb)…"
head -c $((20 * 1024 * 1024)) /dev/urandom > dataset.bin
V1_SHA="$(sha256 dataset.bin)"
git add dataset.bin && git commit -q -m "dataset v1"
STORED_V1="$(stored_bytes)"
BLOB_SIZE="$(git cat-file -s "$(git rev-parse HEAD:dataset.bin)")"
ok "git blob is $BLOB_SIZE bytes (pointer); CAS holds $STORED_V1 bytes"

# ─── commit v2 = v1 + 1 MiB appended ──────────────────────────────────────────
log "Committing v2 (v1 with 1 MiB appended)…"
head -c $((1 * 1024 * 1024)) /dev/urandom >> dataset.bin
V2_SHA="$(sha256 dataset.bin)"
git add dataset.bin && git commit -q -m "dataset v2 (append)"
STORED_V2="$(stored_bytes)"

DELTA=$(( STORED_V2 - STORED_V1 ))
APPENDED=$((1 * 1024 * 1024))
log "Storage growth from the v2 commit"
echo "    full size of v2          : $(filesize dataset.bin) bytes (one xorb)"
echo "    bytes appended           : $APPENDED"
echo "    CAS growth for v2 commit : $DELTA bytes"
# Chunk dedup => growth ≈ the appended region, far below the whole file. The
# /api (whole-xorb) path would have re-stored the entire ~21 MiB xorb here.
if [[ "$DELTA" -lt $((STORED_V1 / 2)) ]]; then
  ok "v2 cost ≈ the appended data, not the whole xorb -> CHUNK-LEVEL dedup"
else
  fail "expected chunk-level dedup; v2 grew CAS by $DELTA"
fi

# ─── clone + checkout round-trips both revisions ─────────────────────────────
log "Cloning into a fresh tree and materializing via smudge…"
CLONE="$WORK_DIR/clone"; git clone -q "$REPO" "$CLONE"; cd "$CLONE"
"$FILTER" install >/dev/null
git checkout -q -- dataset.bin
[[ "$(sha256 dataset.bin)" == "$V2_SHA" ]] && ok "checkout == v2 (byte-for-byte)" || fail "v2 mismatch"

git show "HEAD~1:dataset.bin" | "$FILTER" smudge dataset.bin > v1.bin
[[ "$(sha256 v1.bin)" == "$V1_SHA" ]] && ok "recovered v1 from history (byte-for-byte)" || fail "v1 mismatch"

log "Done — git stores pointers; OpenXet dedups at the chunk level over /v1."
