#!/usr/bin/env python3
"""Drive OpenXet with HuggingFace's official Xet client (`hf_xet`).

Proves the server is wire-compatible with the real client: upload bytes with
chunk-level dedup, then download them back via CAS reconstruction — no OpenXet
code involved on the client side.

The only piece hf_xet does not provide is token *issuance* (on huggingface.co
that's the Hub's `xet-{read,write}-token` endpoint). OpenXet validates JWTs
signed with its `OPENXET_AUTH_SECRET`, so we mint one locally with PyJWT.

Env: OPENXET_URL (default http://127.0.0.1:8080), OPENXET_AUTH_SECRET.
"""

import os
import sys
import tempfile
import time

import hf_xet
import jwt

ENDPOINT = os.environ.get("OPENXET_URL", "http://127.0.0.1:8080")
SECRET = os.environ.get("OPENXET_AUTH_SECRET", "change-me-in-production")


def mint_token(scope: str) -> tuple[str, int]:
    """Mint an OpenXet JWT the same way `openxet-client` does."""
    exp = int(time.time()) + 3600
    token = jwt.encode(
        {"scope": scope, "repo": "demo/repo", "exp": exp}, SECRET, algorithm="HS256"
    )
    return token, exp


def main() -> None:
    data = os.urandom(4 * 1024 * 1024)  # 4 MiB of incompressible data

    token, exp = mint_token("write")
    session = hf_xet.XetSession()

    commit = session.new_upload_commit(
        endpoint=ENDPOINT, token=token, token_expiry_unix_secs=exp
    )
    handle = commit.start_upload_bytes(data, sha256=hf_xet.SKIP_SHA256)
    commit.wait_to_finish()
    info = handle.result().xet_info
    print(f"uploaded {len(data)} bytes via hf_xet -> file hash {info.hash}")

    token, exp = mint_token("read")
    with tempfile.TemporaryDirectory() as tmp:
        dest = os.path.join(tmp, "roundtrip.bin")
        with session.new_file_download_group(
            endpoint=ENDPOINT, token=token, token_expiry_unix_secs=exp
        ) as group:
            group.start_download_file(info, dest)
        with open(dest, "rb") as f:
            downloaded = f.read()

    if downloaded != data:
        sys.exit("FAIL: downloaded bytes differ from uploaded bytes")
    print(f"downloaded {len(downloaded)} bytes via hf_xet — byte-for-byte identical")


if __name__ == "__main__":
    main()
