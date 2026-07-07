#!/usr/bin/env python3
"""End-to-end check for openxet-fsspec against the gitea compose stack.

Prereq: docker compose -f docker/compose.gitea.yaml up -d
(user/repo are created below via the Gitea API, idempotently)

Run: uv run --with-editable . demo.py
"""

import os
import secrets
import tempfile

import anyio
import requests
from fsspec.asyn import sync

from openxet_fsspec import XetFileSystem
from openxet_fsspec.spec import build_pointer, parse_pointer

GITEA_URL = os.environ.get("GITEA_URL", "http://localhost:3000")
GITEA_USER = "xet"
GITEA_PASS = "xetpass123"
REPO = "fsspec-demo"
CAS_URL = os.environ.get("OPENXET_URL", "http://localhost:8080")
CAS_TOKEN = os.environ.get("OPENXET_TOKEN")  # None → dev server (auth disabled)

FS_OPTS = {
    "forge_url": f"{GITEA_URL}/api/v1",
    "flavor": "gitea",
    "owner": GITEA_USER,
    "repo": REPO,
    "ref": "main",
    "forge_auth": (GITEA_USER, GITEA_PASS),
    "cas_url": CAS_URL,
    "cas_token": CAS_TOKEN,
}

# ── offline self-check: pointer round-trip ───────────────────────────────────
p = build_pointer("ab" * 32, 1234)
assert parse_pointer(p) == ("ab" * 32, 1234)
assert parse_pointer(b"just a regular file\n") is None
print("ok  pointer parse/build")

# ── gitea repo (idempotent) ──────────────────────────────────────────────────
requests.post(
    f"{GITEA_URL}/api/v1/user/repos",
    auth=(GITEA_USER, GITEA_PASS),
    json={"name": REPO, "private": False, "auto_init": True},
)

fs = XetFileSystem(**FS_OPTS)
data = secrets.token_bytes(3 * 1024 * 1024)

# ── sync facade: upload → pointer in forge, bytes in CAS ─────────────────────
fs.pipe_file("data/blob.bin", data)
pointer = sync(fs.loop, fs.forge.read, "data/blob.bin")
info = parse_pointer(pointer)
assert info is not None and info[1] == len(data), "forge does not hold a pointer"
print(f"ok  upload: forge holds a {len(pointer)}-byte pointer for {len(data)} bytes")

# ── sync facade: download full, ranged, seek via open() ──────────────────────
assert fs.cat_file("data/blob.bin") == data
assert fs.cat_file("data/blob.bin", start=1024, end=2048) == data[1024:2048]
with fs.open("data/blob.bin", "rb") as f:
    f.seek(1_000_000)
    assert f.read(16) == data[1_000_000:1_000_016]
print("ok  download: full, ranged, seek")

# ── file-like write path ─────────────────────────────────────────────────────
with fs.open("notes/hello.txt", "wb") as f:
    f.write(b"hello ")
    f.write(b"openxet")
assert fs.cat_file("notes/hello.txt") == b"hello openxet"
print("ok  open('wb') write path")

# ── put_file / get_file (anyio.Path local IO) ────────────────────────────────
with tempfile.TemporaryDirectory() as tmp:
    src, dst = os.path.join(tmp, "src.bin"), os.path.join(tmp, "dst.bin")
    with open(src, "wb") as f:
        f.write(data[: 512 * 1024])
    fs.put_file(src, "data/local.bin")
    fs.get_file("data/local.bin", dst)
    with open(dst, "rb") as f:
        assert f.read() == data[: 512 * 1024]
print("ok  put_file/get_file")

# ── namespace ops ────────────────────────────────────────────────────────────
fs.pipe_file("data/blob.bin", data[: 1024 * 1024])  # overwrite = update commit
assert fs.info("data/blob.bin")["size"] == 1024 * 1024
assert "data/blob.bin" in fs.ls("data")
assert fs.info("data")["type"] == "directory"
assert fs.cat_file("README.md")  # non-pointer file passes through raw
fs.rm_file("notes/hello.txt")
try:
    fs.info("notes/hello.txt")
    raise SystemExit("FAIL: rm did not delete")
except FileNotFoundError:
    pass
print("ok  overwrite/ls/info/raw-passthrough/rm")


# ── native async usage ───────────────────────────────────────────────────────
async def async_checks():
    afs = XetFileSystem(**FS_OPTS, asynchronous=True, skip_instance_cache=True)
    await afs._pipe_file("data/async.bin", data[:2048])
    assert await afs._cat_file("data/async.bin") == data[:2048]
    assert await afs._cat_file("data/async.bin", start=100, end=200) == data[100:200]
    assert (await afs._info("data/async.bin"))["size"] == 2048
    await afs._rm_file("data/async.bin")
    await afs._close()


anyio.run(async_checks)
print("ok  native async: pipe/cat/info/rm")

print("\nall checks passed")
