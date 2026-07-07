#!/usr/bin/env bash
#
# OpenXet + HuggingFace `git-xet` (Git LFS custom transfer agent) demo
# ====================================================================
#
# Pushes a real git repo through the official `git-xet` binary with OpenXet
# as the CAS: files are tracked by stock git-lfs (standard LFS pointers in
# git), and the byte transfer is delegated to git-xet, which chunks, dedups
# and uploads over the Xet wire protocol — exactly how a Xet-enabled push to
# huggingface.co works. Downloads come back through plain git-lfs basic HTTP
# via OpenXet's `/v1/content/sha256:<oid>` route.
#
# `lfs_server.py` (stdlib-only) plays the HF Hub's part: git smart-HTTP +
# the LFS batch API that negotiates the "xet" agent and mints CAS JWTs.
#
# Requirements: bash, cargo, curl, python3, git, git-lfs, git-xet
#   git-xet: `brew install git-xet`, or
#            `cargo install --git https://github.com/huggingface/xet-core.git git-xet`
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

CAS_PORT="${OPENXET_DEMO_PORT:-8098}"
GIT_PORT="${OPENXET_DEMO_GIT_PORT:-8175}"
export OPENXET_URL="http://127.0.0.1:${CAS_PORT}"
export OPENXET_AUTH_SECRET="git-xet-demo-secret-0123456789abcdef"
WORK_DIR="$(mktemp -d)"
CAS_PID="" GIT_PID=""

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓ %s\033[0m\n' "$*"; }
fail() { printf '\033[1;31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

cleanup() {
  for pid in "$CAS_PID" "$GIT_PID"; do
    [[ -n "$pid" ]] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null || true
  done
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

for tool in git python3 curl; do
  command -v "$tool" >/dev/null || fail "$tool not found"
done
git lfs version >/dev/null 2>&1 || fail "git-lfs not found (brew install git-lfs)"
command -v git-xet >/dev/null || fail \
  "git-xet not found. Install: brew install git-xet  (or: cargo install --git https://github.com/huggingface/xet-core.git git-xet)"
# The pre-HuggingFace XetHub product also shipped a `git-xet` binary; make
# sure the one on PATH is the LFS transfer agent from xet-core.
git-xet transfer --help >/dev/null 2>&1 || fail \
  "$(command -v git-xet) is not the HF LFS transfer agent (no 'transfer' subcommand) — install https://github.com/huggingface/xet-core git-xet"

log "Building openxet-server (release)…"
cargo build --release --quiet --bin openxet-server

log "Launching OpenXet CAS on $OPENXET_URL"
OPENXET_HOST=127.0.0.1 OPENXET_PORT="$CAS_PORT" OPENXET_DATA_DIR="$WORK_DIR/cas-data" \
OPENXET_STORAGE_BACKEND=filesystem OPENXET_AUTH_SECRET="$OPENXET_AUTH_SECRET" \
  ./target/release/openxet-server >"$WORK_DIR/cas.log" 2>&1 &
CAS_PID=$!

log "Launching git + LFS server on http://127.0.0.1:${GIT_PORT}"
git init -q --bare -b main "$WORK_DIR/origin.git"
GIT_REPO="$WORK_DIR/origin.git" PORT="$GIT_PORT" \
  python3 "$SCRIPT_DIR/lfs_server.py" >"$WORK_DIR/lfs.log" 2>&1 &
GIT_PID=$!

for _ in $(seq 1 50); do
  curl -sf -o /dev/null "http://127.0.0.1:${GIT_PORT}/origin.git/info/refs?service=git-upload-pack" &&
    curl -s -o /dev/null "$OPENXET_URL/v1/shards" && break
  sleep 0.2
done
curl -s -o /dev/null "$OPENXET_URL/v1/shards" || { cat "$WORK_DIR/cas.log"; fail "CAS did not start"; }

# ---------------------------------------------------------------- push side
log "Creating a repo, tracking *.bin with git-lfs, pushing through git-xet…"
PUSH_DIR="$WORK_DIR/push"
git init -q -b main "$PUSH_DIR"
cd "$PUSH_DIR"
git config user.email demo@openxet.local
git config user.name "OpenXet Demo"
git config lfs.locksverify false
# What `git xet install` writes, but repo-local so we don't touch your config.
git config lfs.customtransfer.xet.path git-xet
git config lfs.customtransfer.xet.args transfer
git config lfs.customtransfer.xet.concurrent true
git lfs install --local >/dev/null
git remote add origin "http://127.0.0.1:${GIT_PORT}/origin.git"

git lfs track "*.bin" >/dev/null
head -c $((8 * 1024 * 1024)) /dev/urandom > model.bin
ORIG_SHA="$(shasum -a 256 model.bin | cut -d' ' -f1)"
git add .gitattributes model.bin
git commit -qm "add model.bin (8 MiB, via git-xet)"

# HF_TOKEN short-circuits git-xet's credential lookup (no interactive prompt);
# our demo LFS server doesn't check it.
HF_TOKEN=unused git push -q origin main 2>"$WORK_DIR/push.log" ||
  { cat "$WORK_DIR/push.log" "$WORK_DIR/lfs.log"; fail "push failed"; }

git show HEAD:model.bin | head -1 | grep -q "git-lfs" ||
  fail "expected a standard LFS pointer in git"
ok "git stores the standard LFS pointer; bytes went to OpenXet via git-xet"

XORBS="$(find "$WORK_DIR/cas-data" -path '*xorb*' -type f | wc -l | tr -d ' ')"
[[ "$XORBS" -gt 0 ]] || fail "no xorbs landed in the CAS"
ok "CAS holds $XORBS xorb(s)"

# --------------------------------------------------------------- clone side
log "Fresh clone + git lfs pull (standard LFS basic download from OpenXet)…"
cd "$WORK_DIR"
GIT_LFS_SKIP_SMUDGE=1 git clone -q "http://127.0.0.1:${GIT_PORT}/origin.git" clone
cd clone
git lfs install --local >/dev/null
git lfs pull >/dev/null

CLONE_SHA="$(shasum -a 256 model.bin | cut -d' ' -f1)"
[[ "$CLONE_SHA" == "$ORIG_SHA" ]] || fail "roundtrip mismatch: $CLONE_SHA != $ORIG_SHA"
ok "clone matches original byte-for-byte (sha256 $ORIG_SHA)"

log "Done — the official git-xet transfer agent works against OpenXet."
