#!/usr/bin/env bash
#
# OpenXet + official HuggingFace Xet client (`hf_xet`) interop demo
# =================================================================
#
# Starts a local OpenXet server, then runs demo.py, which uploads and
# downloads through the real `hf_xet` Python package (the same client
# `huggingface_hub` uses) pointed at OpenXet via its `endpoint` parameter.
#
# Requirements: bash, cargo, curl, python3.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

PORT="${OPENXET_DEMO_PORT:-8099}"
export OPENXET_URL="http://127.0.0.1:${PORT}"
WORK_DIR="$(mktemp -d)"
SERVER_PID=""

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

cleanup() {
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

server_up() { curl -s -o /dev/null "$OPENXET_URL/v1/shards"; } # any HTTP response = up

log "Building openxet-server (release)…"
cargo build --release --quiet --bin openxet-server
log "Launching OpenXet on $OPENXET_URL"
OPENXET_HOST=127.0.0.1 OPENXET_PORT="$PORT" OPENXET_DATA_DIR="$WORK_DIR/cas-data" \
OPENXET_STORAGE_BACKEND=filesystem OPENXET_AUTH_ENABLED=false \
  ./target/release/openxet-server >"$WORK_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 50); do server_up && break; sleep 0.2; done
server_up || { cat "$WORK_DIR/server.log"; fail "server did not start"; }

log "Setting up python venv with hf_xet + pyjwt…"
python3 -m venv "$WORK_DIR/venv"
"$WORK_DIR/venv/bin/pip" -q install hf_xet pyjwt

log "Running the official hf_xet client against OpenXet…"
"$WORK_DIR/venv/bin/python" "$SCRIPT_DIR/demo.py"

log "Done — the stock HuggingFace client works against OpenXet unmodified."
