//! Integration tests validating our use of the xet-core crates against the
//! official xet-spec-reference-files from HuggingFace.
//!
//! Reference: https://huggingface.co/datasets/xet-team/xet-spec-reference-files
//!
//! The formats and hashes come from the pinned `xet-core-structures` /
//! `xet-data` crates, so these tests primarily pin the *wire format* across
//! version bumps of those crates: if an upgrade changes chunking boundaries,
//! hashing, or serialization, these fail before anything ships.
//!
//! Reference files are downloaded on first use into a temp directory and
//! reused across runs. Set `OPENXET_TEST_DATA_DIR` to override the cache
//! location, or pre-populate it to run offline.
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Mutex;

use xet_core_structures::merklehash::{MerkleHash, compute_data_hash, file_hash, xorb_hash};
use xet_core_structures::metadata_shard::MDBShardInfo;
use xet_core_structures::metadata_shard::chunk_verification::range_hash_from_chunks;
use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;
use xet_core_structures::xorb_object::{
    Chunk, CompressionScheme, XORB_CHUNK_HEADER_LENGTH, parse_chunk_header,
};
use xet_data::deduplication::Chunker;

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
        "chunk1.bin" => "b10aa1dc71c61661de92280c41a188aabc47981739b785724a099945d8dc5ce4.chunk",
        "chunk2.bin" => "26255591fa803b6baf25d88c315b8a6f5153d5bcfdf18ec5ef526264e0ccc907.chunk",
        "chunk3.bin" => "099cb228194fe640e36a6c7d274ee5ed3a714ccd557a0951d9b6b43a7292b5d1.chunk",
        "ev_data.csv" => "Electric_Vehicle_Population_Data_20250917.csv",
        "chunks.txt" => "Electric_Vehicle_Population_Data_20250917.csv.chunks",
        "xorb_hash.txt" => "Electric_Vehicle_Population_Data_20250917.csv.xet-xorb-hash",
        "file_hash.txt" => "Electric_Vehicle_Population_Data_20250917.csv.xet-file-hash",
        "range_hash.txt" => {
            "eea25d6ee393ccae385820daed127b96ef0ea034dfb7cf6da3a950ce334b7632.xorb.range-hash"
        }
        "reference.xorb" => "eea25d6ee393ccae385820daed127b96ef0ea034dfb7cf6da3a950ce334b7632.xorb",
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

    // ponytail: unique tmp per process — nextest runs each test in its own
    // process, so the in-process DOWNLOAD_LOCK can't stop concurrent downloads
    // racing on a shared tmp path. Unique name → rename never collides.
    let tmp = path.with_extension(format!("download.{}", std::process::id()));
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

fn chunk_reference_csv() -> (Vec<u8>, Vec<Chunk>) {
    let csv_data = fs::read(test_data_path("ev_data.csv")).unwrap();
    let chunks = Chunker::default().next_block(&csv_data, true);
    (csv_data, chunks)
}

// ─── Chunk Hash Tests ────────────────────────────────────────────────────────

#[test]
fn test_chunk_hash_reference_chunk1() {
    let data = fs::read(test_data_path("chunk1.bin")).unwrap();
    let hash = compute_data_hash(&data);
    let expected_hex = "b10aa1dc71c61661de92280c41a188aabc47981739b785724a099945d8dc5ce4";
    assert_eq!(hash.hex(), expected_hex, "chunk1 hash mismatch");
}

#[test]
fn test_chunk_hash_reference_chunk2() {
    let data = fs::read(test_data_path("chunk2.bin")).unwrap();
    let hash = compute_data_hash(&data);
    let expected_hex = "26255591fa803b6baf25d88c315b8a6f5153d5bcfdf18ec5ef526264e0ccc907";
    assert_eq!(hash.hex(), expected_hex, "chunk2 hash mismatch");
}

#[test]
fn test_chunk_hash_reference_chunk3() {
    let data = fs::read(test_data_path("chunk3.bin")).unwrap();
    let hash = compute_data_hash(&data);
    let expected_hex = "099cb228194fe640e36a6c7d274ee5ed3a714ccd557a0951d9b6b43a7292b5d1";
    assert_eq!(hash.hex(), expected_hex, "chunk3 hash mismatch");
}

// ─── Chunking Boundary Tests ─────────────────────────────────────────────────

#[test]
fn test_chunking_boundaries_match_reference() {
    let (_, chunks) = chunk_reference_csv();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));

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
            chunk.data.len(),
            *expected_size,
            "chunk {} size mismatch: got {}, expected {}",
            i,
            chunk.data.len(),
            *expected_size
        );
    }
}

#[test]
fn test_chunk_hashes_match_reference() {
    let (_, chunks) = chunk_reference_csv();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));

    // Verify chunk hashes match
    for (i, (chunk, (expected_hash, _))) in chunks.iter().zip(ref_chunks.iter()).enumerate() {
        assert_eq!(
            chunk.hash.hex(),
            *expected_hash,
            "chunk {} hash mismatch",
            i,
        );
    }
}

// ─── Xorb Hash Test ─────────────────────────────────────────────────────────

#[test]
fn test_xorb_hash_matches_reference() {
    let (_, chunks) = chunk_reference_csv();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));
    let expected_xorb_hash = fs::read_to_string(test_data_path("xorb_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    let chunk_hashes_and_sizes: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|c| (c.hash, c.data.len() as u64))
        .collect();

    // Verify we have the right number of chunks
    assert_eq!(chunk_hashes_and_sizes.len(), ref_chunks.len());

    // Xorb hash = aggregated merkle hash of (chunk_hash, chunk_size) pairs
    let computed = xorb_hash(&chunk_hashes_and_sizes);
    assert_eq!(computed.hex(), expected_xorb_hash, "xorb hash mismatch");
}

// ─── File Hash Test ──────────────────────────────────────────────────────────

#[test]
fn test_file_hash_matches_reference() {
    let (_, chunks) = chunk_reference_csv();
    let expected_file_hash = fs::read_to_string(test_data_path("file_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    let chunk_hashes_and_sizes: Vec<(MerkleHash, u64)> = chunks
        .iter()
        .map(|c| (c.hash, c.data.len() as u64))
        .collect();

    let computed = file_hash(&chunk_hashes_and_sizes);
    assert_eq!(computed.hex(), expected_file_hash, "file hash mismatch");
}

// ─── Verification/Range Hash Test ────────────────────────────────────────────

#[test]
fn test_verification_range_hash_matches_reference() {
    let (_, chunks) = chunk_reference_csv();
    let expected_range_hash = fs::read_to_string(test_data_path("range_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    let chunk_hashes: Vec<MerkleHash> = chunks.iter().map(|c| c.hash).collect();

    // The range hash covers all chunks (single term for the entire file)
    let range_hash = range_hash_from_chunks(&chunk_hashes);
    assert_eq!(
        range_hash.hex(),
        expected_range_hash,
        "verification range hash mismatch"
    );
}

// ─── Xorb Deserialization Test ───────────────────────────────────────────────

#[test]
fn test_xorb_deserialization() {
    let xorb_data = fs::read(test_data_path("reference.xorb")).unwrap();
    let ref_chunks = parse_chunks_file(&test_data_path("chunks.txt"));
    let expected_xorb_hash = fs::read_to_string(test_data_path("xorb_hash.txt"))
        .unwrap()
        .trim()
        .to_string();

    // The reference xorb carries no parseable XorbObjectInfoV1 footer — its
    // tail is an opaque 4-byte marker — so walk the chunk frames the way a
    // footer-less server-side validation does: parse each header, decompress,
    // recompute hashes, and stop at the first non-header position.
    let mut chunk_hashes_and_sizes: Vec<(MerkleHash, u64)> = Vec::new();
    let mut has_lz4 = false;
    let mut pos = 0usize;

    while pos + XORB_CHUNK_HEADER_LENGTH <= xorb_data.len() {
        let header_bytes: [u8; XORB_CHUNK_HEADER_LENGTH] = xorb_data
            [pos..pos + XORB_CHUNK_HEADER_LENGTH]
            .try_into()
            .unwrap();
        let Ok(header) = parse_chunk_header(header_bytes) else {
            break; // opaque tail
        };

        let scheme = header.get_compression_scheme().unwrap();
        has_lz4 |= scheme == CompressionScheme::LZ4;

        let data_start = pos + XORB_CHUNK_HEADER_LENGTH;
        let data_end = data_start + header.get_compressed_length() as usize;
        let data = scheme
            .decompress_from_slice(&xorb_data[data_start..data_end])
            .unwrap();
        assert_eq!(data.len(), header.get_uncompressed_length() as usize);

        chunk_hashes_and_sizes.push((compute_data_hash(&data), data.len() as u64));
        pos = data_end;
    }

    assert_eq!(
        chunk_hashes_and_sizes.len(),
        ref_chunks.len(),
        "xorb chunk count mismatch"
    );

    // Verify each chunk hash and size against the reference list, and the
    // aggregate against the reference xorb hash.
    for (i, ((hash, size), (expected_hash, expected_size))) in chunk_hashes_and_sizes
        .iter()
        .zip(ref_chunks.iter())
        .enumerate()
    {
        assert_eq!(hash.hex(), *expected_hash, "xorb chunk {i} hash mismatch");
        assert_eq!(
            *size as usize, *expected_size,
            "xorb chunk {i} size mismatch"
        );
    }
    assert_eq!(
        xorb_hash(&chunk_hashes_and_sizes).hex(),
        expected_xorb_hash,
        "recomputed xorb hash mismatch"
    );

    // The interop claim "frame-based LZ4 matches xet-core's" rests on this
    // xet-core-produced xorb actually containing LZ4 chunks — pin that here.
    assert!(
        has_lz4,
        "reference xorb contains no LZ4 chunks; LZ4 interop is not exercised"
    );
}

// ─── Shard Deserialization Tests ─────────────────────────────────────────────

#[test]
fn test_shard_deserialization_no_footer() {
    let shard_data = fs::read(test_data_path("reference_shard_nofooter.bin")).unwrap();
    let shard =
        MDBMinimalShard::from_reader(&mut Cursor::new(&shard_data[..]), true, true).unwrap();

    // This shard is one file upload: file info + xorb info present
    assert!(shard.num_files() > 0, "expected at least one file block");
    assert!(shard.num_xorb() > 0, "expected at least one xorb block");

    // Verify the file hash matches our expected file
    let expected_file_hash = fs::read_to_string(test_data_path("file_hash.txt"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        shard.file(0).unwrap().file_hash().hex(),
        expected_file_hash,
        "shard file hash mismatch"
    );

    // Verify xorb hash in the xorb info matches
    let expected_xorb_hash = fs::read_to_string(test_data_path("xorb_hash.txt"))
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        shard.xorb(0).unwrap().xorb_hash().hex(),
        expected_xorb_hash,
        "shard xorb info hash mismatch"
    );
}

#[test]
fn test_shard_deserialization_with_footer() {
    let shard_data = fs::read(test_data_path("reference_shard_full.bin")).unwrap();
    let shard_info = MDBShardInfo::load_from_reader(&mut Cursor::new(&shard_data[..])).unwrap();

    assert!(shard_info.header.footer_size > 0);
    assert!(shard_info.metadata.file_info_offset > 0);
    assert!(shard_info.metadata.xorb_info_offset > shard_info.metadata.file_info_offset);

    // The file info section itself must hold our reference file (the footer's
    // lookup-table counts may be zero in older-generation reference shards).
    let mut reader = Cursor::new(&shard_data[..]);
    let file_infos = shard_info.read_all_file_info_sections(&mut reader).unwrap();
    let expected_file_hash = fs::read_to_string(test_data_path("file_hash.txt"))
        .unwrap()
        .trim()
        .to_string();
    assert!(
        file_infos
            .iter()
            .any(|fi| fi.metadata.file_hash.hex() == expected_file_hash),
        "reference file not found in shard file info section"
    );
}

#[test]
fn test_shard_deserialization_dedupe() {
    let shard_data = fs::read(test_data_path("reference_shard_dedupe.bin")).unwrap();
    let mut reader = Cursor::new(&shard_data[..]);
    let shard_info = MDBShardInfo::load_from_reader(&mut reader).unwrap();

    // Dedupe shards carry an HMAC key and xorb info with keyed chunk hashes
    assert!(
        shard_info.chunk_hashes_protected(),
        "dedupe shard should have HMAC key set"
    );

    let xorb_blocks = shard_info.read_all_xorb_blocks_full(&mut reader).unwrap();
    assert!(
        !xorb_blocks.is_empty(),
        "dedupe shard should have xorb info blocks"
    );
}
