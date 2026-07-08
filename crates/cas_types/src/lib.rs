//! Wire types for the Xet `/v1` HTTP API.
//!
//! The binary formats (xorbs, shards) and hashing live in HuggingFace's
//! `xet-core-structures` / `xet-data` crates; this crate only carries the
//! JSON types this server exchanges over HTTP.

pub mod reconstruction;
