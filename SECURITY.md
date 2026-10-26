# Security

OpenXet stores other people's data and decides who may read it, so please report anything that
weakens that privately, through
[GitHub's private vulnerability reporting](https://github.com/ggoggam/openxet/security/advisories/new),
not in a public issue or pull request.

## What to report

Anything that lets a client read, change or delete data it shouldn't, or take down a server
others depend on. For example:

- **Authentication:** accepting a token the configured OIDC issuers didn't sign, an expired token
  or one meant for another audience, or treating a request as authenticated when it isn't.
- **Authorization:** acting beyond a token's `openxet:read`, `openxet:write` or `openxet:admin`
  role, or reading or releasing another owner's files and claims without admin.
- **Signed fetch URLs:** forging, reusing past expiry, or widening the HMAC-signed xorb URLs or
  the presigned S3 URLs that reconstruction responses hand out.
- **Deduplication:** learning whether someone else has stored given content, or getting chunks
  of theirs, from chunk or dedup queries.
- **Untrusted input:** xorbs, shards or requests that crash the server, exhaust its memory or
  disk, write outside its data directory, or corrupt stored data.
- **Web UI:** cross-site scripting or similar through file names, metadata or file contents.

Not vulnerabilities in themselves:

- A server run with `OPENXET_AUTH_ENABLED=false`. That mode has no access control by design and
  is meant for local development.
- The development credentials in `docker/` and the mise tasks (such as RustFS's
  `rustfsadmin`), which are for local use only.
- Bugs in HuggingFace's `xet-core` crates, which OpenXet uses for its formats, hashing and
  chunking. Report those [upstream](https://github.com/huggingface/xet-core/security). If one
  affects OpenXet in a way it could defend against, report that here as well.

## How to report

Include what's affected, how to reproduce it (a request sequence or a small script is ideal), the
commit or version you tested and the backend (filesystem or S3) and auth setup you used. Leave
real tokens, secrets and other people's data out of the report: a description of what leaks is
enough.

## Supported versions

OpenXet has no releases yet. Fixes land on `main`, and only the latest `main` is supported.
