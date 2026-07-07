#!/usr/bin/env python3
"""Tiny Git + Git LFS server that fronts an OpenXet CAS for HuggingFace's
`git-xet` transfer agent — the same server-side contract the HF Hub speaks.

On one port (stdlib only, git does the heavy lifting) it serves:

  * git smart-HTTP (clone/push) for one bare repo, by shelling out to
    `git upload-pack/receive-pack --stateless-rpc`;
  * the Git LFS batch API (`POST .../info/lfs/objects/batch`):
      - upload:   negotiates the `xet` transfer agent and hands git-xet the
        CAS URL + a freshly minted OpenXet JWT via `X-Xet-*` action headers;
      - download: standard LFS `basic` transfer whose href is OpenXet's
        `GET /v1/content/sha256:<oid>` (git-xet does not implement downloads
        — plain git-lfs fetches the bytes over HTTP);
  * `GET /xet-token` — the token refresh route git-xet calls if its CAS
    token expires mid-transfer (camelCase `CasJWTInfo` JSON).

Env: OPENXET_URL, OPENXET_AUTH_SECRET (must match the server), GIT_REPO
(path to the bare repo), PORT (default 8175).
"""

import base64
import gzip
import hashlib
import hmac
import json
import os
import subprocess
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

OPENXET_URL = os.environ.get("OPENXET_URL", "http://127.0.0.1:8080")
SECRET = os.environ["OPENXET_AUTH_SECRET"]
GIT_REPO = os.environ["GIT_REPO"]
PORT = int(os.environ.get("PORT", "8175"))
TOKEN_TTL_SECS = 3600
LFS_JSON = "application/vnd.git-lfs+json"


def b64url(data: bytes) -> bytes:
    return base64.urlsafe_b64encode(data).rstrip(b"=")


def mint_jwt(scope: str) -> tuple[str, int]:
    """HS256 JWT with the claims OpenXet validates: {scope, repo, exp}."""
    exp = int(time.time()) + TOKEN_TTL_SECS
    seg = lambda obj: b64url(json.dumps(obj, separators=(",", ":")).encode())
    signing = seg({"alg": "HS256", "typ": "JWT"}) + b"." + seg(
        {"scope": scope, "repo": "demo/repo", "exp": exp}
    )
    sig = hmac.new(SECRET.encode(), signing, hashlib.sha256).digest()
    return (signing + b"." + b64url(sig)).decode(), exp


def pkt_line(line: str) -> bytes:
    data = line.encode()
    return b"%04x" % (len(data) + 4) + data


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _send(self, code: int, body: bytes = b"", ctype: str = LFS_JSON) -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        self.wfile.write(body)

    def _read_body(self) -> bytes:
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            data = b""
            while True:
                size = int(self.rfile.readline().strip(), 16)
                chunk = self.rfile.read(size)
                self.rfile.readline()  # trailing CRLF
                if size == 0:
                    break
                data += chunk
        else:
            data = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if self.headers.get("Content-Encoding") == "gzip":
            data = gzip.decompress(data)
        return data

    def _git(self, service: str, *args: str, input: bytes | None = None) -> bytes:
        # service is "git-upload-pack" or "git-receive-pack"
        cmd = ["git", service.removeprefix("git-"), "--stateless-rpc", *args, GIT_REPO]
        return subprocess.run(cmd, input=input, capture_output=True, check=True).stdout

    def do_GET(self) -> None:  # noqa: N802 (http.server API)
        url = urlparse(self.path)
        if url.path.endswith("/info/refs"):
            service = parse_qs(url.query).get("service", [""])[0]
            if service not in ("git-upload-pack", "git-receive-pack"):
                return self._send(400, b"smart HTTP only", "text/plain")
            body = pkt_line(f"# service={service}\n") + b"0000"
            body += self._git(service, "--advertise-refs")
            return self._send(200, body, f"application/x-{service}-advertisement")
        if url.path == "/xet-token":
            token, exp = mint_jwt("write")
            info = {"casUrl": OPENXET_URL, "accessToken": token, "exp": exp}
            return self._send(200, json.dumps(info).encode(), "application/json")
        self._send(404, b"not found", "text/plain")

    def do_POST(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        service = path.rsplit("/", 1)[-1]
        if service in ("git-upload-pack", "git-receive-pack"):
            out = self._git(service, input=self._read_body())
            return self._send(200, out, f"application/x-{service}-result")
        if path.endswith("/info/lfs/objects/batch"):
            return self._batch(json.loads(self._read_body()))
        if "/info/lfs/locks" in path:
            msg = {"message": "locking not supported"}
            return self._send(404, json.dumps(msg).encode())
        self._send(404, b"not found", "text/plain")

    def _batch(self, req: dict) -> None:
        op = req.get("operation")
        objects = req.get("objects", [])
        if op == "upload":
            # git-lfs advertises "xet" only when the agent is configured
            # (lfs.customtransfer.xet.*). Uploads REQUIRE it: this server has
            # no basic upload path — bytes live in the CAS, not here.
            if "xet" not in req.get("transfers", []):
                msg = {"message": "uploads require the git-xet transfer agent (run `git xet install`)"}
                return self._send(422, json.dumps(msg).encode())
            token, exp = mint_jwt("write")
            host = self.headers.get("Host", f"127.0.0.1:{PORT}")
            action = {
                "href": f"http://{host}/xet-token",  # git-xet's token refresh route
                "header": {
                    "X-Xet-Cas-Url": OPENXET_URL,
                    "X-Xet-Access-Token": token,
                    "X-Xet-Token-Expiration": str(exp),
                    "X-Xet-Session-Id": uuid.uuid4().hex,
                },
            }
            resp = {
                "transfer": "xet",
                "objects": [
                    {"oid": o["oid"], "size": o["size"], "authenticated": True,
                     "actions": {"upload": action}}
                    for o in objects
                ],
            }
        elif op == "download":
            token, _ = mint_jwt("read")
            resp = {
                "transfer": "basic",
                "objects": [
                    {"oid": o["oid"], "size": o["size"], "authenticated": True,
                     "actions": {"download": {
                         "href": f"{OPENXET_URL}/v1/content/sha256:{o['oid']}",
                         "header": {"Authorization": f"Bearer {token}"},
                     }}}
                    for o in objects
                ],
            }
        else:
            msg = {"message": f"unsupported operation: {op}"}
            return self._send(400, json.dumps(msg).encode())
        self._send(200, json.dumps(resp).encode())

    def log_message(self, fmt: str, *args) -> None:
        print(f"[lfs-server] {self.command} {self.path} -> {args[1] if len(args) > 1 else ''}")


if __name__ == "__main__":
    print(f"[lfs-server] serving git repo {GIT_REPO} + LFS/xet API on 127.0.0.1:{PORT}")
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
