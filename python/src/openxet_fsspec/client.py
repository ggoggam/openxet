from __future__ import annotations

import asyncio
import time

import aiohttp
import anyio


def _ungroup4(grouped: bytes) -> bytes:
    """Reverse BG4 byte grouping: 4 position-groups back to interleaved bytes."""
    n = len(grouped)
    full, rem = divmod(n, 4)
    sizes = [full + (1 if g < rem else 0) for g in range(4)]
    groups, offset = [], 0
    for size in sizes:
        groups.append(grouped[offset : offset + size])
        offset += size
    out = bytearray(n)
    pos = 0
    for i in range(sizes[0] if sizes else 0):
        for group in groups:
            if i < len(group):
                out[pos] = group[i]
                pos += 1
    return bytes(out)


def decode_chunks(data: bytes, start: int, end: int) -> bytes:
    """Decode chunk frames [start, end) from a fetched xorb byte range.

    Each frame: 8-byte header (version u8, compressed_size u24le,
    compression_type u8, uncompressed_size u24le) + compressed payload.
    """
    import lz4.frame  # noqa: PLC0415 -- only needed on the download path

    out, pos, idx = [], 0, 0
    while pos + 8 <= len(data) and idx < end:
        if data[pos] != 0:
            msg = f"unsupported chunk version {data[pos]}"
            raise ValueError(msg)
        csize = int.from_bytes(data[pos + 1 : pos + 4], "little")
        ctype = data[pos + 4]
        payload = data[pos + 8 : pos + 8 + csize]
        if len(payload) < csize:
            msg = "truncated chunk payload"
            raise ValueError(msg)
        pos += 8 + csize
        if idx >= start:
            if ctype == 0:
                raw = payload
            elif ctype in (1, 2):
                raw = lz4.frame.decompress(payload)
                if ctype == 2:
                    raw = _ungroup4(raw)
            else:
                msg = f"unknown compression type {ctype}"
                raise ValueError(msg)
            out.append(raw)
        idx += 1
    return b"".join(out)


class CasClient:
    """OpenXet /v1 data plane: hf_xet for upload, Xet reconstruction for download."""

    def __init__(self, endpoint, secret=None, token=None, repo="openxet/fsspec"):
        if not secret and not token:
            raise ValueError("need cas_secret (to mint JWTs) or cas_token")
        self.endpoint = endpoint.rstrip("/")
        self.secret = secret
        self.static_token = token
        self.repo = repo
        self._session = None

    def session(self) -> aiohttp.ClientSession:
        if self._session is None:
            self._session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=30)
            )
        return self._session

    async def close(self) -> None:
        if self._session is not None:
            await self._session.close()
            self._session = None

    def _token(self, scope):
        if self.static_token:
            return self.static_token, int(time.time()) + 3600
        import jwt  # noqa: PLC0415 -- optional dep, imported only when minting JWTs

        exp = int(time.time()) + 3600
        payload = {"scope": scope, "repo": self.repo, "exp": exp}
        return jwt.encode(payload, self.secret, algorithm="HS256"), exp

    async def download(self, file_hash, start=None, end=None) -> bytes:
        """Fetch bytes for `file_hash`; start/end are an inclusive byte range.

        Xet-protocol download: GET the (Range-aware) reconstruction plan, fetch
        each term's xorb byte range from its presigned URL, decode the chunk
        frames, and reassemble.
        """
        token, _ = self._token("read")
        headers = {"Authorization": f"Bearer {token}"}
        if start is not None:
            headers["Range"] = f"bytes={start}-{'' if end is None else end}"
        url = f"{self.endpoint}/v1/reconstructions/{file_hash}"
        async with self.session().get(url, headers=headers) as r:
            r.raise_for_status()
            recon = await r.json()

        async def fetch_term(term) -> bytes:
            infos = recon["fetch_info"].get(term["hash"], [])
            info = next(
                (
                    f
                    for f in infos
                    if f["range"]["start"] <= term["range"]["start"]
                    and f["range"]["end"] >= term["range"]["end"]
                ),
                None,
            )
            if info is None:
                msg = f"no fetch info for xorb {term['hash']}"
                raise ValueError(msg)
            # presigned URL (token in query) — no auth header
            rng = f"bytes={info['url_range']['start']}-{info['url_range']['end']}"
            async with self.session().get(info["url"], headers={"Range": rng}) as rr:
                rr.raise_for_status()
                data = await rr.read()
            return decode_chunks(
                data,
                term["range"]["start"] - info["range"]["start"],
                term["range"]["end"] - info["range"]["start"],
            )

        parts = await asyncio.gather(*(fetch_term(t) for t in recon["terms"]))
        buf = b"".join(parts)
        # terms are chunk-aligned; trim to the exact requested byte window
        offset = recon["offset_into_first_range"]
        if start is not None and end is not None:
            return buf[offset : offset + (end - start + 1)]
        return buf[offset:]

    async def upload(self, data: bytes) -> str:
        """Chunk, dedup, and upload `data` via hf_xet; return the file hash."""
        # hf_xet's pipeline is blocking (native Rust) — run it off-loop
        return await anyio.to_thread.run_sync(self._upload_blocking, bytes(data))

    def _upload_blocking(self, data: bytes) -> str:
        import hf_xet  # noqa: PLC0415 -- heavy native dep, imported off-loop on write

        token, exp = self._token("write")
        session = hf_xet.XetSession()
        commit = session.new_upload_commit(
            endpoint=self.endpoint, token=token, token_expiry_unix_secs=exp
        )
        handle = commit.start_upload_bytes(data, sha256=hf_xet.SKIP_SHA256)
        commit.wait_to_finish()
        return handle.result().xet_info.hash
