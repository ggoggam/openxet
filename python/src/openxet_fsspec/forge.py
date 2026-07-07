from __future__ import annotations

import base64

import aiohttp


class Forge:
    """Git forge contents API (GitHub and Gitea share the same shape).

    Differences handled here: file creation is PUT on GitHub but POST on
    Gitea, and the token header scheme differs.
    """

    def __init__(
        self, api_base, owner, repo, ref="main", token=None, auth=None, flavor="github"
    ):
        self.contents = f"{api_base.rstrip('/')}/repos/{owner}/{repo}/contents"
        self.ref = ref
        self.flavor = flavor
        self._headers = {}
        self._auth = aiohttp.BasicAuth(*auth) if auth else None
        if token:
            scheme = "Bearer" if flavor == "github" else "token"
            self._headers["Authorization"] = f"{scheme} {token}"
        self._session = None

    def session(self) -> aiohttp.ClientSession:
        # lazily created so the connection pool binds to the loop actually
        # running the coroutines (fsspec's internal loop, or the caller's)
        if self._session is None:
            self._session = aiohttp.ClientSession(
                headers=self._headers,
                auth=self._auth,
                timeout=aiohttp.ClientTimeout(total=30),
            )
        return self._session

    async def close(self) -> None:
        if self._session is not None:
            await self._session.close()
            self._session = None

    def _url(self, path) -> str:
        return f"{self.contents}/{path.lstrip('/')}"

    async def stat(self, path) -> dict | list | None:
        """Contents API response for `path`: dict (file), list (dir), or None."""
        async with self.session().get(self._url(path), params={"ref": self.ref}) as r:
            if r.status == 404:
                return None
            r.raise_for_status()
            return await r.json()

    async def read(self, path) -> bytes:
        st = await self.stat(path)
        if st is None or isinstance(st, list):
            raise FileNotFoundError(path)
        if st.get("content"):
            return base64.b64decode(st["content"])
        if st.get("size", 0) == 0:
            return b""
        # GitHub omits inline content for blobs >1 MiB; fall back to the raw URL
        async with self.session().get(st["download_url"]) as r:
            r.raise_for_status()
            return await r.read()

    async def write(self, path, data: bytes, message: str) -> None:
        st = await self.stat(path)
        body = {
            "content": base64.b64encode(data).decode(),
            "message": message,
            "branch": self.ref,
        }
        if isinstance(st, dict):  # update
            body["sha"] = st["sha"]
            method = "PUT"
        else:  # create: PUT on GitHub, POST on Gitea
            method = "PUT" if self.flavor == "github" else "POST"
        async with self.session().request(method, self._url(path), json=body) as r:
            r.raise_for_status()

    async def delete(self, path, message: str) -> None:
        st = await self.stat(path)
        if not isinstance(st, dict):
            raise FileNotFoundError(path)
        body = {"message": message, "branch": self.ref, "sha": st["sha"]}
        async with self.session().delete(self._url(path), json=body) as r:
            r.raise_for_status()
