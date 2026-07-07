"""Async fsspec filesystem for OpenXet.

A Git forge (GitHub or Gitea) holds pointer files — the namespace: paths,
branches, history. The OpenXet CAS holds the bytes. Same pointer format as
examples/git-integration/git-openxet-protocol:

    version https://openxet/spec/v1
    xet-file-hash <64-hex>
    size <bytes>

read:  forge contents API -> pointer -> Xet reconstruction (Range-capable):
       GET /v1/reconstructions/{hash} -> ranged xorb fetches -> chunk decode
write: hf_xet upload pipeline (chunk/dedup/xorb/shard) -> pointer -> forge commit

Built on fsspec's AsyncFileSystem: every operation is a coroutine (`await
fs._cat_file(...)` with asynchronous=True), and fsspec mirrors them as sync
methods (`fs.cat_file(...)`) otherwise. HTTP is aiohttp; the blocking hf_xet
upload runs via anyio.to_thread; local file IO uses anyio.Path.

Usage:
    fs = XetFileSystem(
        forge_url="http://localhost:3000/api/v1", flavor="gitea",
        owner="xet", repo="my-dataset", ref="main", forge_auth=("xet", "pass"),
        cas_url="http://localhost:8080", cas_secret="...",
    )
    fs.pipe_file("data/blob.bin", b"...")     # upload bytes, commit pointer
    fs.cat_file("data/blob.bin")              # resolve pointer, download bytes
    with fs.open("data/blob.bin") as f:       # seekable, ranged reads
        f.seek(1024); f.read(10)
"""

from __future__ import annotations

import base64
import io
import weakref

import anyio
from fsspec.asyn import AsyncFileSystem, sync
from fsspec.spec import AbstractBufferedFile

from .client import CasClient
from .forge import Forge

POINTER_VERSION = "https://openxet/spec/v1"


def build_pointer(file_hash: str, size: int) -> bytes:
    """Render an OpenXet pointer for `file_hash`/`size`."""
    return f"{POINTER_VERSION}\nxet-file-hash {file_hash}\nsize {size}\n".encode()


def parse_pointer(data: bytes) -> tuple[str, int] | None:
    """Return (file_hash, size) if `data` is an OpenXet pointer, else None."""
    if not data.startswith(POINTER_VERSION.encode()):
        return None
    file_hash, size = None, None
    for line in data.decode(errors="replace").splitlines()[1:]:
        key, _, value = line.partition(" ")
        if key == "xet-file-hash":
            file_hash = value.strip()
        elif key == "size":
            size = int(value)
    if file_hash is None or size is None:
        return None
    return file_hash, size


class XetFileSystem(AsyncFileSystem):
    """Paths are repo-relative; repo/branch are fixed at construction."""

    protocol = "openxet"

    def __init__(
        self,
        forge_url,
        owner,
        repo,
        ref="main",
        forge_token=None,
        forge_auth=None,
        flavor="github",
        cas_url="http://127.0.0.1:8080",
        cas_secret=None,
        cas_token=None,
        commit_message=None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self.forge = Forge(
            forge_url,
            owner,
            repo,
            ref=ref,
            token=forge_token,
            auth=forge_auth,
            flavor=flavor,
        )
        self.cas = CasClient(
            cas_url, secret=cas_secret, token=cas_token, repo=f"{owner}/{repo}"
        )
        self.commit_message = commit_message or "openxet-fsspec"
        if not self.asynchronous:
            # async instances own their loop: call `await fs._close()` instead
            weakref.finalize(self, self._finalize, self.loop, self.forge, self.cas)

    async def _close(self):
        await self.forge.close()
        await self.cas.close()

    @staticmethod
    def _finalize(loop, forge, cas) -> None:
        if loop is not None and loop.is_running():
            try:
                sync(loop, forge.close, timeout=1)
                sync(loop, cas.close, timeout=1)
            except Exception:  # noqa: BLE001, S110 -- best-effort teardown
                pass  # interpreter teardown; nothing left to leak

    # ── namespace (forge) ─────────────────────────────────────────────────

    def _entry(self, st: dict) -> dict:
        info = {
            "name": st["path"],
            "size": st["size"],
            "type": "directory" if st["type"] == "dir" else "file",
        }
        # small files come with inline content; surface the real size if pointer
        if st.get("content"):
            pointer = parse_pointer(base64.b64decode(st["content"]))
            if pointer:
                info["xet_hash"], info["size"] = pointer
        return info

    async def _info(self, path, **kwargs):
        path = self._strip_protocol(path)
        if path in ("", "/"):
            return {"name": "", "size": 0, "type": "directory"}
        st = await self.forge.stat(path)
        if st is None:
            raise FileNotFoundError(path)
        if isinstance(st, list):
            return {"name": path, "size": 0, "type": "directory"}
        return self._entry(st)

    async def _ls(self, path, detail=False, **kwargs):
        path = self._strip_protocol(path)
        st = await self.forge.stat(path)
        if st is None:
            raise FileNotFoundError(path)
        # ponytail: directory listings report the forge-side (pointer) size;
        # info() on a single file resolves the real size.
        entries = (
            [self._entry(e) for e in st] if isinstance(st, list) else [self._entry(st)]
        )
        return entries if detail else [e["name"] for e in entries]

    async def _rm_file(self, path, **kwargs):
        path = self._strip_protocol(path)
        await self.forge.delete(path, f"{self.commit_message}: rm {path}")

    # ── data plane (CAS) ──────────────────────────────────────────────────

    async def _resolve(self, path):
        """-> ('cas', hash, size) for pointers, ('raw', bytes, size) otherwise."""
        data = await self.forge.read(self._strip_protocol(path))
        pointer = parse_pointer(data)
        if pointer:
            return "cas", pointer[0], pointer[1]
        return "raw", data, len(data)

    async def _cat_file(self, path, start=None, end=None, **kwargs):
        kind, ref, size = await self._resolve(path)
        if kind == "raw":
            return ref[start:end]
        if start is None and end is None:
            return await self.cas.download(ref)
        start, end = self._process_limits_to_abs(start, end, size)
        if start >= end:
            return b""
        return await self.cas.download(ref, start, end - 1)

    @staticmethod
    def _process_limits_to_abs(start, end, size) -> tuple[int, int]:
        start = 0 if start is None else start + size if start < 0 else start
        end = size if end is None else end + size if end < 0 else min(end, size)
        return start, end

    async def _pipe_file(self, path, value, **kwargs):
        path = self._strip_protocol(path)
        file_hash = await self.cas.upload(bytes(value))
        await self.forge.write(
            path,
            build_pointer(file_hash, len(value)),
            f"{self.commit_message}: put {path}",
        )

    async def _put_file(self, lpath, rpath, **kwargs):
        # ponytail: whole file in memory; chunked streaming if multi-GiB matters
        await self._pipe_file(rpath, await anyio.Path(lpath).read_bytes())

    async def _get_file(self, rpath, lpath, **kwargs):
        await anyio.Path(lpath).write_bytes(await self._cat_file(rpath))

    def _open(self, path, mode="rb", block_size=None, **kwargs):
        return XetFile(
            self, self._strip_protocol(path), mode=mode, block_size=block_size, **kwargs
        )


class XetFile(AbstractBufferedFile):
    """Sync file facade; runs coroutines on the filesystem's internal loop."""

    def __init__(self, fs, path, mode="rb", **kwargs):
        if "r" in mode:
            self._kind, self._ref, size = sync(fs.loop, fs._resolve, path)
            kwargs["size"] = size
        super().__init__(fs, path, mode=mode, **kwargs)

    def _fetch_range(self, start, end):  # end exclusive
        if self._kind == "raw":
            return self._ref[start:end]
        if start >= end:
            return b""
        return sync(self.fs.loop, self.fs.cas.download, self._ref, start, end - 1)

    def _initiate_upload(self):
        # ponytail: writes are spooled in memory and uploaded as one commit on
        # close; stream to a tempfile if multi-GiB writes ever matter.
        self._staged = io.BytesIO()

    def _upload_chunk(self, final=False):
        self.buffer.seek(0)
        self._staged.write(self.buffer.read())
        if final:
            sync(self.fs.loop, self.fs._pipe_file, self.path, self._staged.getvalue())
        return True
