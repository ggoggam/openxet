# OpenXet examples

## `hf-xet-client/` — wire-compatibility proof with the official HuggingFace client

```bash
examples/hf-xet-client/demo.sh
```

Demonstrates that OpenXet speaks the exact Xet wire protocol (`/v1/*` endpoints)
that HuggingFace's official [`hf_xet`](https://pypi.org/project/hf-xet/) Python
package uses. The stock `huggingface_hub` client can point at your OpenXet server
via the `endpoint` parameter, upload and download files with chunk-level
deduplication, and everything works unmodified.

The only piece left to the server operator is token *issuance* — on
huggingface.co that's the Hub's `xet-{read,write}-token` endpoint. For
self-hosted deployments, the demo mints a JWT locally and hands it to
`hf_xet.XetSession`; production setups would wire this to an OIDC issuer
(OpenXet verifies against JWKS).
