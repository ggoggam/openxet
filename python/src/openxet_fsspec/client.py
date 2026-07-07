from __future__ import annotations

import time

import aiohttp
import anyio


class CasClient:
    """OpenXet /v1 data plane: hf_xet for upload, plain ranged GET for download."""

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
        """Fetch bytes for `file_hash`; start/end are an inclusive byte range."""
        token, _ = self._token("read")
        headers = {"Authorization": f"Bearer {token}"}
        if start is not None:
            headers["Range"] = f"bytes={start}-{'' if end is None else end}"
        url = f"{self.endpoint}/v1/content/{file_hash}"
        async with self.session().get(url, headers=headers) as r:
            r.raise_for_status()
            return await r.read()

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
