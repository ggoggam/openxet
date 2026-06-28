#!/usr/bin/env bash
#
# Git + OpenXet integration demo
# ===============================
#
# Shows OpenXet acting as the large-file storage backend for a git repository,
# the same way Git LFS / HuggingFace Xet do: git stores tiny *pointer files*,
# and a clean/smudge filter swaps pointers <-> real bytes against the CAS.
#
# Steps:
#   1. Build + launch the OpenXet server (filesystem backend, temp dir).
#   2. Create a git repo, install the git-openxet filter, track *.bin.
#   3. Commit v1 of a large binary dataset. Show git stored only a pointer.
#   4. Commit v2 (v1 with an appended tail). Show OpenXet deduped the unchanged
#      part, so the repo growth is bytes-on-the-wire, not whole-file copies.
#   5. Fresh clone + checkout -> smudge filter materializes real bytes, verified
#      byte-for-byte against the original.
#
# Requirements: bash, git, cargo, curl, python3.
# Override the port with OPENXET_DEMO_PORT.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FILTER="$SCRIPT_DIR/git-openxet"
cd "$REPO_ROOT"

PORT="${OPENXET_DEMO_PORT:-8094}"
export OPENXET_URL="http://127.0.0.1:${PORT}"
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

# ─── 1. build + launch the CAS server ────────────────────────────────────────
log "Building openxet-server (release)…"
cargo build --release --quiet --bin openxet-server
mkdir -p "$DATA_DIR"
log "Launching OpenXet on $OPENXET_URL"
OPENXET_HOST=127.0.0.1 OPENXET_PORT="$PORT" OPENXET_DATA_DIR="$DATA_DIR" \
OPENXET_STORAGE_BACKEND=filesystem \
  ./target/release/openxet-server >"$WORK_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do curl -fsS "$OPENXET_URL/api/stats" >/dev/null 2>&1 && break; sleep 0.2; done
curl -fsS "$OPENXET_URL/api/stats" >/dev/null 2>&1 || { cat "$WORK_DIR/server.log"; fail "server did not start"; }
ok "Server is up"

# ─── 2. create a git repo wired to the filter ────────────────────────────────
REPO="$WORK_DIR/my-dataset"
log "Initializing git repo at $REPO"
git init -q "$REPO"
cd "$REPO"
git config user.email demo@openxet.local
git config user.name  "OpenXet Demo"
"$FILTER" install >/dev/null
echo '*.bin filter=openxet -text' > .gitattributes
git add .gitattributes && git commit -q -m "Track *.bin with openxet filter"
ok "Filter installed; *.bin tracked via OpenXet"

# ─── 3. commit v1 ────────────────────────────────────────────────────────────
log "Creating + committing v1 (130 MiB dataset.bin)…"
head -c $((130 * 1024 * 1024)) /dev/urandom > dataset.bin
V1_SHA="$(sha256 dataset.bin)"
git add dataset.bin
git commit -q -m "Add dataset v1"
STORED_V1="$(stored_bytes)"

# What actually went into git? The pointer, not the bytes.
POINTER="$(git show HEAD:dataset.bin)"
BLOB_SIZE="$(git cat-file -s "$(git rev-parse HEAD:dataset.bin)")"
ok "Working tree dataset.bin : $(filesize dataset.bin) bytes (real data)"
ok "git blob for dataset.bin : $BLOB_SIZE bytes (just a pointer)"
echo "    ┌─ pointer stored in git history ─────────────────"
printf '%s\n' "$POINTER" | sed 's/^/    │ /'
echo "    └─────────────────────────────────────────────────"
ok "CAS now holds $STORED_V1 bytes"

# ─── 4. commit v2 (append) and show dedup ────────────────────────────────────
log "Creating + committing v2 (v1 with 4 MiB appended)…"
head -c $((4 * 1024 * 1024)) /dev/urandom >> dataset.bin
V2_SHA="$(sha256 dataset.bin)"
git add dataset.bin
git commit -q -m "Append to dataset (v2)"
STORED_V2="$(stored_bytes)"

DELTA=$(( STORED_V2 - STORED_V1 ))
log "Storage growth from the v2 commit"
echo "    full size of v2          : $(filesize dataset.bin) bytes"
echo "    CAS growth for v2 commit : $DELTA bytes (only the changed region's xorb)"
[[ "$DELTA" -lt "$(filesize dataset.bin)" ]] \
  && ok "Committing a new revision cost far less than the whole file -> dedup" \
  || fail "expected dedup: v2 commit grew CAS by the full file size"

GIT_DIR_SIZE="$(du -sk .git | awk '{print $1}')"
ok "git history (.git) is only ~${GIT_DIR_SIZE} KiB — the bytes live in the CAS, not git"

# ─── 5. fresh clone -> smudge materializes the bytes ─────────────────────────
log "Cloning the repo into a fresh working tree…"
CLONE="$WORK_DIR/clone"
git clone -q "$REPO" "$CLONE"
cd "$CLONE"
# The clone inherits .gitattributes but not local filter config; install it so
# checkout can smudge. (LFS/Xet ship the filter globally; here we wire it up.)
"$FILTER" install >/dev/null
git checkout -q -- dataset.bin   # re-run smudge now that the filter exists

CLONE_SHA="$(sha256 dataset.bin)"
[[ "$CLONE_SHA" == "$V2_SHA" ]] \
  && ok "Checked-out dataset.bin matches v2 byte-for-byte ($CLONE_SHA == HEAD)" \
  || fail "checkout mismatch: $CLONE_SHA != $V2_SHA"

# And we can recover the older revision the normal git way.
log "Recovering v1 via 'git checkout' of the first data commit…"
git checkout -q "HEAD~1" -- dataset.bin 2>/dev/null || git checkout -q HEAD -- dataset.bin
# checkout of the parent re-smudges the v1 pointer:
git show "HEAD~1:dataset.bin" | "$FILTER" smudge dataset.bin > v1_recovered.bin
[[ "$(sha256 v1_recovered.bin)" == "$V1_SHA" ]] \
  && ok "Recovered v1 byte-for-byte from history" \
  || fail "v1 recovery mismatch"

log "Done."
echo "    Each git commit pins an exact dataset revision; OpenXet stores the"
echo "    deduplicated bytes once. This is the Git-LFS / HuggingFace-Xet model."
