#!/usr/bin/env python3
"""Drive OpenXet with HuggingFace's official Xet client (`hf_xet`).

Proves the server is wire-compatible with the real client: upload bytes with
chunk-level dedup, then download them back via CAS reconstruction — no OpenXet
code involved on the client side.

The only piece hf_xet does not provide is token *issuance* (on huggingface.co
that's the Hub's `xet-{read,write}-token` endpoint). OpenXet's server verifies
OIDC bearer tokens; pass one via OPENXET_TOKEN, or run the server with auth
disabled (dev) and leave it unset — the server then ignores the token.

Env: OPENXET_URL (default http://127.0.0.1:8080), OPENXET_TOKEN (optional).
"""

import os
import sys
import tempfile
import time

import hf_xet

ENDPOINT = os.environ.get("OPENXET_URL", "http://127.0.0.1:8080")
# Optional bearer token; a dev server (auth disabled) ignores it, a real one
# verifies it via OIDC/JWKS. hf_xet requires a string, so default to empty.
TOKEN = os.environ.get("OPENXET_TOKEN", "")


def cas_token() -> tuple[str, int]:
    return TOKEN, int(time.time()) + 3600


def main() -> None:
    data = os.urandom(4 * 1024 * 1024)  # 4 MiB of incompressible data

    token, exp = cas_token()
    session = hf_xet.XetSession()

    commit = session.new_upload_commit(
        endpoint=ENDPOINT, token=token, token_expiry_unix_secs=exp
    )
    handle = commit.start_upload_bytes(data, sha256=hf_xet.SKIP_SHA256)
    commit.wait_to_finish()
    info = handle.result().xet_info
    print(f"uploaded {len(data)} bytes via hf_xet -> file hash {info.hash}")

    token, exp = cas_token()
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
