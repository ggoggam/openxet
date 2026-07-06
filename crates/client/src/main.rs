//! openxet-client — a reference client for the Xet CAS wire protocol (`/v1/*`).
//!
//! This client performs **chunk-level, cross-revision deduplication**
//! the same way HuggingFace's `xet-core` does:
//!
//!   put (upload):
//!     1. content-defined chunk the file, hash each chunk (Blake3 keyed)
//!     2. ask GET /v1/chunks/default-merkledb/{hash} which chunks already exist
//!        (the response is an HMAC-protected shard; we match our chunk hashes
//!         against it to discover (xorb, index) for already-stored chunks)
//!     3. pack only the NEW chunks into xorbs and POST /v1/xorbs/default/{hash}
//!     4. POST /v1/shards a shard whose file reconstruction references both the
//!        new xorbs and the pre-existing ones
//!     -> prints the file hash (the "pointer" identity)
//!
//!   get (download):
//!     GET /v1/reconstructions/{file_id}, fetch the byte ranges named in
//!     fetch_info, decompress, and concatenate -> the original bytes.
//!
//! It reuses the workspace's own hashing/chunking/cas-types crates, so it is
//! byte-for-byte wire compatible with the server by construction.

use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;

use openxet_cas_types::chunk::CompressionType;
use openxet_cas_types::reconstruction::QueryReconstructionResponse;
use openxet_cas_types::shard::{
    CASChunkSequenceEntry, CASChunkSequenceHeader, CASInfoBlock, FileDataSequenceEntry,
    FileDataSequenceHeader, FileInfoBlock, FileVerificationEntry, MDB_FILE_FLAG_WITH_VERIFICATION,
    Shard, ShardHeader,
};
use openxet_cas_types::xorb::{
    XORB_SOFT_LIMIT, compute_xorb_hash, deserialize_xorb, serialize_single_chunk,
};
use openxet_chunking::chunk_data;
use openxet_hashing::{
    MerkleHash, compute_chunk_hash, compute_file_hash, compute_verification_hash,
};

type HmacSha256 = Hmac<Sha256>;

#[derive(Parser)]
#[command(name = "openxet-client", about = "Reference Xet CAS protocol client")]
struct Cli {
    /// Base URL of the OpenXet server.
    #[arg(long, env = "OPENXET_URL", default_value = "http://127.0.0.1:8080")]
    url: String,

    /// JWT signing secret (must match the server's OPENXET_AUTH_SECRET).
    #[arg(
        long,
        env = "OPENXET_AUTH_SECRET",
        default_value = "change-me-in-production"
    )]
    secret: String,

    /// Repository id placed in the token claims.
    #[arg(long, default_value = "demo/repo")]
    repo: String,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Upload a file with chunk-level dedup; prints the file hash.
    Put {
        /// File to upload, or "-" for stdin.
        file: String,
        /// Also print a short dedup report to stderr.
        #[arg(long)]
        stats: bool,
    },
    /// Download a file by its hash; writes bytes to --out (or stdout).
    Get {
        /// 64-char hex file hash.
        hash: String,
        /// Output path, or "-" for stdout.
        #[arg(long, default_value = "-")]
        out: String,
    },
}

#[derive(Serialize)]
struct Claims<'a> {
    scope: &'a str,
    repo: &'a str,
    exp: usize,
}

fn mint_token(secret: &str, scope: &str, repo: &str) -> Result<String> {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as usize
        + 3600;
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &Claims { scope, repo, exp },
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok(token)
}

fn hmac_chunk(key: &[u8; 32], hash: &MerkleHash) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("valid key length");
    mac.update(hash.as_bytes());
    mac.finalize().into_bytes().into()
}

fn read_input(path: &str) -> Result<Vec<u8>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        std::fs::read(path).with_context(|| format!("reading {path}"))
    }
}

/// Where a file chunk lives, once resolved.
#[derive(Clone)]
struct Placement {
    xorb_hash: String,
    index_in_xorb: u32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Command::Put { file, stats } => cmd_put(&cli, file, *stats),
        Command::Get { hash, out } => cmd_get(&cli, hash, out),
    }
}

// ─── upload ──────────────────────────────────────────────────────────────────

fn cmd_put(cli: &Cli, file: &str, stats: bool) -> Result<()> {
    let data = read_input(file)?;
    if data.is_empty() {
        bail!("refusing to upload empty input");
    }
    let token = mint_token(&cli.secret, "write", &cli.repo)?;
    let http = reqwest::blocking::Client::new();

    // 1. Chunk + hash.
    let chunk_infos = chunk_data(&data);
    let chunk_slices: Vec<&[u8]> = chunk_infos
        .iter()
        .map(|ci| &data[ci.offset..ci.offset + ci.length])
        .collect();
    let chunk_hashes: Vec<MerkleHash> =
        chunk_slices.iter().map(|d| compute_chunk_hash(d)).collect();
    let sizes: Vec<usize> = chunk_slices.iter().map(|d| d.len()).collect();

    let hashes_and_sizes: Vec<(MerkleHash, usize)> = chunk_hashes
        .iter()
        .copied()
        .zip(sizes.iter().copied())
        .collect();
    let file_hash = compute_file_hash(&hashes_and_sizes);

    // 2. Dedup query pass: resolve as many chunks as possible to existing xorbs.
    let n = chunk_hashes.len();
    let mut placement: Vec<Option<Placement>> = vec![None; n];
    let mut queries = 0usize;

    let mut i = 0;
    while i < n {
        if placement[i].is_some() {
            i += 1;
            continue;
        }
        queries += 1;
        let url = format!(
            "{}/v1/chunks/default-merkledb/{}",
            cli.url,
            chunk_hashes[i].to_hex()
        );
        let resp = http.get(&url).bearer_auth(&token).send()?;
        if resp.status().as_u16() == 404 {
            // Not stored anywhere; leave unresolved (becomes a new chunk).
            i += 1;
            continue;
        }
        if !resp.status().is_success() {
            bail!("dedup query failed ({}): {}", resp.status(), url);
        }

        // Parse the HMAC-protected shard response.
        let body = resp.bytes()?;
        let shard = Shard::from_bytes(&body).context("parsing dedup shard")?;
        let footer = shard
            .footer
            .as_ref()
            .context("dedup shard missing footer")?;
        let key = &footer.chunk_hash_hmac_key;

        // Build: HMAC(chunk_hash) -> (xorb_hash, index_in_xorb)
        let mut by_hmac: HashMap<[u8; 32], Placement> = HashMap::new();
        for block in &shard.cas_info_blocks {
            let xorb_hash = block.header.cas_hash.to_hex();
            for (idx, entry) in block.entries.iter().enumerate() {
                by_hmac.insert(
                    *entry.chunk_hash.as_bytes(),
                    Placement {
                        xorb_hash: xorb_hash.clone(),
                        index_in_xorb: idx as u32,
                    },
                );
            }
        }

        // Resolve every still-unresolved chunk this response covers.
        for j in 0..n {
            if placement[j].is_some() {
                continue;
            }
            if let Some(p) = by_hmac.get(&hmac_chunk(key, &chunk_hashes[j])) {
                placement[j] = Some(p.clone());
            }
        }
        i += 1;
    }

    // 3. Pack the still-unresolved (new) chunks into xorbs and upload them.
    //    `built` records, for each xorb we create this run, its hash and the
    //    global chunk indices it holds in order — used to emit CAS info below.
    let new_chunk_count = placement.iter().filter(|p| p.is_none()).count();
    let mut built: Vec<(String, Vec<usize>)> = Vec::new();
    {
        // Greedy fill by serialized size, like the server's pipeline.
        let mut group: Vec<usize> = Vec::new(); // global chunk indices
        let mut group_bytes = 0usize;

        for idx in 0..n {
            if placement[idx].is_some() {
                continue;
            }
            let serialized = serialize_single_chunk(chunk_slices[idx], CompressionType::Lz4)?;
            if !group.is_empty() && group_bytes + serialized.len() > XORB_SOFT_LIMIT {
                let hash = upload_xorb(&http, &cli.url, &token, &chunk_slices, &group)?;
                for (local_idx, &gi) in group.iter().enumerate() {
                    placement[gi] = Some(Placement {
                        xorb_hash: hash.clone(),
                        index_in_xorb: local_idx as u32,
                    });
                }
                built.push((hash, std::mem::take(&mut group)));
                group_bytes = 0;
            }
            group_bytes += serialized.len();
            group.push(idx);
        }
        if !group.is_empty() {
            let hash = upload_xorb(&http, &cli.url, &token, &chunk_slices, &group)?;
            for (local_idx, &gi) in group.iter().enumerate() {
                placement[gi] = Some(Placement {
                    xorb_hash: hash.clone(),
                    index_in_xorb: local_idx as u32,
                });
            }
            built.push((hash, group));
        }
    }
    let new_xorb_count = built.len();

    // 4. Build the shard: coalesce consecutive chunks sharing a xorb + adjacent
    //    indices into reconstruction terms; add a verification entry per term;
    //    emit CAS info for the xorbs we built this run (existing ones are
    //    already registered server-side).
    let placement: Vec<Placement> = placement
        .into_iter()
        .map(|p| p.expect("every chunk resolved or uploaded"))
        .collect();

    let mut entries: Vec<FileDataSequenceEntry> = Vec::new();
    let mut verifications: Vec<FileVerificationEntry> = Vec::new();

    let mut t = 0;
    while t < n {
        let xorb = &placement[t].xorb_hash;
        let start_idx = placement[t].index_in_xorb;
        let mut end = t + 1;
        let mut expected_idx = start_idx + 1;
        while end < n
            && &placement[end].xorb_hash == xorb
            && placement[end].index_in_xorb == expected_idx
        {
            expected_idx += 1;
            end += 1;
        }

        let unpacked: u32 = (t..end).map(|k| sizes[k] as u32).sum();
        entries.push(FileDataSequenceEntry {
            cas_hash: MerkleHash::from_hex(xorb).expect("valid xorb hash"),
            cas_flags: 0,
            unpacked_segment_bytes: unpacked,
            chunk_index_start: start_idx,
            chunk_index_end: placement[end - 1].index_in_xorb + 1,
        });
        let term_hashes: Vec<MerkleHash> = (t..end).map(|k| chunk_hashes[k]).collect();
        verifications.push(FileVerificationEntry {
            range_hash: compute_verification_hash(&term_hashes),
        });
        t = end;
    }

    // CAS info for the xorbs we built this run, so the server can register
    // their chunks for future dedup. Entries are in xorb order (local index).
    let mut cas_info_blocks = Vec::new();
    for (xorb_hash, group) in &built {
        let mut byte_offset = 0u32;
        let mut num_bytes = 0u32;
        let cas_entries: Vec<CASChunkSequenceEntry> = group
            .iter()
            .map(|&k| {
                let size = sizes[k] as u32;
                let entry = CASChunkSequenceEntry {
                    chunk_hash: chunk_hashes[k],
                    chunk_byte_range_start: byte_offset,
                    unpacked_segment_bytes: size,
                };
                byte_offset += size;
                num_bytes += size;
                entry
            })
            .collect();
        cas_info_blocks.push(CASInfoBlock {
            header: CASChunkSequenceHeader {
                cas_hash: MerkleHash::from_hex(xorb_hash).expect("valid xorb hash"),
                cas_flags: 0,
                num_entries: cas_entries.len() as u32,
                num_bytes_in_cas: num_bytes,
                num_bytes_on_disk: 0, // informational; server does not verify
            },
            entries: cas_entries,
        });
    }

    let shard = Shard {
        header: ShardHeader::new(0),
        file_info_blocks: vec![FileInfoBlock {
            header: FileDataSequenceHeader {
                file_hash,
                file_flags: MDB_FILE_FLAG_WITH_VERIFICATION,
                num_entries: entries.len() as u32,
            },
            entries,
            verification_entries: verifications,
            metadata_ext: None,
        }],
        cas_info_blocks,
        footer: None,
    };
    let shard_bytes = shard.to_upload_bytes()?;

    let resp = http
        .post(format!("{}/v1/shards", cli.url))
        .bearer_auth(&token)
        .header("content-type", "application/octet-stream")
        .body(shard_bytes)
        .send()?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        bail!("shard upload failed ({status}): {text}");
    }

    if stats {
        eprintln!(
            "openxet-client: {} chunks, {} new ({} new xorb(s)), {} dedup quer{}",
            n,
            new_chunk_count,
            new_xorb_count,
            queries,
            if queries == 1 { "y" } else { "ies" }
        );
    }

    println!("{}", file_hash.to_hex());
    Ok(())
}

/// Serialize `group`'s chunks into a xorb, POST it, and return its hash hex.
fn upload_xorb(
    http: &reqwest::blocking::Client,
    base_url: &str,
    token: &str,
    chunk_slices: &[&[u8]],
    group: &[usize],
) -> Result<String> {
    let mut xorb_bytes = Vec::new();
    let mut hs: Vec<(MerkleHash, usize)> = Vec::with_capacity(group.len());
    for &gi in group {
        let slice = chunk_slices[gi];
        xorb_bytes.extend_from_slice(&serialize_single_chunk(slice, CompressionType::Lz4)?);
        hs.push((compute_chunk_hash(slice), slice.len()));
    }
    let xorb_hash = compute_xorb_hash(&hs).to_hex();

    let resp = http
        .post(format!("{base_url}/v1/xorbs/default/{xorb_hash}"))
        .bearer_auth(token)
        .header("content-type", "application/octet-stream")
        .body(xorb_bytes)
        .send()?;
    if !resp.status().is_success() {
        bail!("xorb upload failed ({}) for {xorb_hash}", resp.status());
    }
    Ok(xorb_hash)
}

// ─── download ──────────────────────────────────────────────────────────────

fn cmd_get(cli: &Cli, hash: &str, out: &str) -> Result<()> {
    let token = mint_token(&cli.secret, "read", &cli.repo)?;
    let http = reqwest::blocking::Client::new();

    let resp = http
        .get(format!("{}/v1/reconstructions/{}", cli.url, hash))
        .bearer_auth(&token)
        .send()?;
    if !resp.status().is_success() {
        bail!("reconstruction failed ({}) for {hash}", resp.status());
    }
    let recon: QueryReconstructionResponse = resp.json()?;

    let mut file_bytes: Vec<u8> = Vec::new();
    for (ti, term) in recon.terms.iter().enumerate() {
        let fetch = recon
            .fetch_info
            .get(&term.hash)
            .and_then(|v| v.iter().find(|f| f.range.contains_range(&term.range)))
            .or_else(|| recon.fetch_info.get(&term.hash).and_then(|v| v.first()))
            .with_context(|| format!("no fetch_info for xorb {}", term.hash))?;

        let range = format!("bytes={}-{}", fetch.url_range.start, fetch.url_range.end);
        let xorb_part = http
            .get(&fetch.url)
            .bearer_auth(&token)
            .header("range", range)
            .send()?;
        if !xorb_part.status().is_success() {
            bail!(
                "xorb fetch failed ({}) for {}",
                xorb_part.status(),
                term.hash
            );
        }
        let part = xorb_part.bytes()?;

        // The fetched range begins at the term's first chunk boundary, so the
        // returned bytes deserialize as a standalone chunk sequence.
        let chunks = deserialize_xorb(&part)
            .with_context(|| format!("decoding xorb range for term {ti}"))?;
        for c in chunks {
            file_bytes.extend_from_slice(&c.data);
        }
    }

    // Honor offset_into_first_range (0 for a full-file download).
    let start = recon.offset_into_first_range as usize;
    let body = &file_bytes[start.min(file_bytes.len())..];

    if out == "-" {
        std::io::stdout().write_all(body)?;
    } else {
        std::fs::write(out, body).with_context(|| format!("writing {out}"))?;
    }
    Ok(())
}
