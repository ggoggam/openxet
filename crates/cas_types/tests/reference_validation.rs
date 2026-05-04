//! Integration tests validating our implementation against the official
//! xet-spec-reference-files from HuggingFace.
//!
//! Reference: https://huggingface.co/datasets/xet-team/xet-spec-reference-files
//!
//! Reference files are downloaded on first use into a temp directory and
//! reused across runs. Set `OPENXET_TEST_DATA_DIR` to override the cache
//! location, or pre-populate it to run offline.
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use openxet_cas_types::shard::Shard;
use openxet_cas_types::xorb;
use openxet_chunking::chunk_data;
use openxet_hashing::{
    MerkleHash, compute_chunk_hash, compute_file_hash, compute_merkle_root,
    compute_verification_hash,
};

const HF_BASE_URL: &str =
    "https://huggingface.co/datasets/xet-team/xet-spec-reference-files/resolve/main/";

static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());

fn test_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENXET_TEST_DATA_DIR") {
        return PathBuf::from(dir);
    }
    std::env::temp_dir().join("openxet-test-data")
}

/// Map our short reference name to the upstream filename in the HF dataset.
fn upstream_filename(name: &str) -> &'static str {
    match name {
        "chunk1.bin" => {
            "b10aa1dc71c61661de92280c41a188aabc47981739b785724a099945d8dc5ce4.chunk"
        }
        "chunk2.bin" => {
            "26255591fa803b6baf25d88c315b8a6f5153d5bcfdf18ec5ef526264e0ccc907.chunk"
        }
        "chunk3.bin" => {
            "099cb228194fe640e36a6c7d274ee5ed3a714ccd557a0951d9b6b43a7292b5d1.chunk"
        }
        "ev_data.csv" => "Electric_Vehicle_Population_Data_20250917.csv",
        "chunks.txt" => "Electric_Vehicle_Population_Data_20250917.csv.chunks",
        "xorb_hash.txt" => "Electric_Vehicle_Population_Data_20250917.csv.xet-xorb-hash",
        "file_hash.txt" => "Electric_Vehicle_Population_Data_20250917.csv.xet-file-hash",
        "range_hash.txt" => {
            "eea25d6ee393ccae385820daed127b96ef0ea034dfb7cf6da3a950ce334b7632.xorb.range-hash"
        }
        "reference.xorb" => {
            "eea25d6ee393ccae385820daed127b96ef0ea034dfb7cf6da3a950ce334b7632.xorb"
        }
        "reference_shard_full.bin" => {
            "Electric_Vehicle_Population_Data_20250917.csv.shard.verification"
        }
        "reference_shard_nofooter.bin" => {
            "Electric_Vehicle_Population_Data_20250917.csv.shard.verification-no-footer"
        }
        "reference_shard_dedupe.bin" => {
            "Electric_Vehicle_Population_Data_20250917.csv.shard.dedupe"
        }
        other => panic!("unknown reference file: {other}"),
    }
}

fn test_data_path(name: &str) -> PathBuf {
    let path = test_data_dir().join(name);
    if path.exists() {
        return path;
    }

    let _guard = DOWNLOAD_LOCK.lock().unwrap();
    if path.exists() {
        return path;
    }

    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let url = format!("{HF_BASE_URL}{}", upstream_filename(name));
    let bytes = reqwest::blocking::get(&url)
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .unwrap_or_else(|e| panic!("failed to download {url}: {e}"));

    let tmp = path.with_extension("download");
    fs::write(&tmp, &bytes).unwrap();
    fs::rename(&tmp, &path).unwrap();
    path
}

/// Parse the chunks.txt reference file into a list of (hash_hex, size) pairs.
fn parse_chunks_file(path: &std::path::PathBuf) -> Vec<(String, usize)> {
    let contents = fs::read_to_string(path).unwrap();
    contents
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let hash_hex = parts[0].to_string();
            let size: usize = parts[1].parse().unwrap();
            (hash_hex, size)
        })
        .collect()
}

// ─── Chunk Hash Tests ────────────────────────────────────────────────────────

#[test]
fn test_chunk_hash_reference_chunk1() {
    let data = fs::read(test_data_path("chunk1.bin")).unwrap();
    let hash = compute_chunk_hash(&data);
    let expected_hex = "b10aa1dc71c61661de92280c41a188aabc47981739b785724a099945d8dc5ce4";
    assert_eq!(hash.to_hex(), expected_hex, "chunk1 hash mismatch");
}

#[test]
fn test_chunk_hash_reference_chunk2() {
    let data = fs::read(test_data_path("chunk2.bin")).unwrap();
    let hash = compute_chunk_hash(&data);
    let expected_hex = "26255591fa803b6baf25d88c315b8a6f5153d5bcfdf18ec5ef526264e0ccc907";
    assert_eq!(hash.to_hex(), expected_hex, "chunk2 hash mismatch");
}

#[test]
fn test_chunk_hash_reference_chunk3() {
    let data = fs::read(test_data_path("chunk3.bin")).unwrap();
    let hash = compute_chunk_hash(&data);
    let expected_hex = "099cb228194fe640e36a6c7d274ee5ed3a714ccd557a0951d9b6b43a7292b5d1";
    assert_eq!(hash.to_hex(), expected_hex, "chunk3 hash mismatch");
}

// ─── Chunking Boundary Tests ─────────────────────────────────────────────────

#[test]
fn test_chunking_boundaries_match_reference() {
    let csv_data = fs::read(test_data_path("ev_data.csv")).unwrap();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));

    let chunks = chunk_data(&csv_data);

    assert_eq!(
        chunks.len(),
        ref_chunks.len(),
        "chunk count mismatch: got {}, expected {}",
        chunks.len(),
        ref_chunks.len()
    );

    // Verify chunk sizes match
    for (i, (chunk, (_, expected_size))) in chunks.iter().zip(ref_chunks.iter()).enumerate() {
        assert_eq!(
            chunk.length, *expected_size,
            "chunk {} size mismatch: got {}, expected {}",
            i, chunk.length, *expected_size
        );
    }
}

#[test]
fn test_chunk_hashes_match_reference() {
    let csv_data = fs::read(test_data_path("ev_data.csv")).unwrap();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));

    let chunks = chunk_data(&csv_data);

    // Verify chunk hashes match
    for (i, (chunk, (expected_hash, _))) in chunks.iter().zip(ref_chunks.iter()).enumerate() {
        let chunk_bytes = &csv_data[chunk.offset..chunk.offset + chunk.length];
        let hash = compute_chunk_hash(chunk_bytes);
        assert_eq!(
            hash.to_hex(),
            *expected_hash,
            "chunk {} hash mismatch at offset {}",
            i,
            chunk.offset
        );
    }
}

// ─── Xorb Hash Test ─────────────────────────────────────────────────────────

#[test]
fn test_xorb_hash_matches_reference() {
    let csv_data = fs::read(test_data_path("ev_data.csv")).unwrap();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));
    let expected_xorb_hash = fs::read_to_string(test_data_path("xorb_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    let chunks = chunk_data(&csv_data);

    // Compute chunk hashes and sizes
    let chunk_hashes_and_sizes: Vec<(MerkleHash, usize)> = chunks
        .iter()
        .map(|c| {
            let chunk_bytes = &csv_data[c.offset..c.offset + c.length];
            (compute_chunk_hash(chunk_bytes), c.length)
        })
        .collect();

    // Verify we have the right number of chunks
    assert_eq!(chunk_hashes_and_sizes.len(), ref_chunks.len());

    // Xorb hash = merkle root of (chunk_hash, chunk_size) pairs
    let xorb_hash = compute_merkle_root(&chunk_hashes_and_sizes);
    assert_eq!(xorb_hash.to_hex(), expected_xorb_hash, "xorb hash mismatch");
}

// ─── File Hash Test ──────────────────────────────────────────────────────────

#[test]
fn test_file_hash_matches_reference() {
    let csv_data = fs::read(test_data_path("ev_data.csv")).unwrap();
    let expected_file_hash = fs::read_to_string(test_data_path("file_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    let chunks = chunk_data(&csv_data);

    let chunk_hashes_and_sizes: Vec<(MerkleHash, usize)> = chunks
        .iter()
        .map(|c| {
            let chunk_bytes = &csv_data[c.offset..c.offset + c.length];
            (compute_chunk_hash(chunk_bytes), c.length)
        })
        .collect();

    let file_hash = compute_file_hash(&chunk_hashes_and_sizes);
    assert_eq!(file_hash.to_hex(), expected_file_hash, "file hash mismatch");
}

// ─── Verification/Range Hash Test ────────────────────────────────────────────

#[test]
fn test_verification_range_hash_matches_reference() {
    let csv_data = fs::read(test_data_path("ev_data.csv")).unwrap();
    let expected_range_hash = fs::read_to_string(test_data_path("range_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    let chunks = chunk_data(&csv_data);

    let chunk_hashes: Vec<MerkleHash> = chunks
        .iter()
        .map(|c| {
            let chunk_bytes = &csv_data[c.offset..c.offset + c.length];
            compute_chunk_hash(chunk_bytes)
        })
        .collect();

    // The range hash covers all 796 chunks (single term for entire file)
    let range_hash = compute_verification_hash(&chunk_hashes);
    assert_eq!(
        range_hash.to_hex(),
        expected_range_hash,
        "verification range hash mismatch"
    );
}

// ─── Xorb Deserialization Test ───────────────────────────────────────────────

#[test]
fn test_xorb_deserialization() {
    let xorb_data = fs::read(test_data_path("reference.xorb")).unwrap();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));

    let deserialized = xorb::deserialize_xorb(&xorb_data).unwrap();

    assert_eq!(
        deserialized.len(),
        ref_chunks.len(),
        "xorb chunk count mismatch: got {}, expected {}",
        deserialized.len(),
        ref_chunks.len()
    );

    // Verify each chunk size matches and hash matches
    for (i, (chunk, (expected_hash, expected_size))) in
        deserialized.iter().zip(ref_chunks.iter()).enumerate()
    {
        assert_eq!(
            chunk.data.len(),
            *expected_size,
            "xorb chunk {} size mismatch: got {}, expected {}",
            i,
            chunk.data.len(),
            *expected_size
        );

        let hash = compute_chunk_hash(&chunk.data);
        assert_eq!(
            hash.to_hex(),
            *expected_hash,
            "xorb chunk {} hash mismatch",
            i
        );
    }
}

// ─── Shard Deserialization Tests ─────────────────────────────────────────────

#[test]
fn test_shard_deserialization_no_footer() {
    let shard_data = fs::read(test_data_path("reference_shard_nofooter.bin")).unwrap();
    let shard = Shard::from_bytes(&shard_data).unwrap();

    // This shard is for one file upload (no footer)
    assert_eq!(shard.header.footer_size, 0);
    assert!(shard.footer.is_none());

    // Should have file info blocks
    assert!(
        !shard.file_info_blocks.is_empty(),
        "expected at least one file info block"
    );

    // Verify the file hash matches our expected file
    let expected_file_hash = fs::read_to_string(test_data_path("file_hash.txt"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        shard.file_info_blocks[0].header.file_hash.to_hex(),
        expected_file_hash,
        "shard file hash mismatch"
    );

    // Should have CAS info blocks (xorb metadata)
    assert!(
        !shard.cas_info_blocks.is_empty(),
        "expected at least one CAS info block"
    );

    // Verify xorb hash in CAS info matches
    let expected_xorb_hash = fs::read_to_string(test_data_path("xorb_hash.txt"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        shard.cas_info_blocks[0].header.cas_hash.to_hex(),
        expected_xorb_hash,
        "shard CAS info xorb hash mismatch"
    );
}

#[test]
fn test_shard_deserialization_with_footer() {
    let shard_data = fs::read(test_data_path("reference_shard_full.bin")).unwrap();
    let shard = Shard::from_bytes(&shard_data).unwrap();

    assert!(shard.header.footer_size > 0);
    assert!(shard.footer.is_some());

    let footer = shard.footer.as_ref().unwrap();
    assert!(footer.file_info_offset > 0);
    assert!(footer.cas_info_offset > footer.file_info_offset);
}

#[test]
fn test_shard_deserialization_dedupe() {
    let shard_data = fs::read(test_data_path("reference_shard_dedupe.bin")).unwrap();
    let shard = Shard::from_bytes(&shard_data).unwrap();

    // Dedupe shards have empty file info and CAS info with HMAC-protected hashes
    assert!(
        shard.file_info_blocks.is_empty(),
        "dedupe shard should have no file info blocks"
    );
    assert!(
        !shard.cas_info_blocks.is_empty(),
        "dedupe shard should have CAS info blocks"
    );

    // Should have footer with HMAC key
    assert!(shard.footer.is_some());
    let footer = shard.footer.as_ref().unwrap();
    assert!(
        footer.has_hmac_key(),
        "dedupe shard should have HMAC key set"
    );
}
