#!/usr/bin/env bash
#
# Gitea + OpenXet end-to-end demo
# ===============================
#
# Shows a self-hosted Gitea acting as the "hub" (names, branches, history)
# with OpenXet as the Xet CAS backend (deduplicated bytes) on RustFS storage —
# the same split HuggingFace uses between the Hub and Xet.
#
#   1. Brings up gitea + openxet + rustfs via docker/compose.gitea.yaml.
#   2. Creates a Gitea user + repo through the Gitea API.
#   3. Clones over HTTP and installs the git-openxet-protocol clean/smudge
#      filter (chunk-level dedup via the real /v1 Xet wire protocol).
#   4. Commits a 20 MiB file and pushes: Gitea receives a ~118-byte pointer,
#      OpenXet receives the chunks (stored in RustFS).
#   5. Appends 1 MiB and pushes again: CAS grows by only ~1 MiB (chunk dedup).
#   6. Fresh-clones from Gitea and verifies the smudge filter restores the
#      bytes exactly.
#
# Requirements: bash, git, docker compose, cargo, curl, python3.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
COMPOSE=(docker compose -f "$REPO_ROOT/docker/compose.gitea.yaml")
FILTER="$REPO_ROOT/examples/git-integration/git-openxet-protocol"

GITEA_URL="http://localhost:3000"
GITEA_USER="xet"
GITEA_PASS="xetpass123"
REPO_NAME="my-dataset"

export OPENXET_URL="http://localhost:8080"
export OPENXET_AUTH_SECRET="change-me-in-production" # must match compose
export OPENXET_CLIENT="$REPO_ROOT/target/release/openxet-client"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m  ✗ %s\033[0m\n' "$*" >&2; exit 1; }

filesize() { wc -c < "$1" | tr -d ' '; }
sha256()   { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}' || sha256sum "$1" | awk '{print $1}'; }
stored_bytes() { curl -fsS "$OPENXET_URL/api/stats" | python3 -c "import sys,json;print(json.load(sys.stdin)['total_size_bytes'])"; }
wait_for() { # wait_for <url> <name>
  for _ in $(seq 1 60); do curl -fsS "$1" >/dev/null 2>&1 && { ok "$2 is up"; return 0; }; sleep 2; done
  fail "$2 did not come up ($1)"
}

# ─── services ─────────────────────────────────────────────────────────────────
log "Starting gitea + openxet + rustfs…"
"${COMPOSE[@]}" up -d --build
wait_for "$OPENXET_URL/api/stats" "OpenXet CAS"
wait_for "$GITEA_URL/api/healthz" "Gitea"

log "Building openxet-client (release)…"
(cd "$REPO_ROOT" && cargo build --release --quiet -p openxet-client)

# ─── gitea user + repo ────────────────────────────────────────────────────────
log "Creating Gitea user '$GITEA_USER' and repo '$REPO_NAME'…"
"${COMPOSE[@]}" exec -u git gitea gitea admin user create \
  --username "$GITEA_USER" --password "$GITEA_PASS" \
  --email xet@example.com --admin --must-change-password=false \
  2>/dev/null || ok "user already exists"
curl -fsS -u "$GITEA_USER:$GITEA_PASS" -X POST "$GITEA_URL/api/v1/user/repos" \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"$REPO_NAME\",\"private\":false}" >/dev/null 2>&1 || ok "repo already exists"

# ─── clone + wire the xet filter ──────────────────────────────────────────────
CLONE_URL="http://$GITEA_USER:$GITEA_PASS@localhost:3000/$GITEA_USER/$REPO_NAME.git"
REPO="$WORK_DIR/$REPO_NAME"
log "Cloning from Gitea and installing the openxet filter…"
git clone -q --no-checkout "$CLONE_URL" "$REPO" 2>/dev/null
cd "$REPO"
git config user.email demo@example.com && git config user.name "OpenXet Demo"
"$FILTER" install
git checkout -qB main origin/main 2>/dev/null || git checkout -qb main
echo '*.bin filter=openxet -text' > .gitattributes
git add .gitattributes && git commit -qm "track *.bin via openxet" || true

# ─── commit 1: 20 MiB file ────────────────────────────────────────────────────
log "Committing a 20 MiB file…"
BASE="$(stored_bytes)"
head -c $((20 * 1024 * 1024)) /dev/urandom > data.bin
ORIG_SHA="$(sha256 data.bin)"
git add data.bin && git commit -qm "add data.bin (20 MiB)"
git push -q origin main
AFTER1="$(stored_bytes)"
ok "pushed; CAS grew by $(( (AFTER1 - BASE) / 1024 / 1024 )) MiB"

# gitea processes pushes asynchronously; retry briefly
POINTER=""
for _ in $(seq 1 15); do
  POINTER="$(curl -fsS -u "$GITEA_USER:$GITEA_PASS" \
    "$GITEA_URL/api/v1/repos/$GITEA_USER/$REPO_NAME/raw/data.bin?ref=main" 2>/dev/null)" && break
  sleep 1
done
echo "$POINTER" | grep -q 'xet-file-hash' || fail "Gitea does not hold a pointer file"
ok "Gitea stores only the pointer ($(printf '%s' "$POINTER" | wc -c | tr -d ' ') bytes):"
printf '%s\n' "$POINTER" | sed 's/^/      /'

# ─── commit 2: append 1 MiB → chunk-level dedup ──────────────────────────────
log "Appending 1 MiB and pushing again (chunk-level dedup)…"
head -c $((1024 * 1024)) /dev/urandom >> data.bin
FINAL_SHA="$(sha256 data.bin)"
git add data.bin && git commit -qm "append 1 MiB"
git push -q origin main
AFTER2="$(stored_bytes)"
GREW=$(( AFTER2 - AFTER1 ))
ok "second commit grew the CAS by only $(( GREW / 1024 )) KiB (file is 21 MiB)"
(( GREW < 3 * 1024 * 1024 )) || fail "expected chunk-level dedup (< 3 MiB growth)"

# ─── fresh clone: smudge restores bytes ──────────────────────────────────────
log "Fresh clone from Gitea — smudge filter fetches bytes from OpenXet…"
FRESH="$WORK_DIR/fresh"
git clone -q --no-checkout "$CLONE_URL" "$FRESH"
cd "$FRESH"
"$FILTER" install
git checkout -q main
[[ "$(sha256 data.bin)" == "$FINAL_SHA" ]] || fail "restored bytes differ from original"
ok "restored data.bin byte-for-byte from the CAS"
git checkout -q HEAD~1 -- data.bin
[[ "$(sha256 data.bin)" == "$ORIG_SHA" ]] || fail "historical revision differs"
ok "historical 20 MiB revision also restored from git history"

log "Done. Gitea: $GITEA_URL ($GITEA_USER/$GITEA_PASS) · OpenXet: $OPENXET_URL · RustFS console: http://localhost:9001"
log "Tear down with: ${COMPOSE[*]} down -v"
