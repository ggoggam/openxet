//! Server-side xorb metadata extraction on top of `xet-core-structures`.
//!
//! Real xet-core clients upload xorbs with a trailing `XorbObjectInfoV1`
//! footer, but the spec only requires chunk frames — reference xorbs exist
//! whose tail is an opaque marker, not a parseable footer. The policy here:
//!
//! - a tail that parses as a footer must FULLY validate (chunk hashes and
//!   offsets recomputed from the data), or the xorb is rejected — a stored
//!   footer is trusted by later reads, so a bad one must never be stored;
//! - any other tail is opaque: chunk metadata is rebuilt by walking and
//!   decompressing the frames, and the tail is stored but never interpreted.

use std::io::Cursor;

use xet_core_structures::merklehash::{MerkleHash, compute_data_hash, xorb_hash};
use xet_core_structures::xorb_object::{
    XORB_CHUNK_HEADER_LENGTH, XorbObject, XorbObjectInfoV1, parse_chunk_header,
};

use crate::error::AppError;
use crate::storage::index::{XorbChunk, XorbLayout};

/// True when the info carries a complete set of unpacked chunk offsets
/// (absent on legacy v0 footers, which predate them).
fn has_unpacked_offsets(info: &XorbObjectInfoV1) -> bool {
    info.unpacked_chunk_offsets.len() == info.num_chunks as usize
}

/// Walk the chunk frames of a serialized xorb, decompressing each chunk and
/// recomputing its hash, and rebuild the metadata the footer would carry.
/// Stops at the first position that does not parse as a chunk header (the
/// opaque tail); corrupt chunk *data* is an error.
fn walk_chunk_frames(body: &[u8]) -> Result<XorbObjectInfoV1, String> {
    let mut info = XorbObjectInfoV1::default();
    let mut pos = 0usize;
    let mut unpacked = 0u32;

    while pos + XORB_CHUNK_HEADER_LENGTH <= body.len() {
        let header_bytes: [u8; XORB_CHUNK_HEADER_LENGTH] = body
            [pos..pos + XORB_CHUNK_HEADER_LENGTH]
            .try_into()
            .unwrap();
        let Ok(header) = parse_chunk_header(header_bytes) else {
            break; // tail (footer, marker, or padding) — not ours to interpret
        };

        let data_start = pos + XORB_CHUNK_HEADER_LENGTH;
        let Some(data_end) = data_start
            .checked_add(header.get_compressed_length() as usize)
            .filter(|&end| end <= body.len())
        else {
            break;
        };

        let scheme = header
            .get_compression_scheme()
            .map_err(|e| format!("chunk {}: {e}", info.num_chunks))?;
        let data = scheme
            .decompress_from_slice(&body[data_start..data_end])
            .map_err(|e| format!("chunk {} failed to decompress: {e}", info.num_chunks))?;

        if data.len() != header.get_uncompressed_length() as usize {
            return Err(format!(
                "chunk {} decompressed to {} bytes, header claims {}",
                info.num_chunks,
                data.len(),
                header.get_uncompressed_length()
            ));
        }

        unpacked += data.len() as u32;
        info.chunk_hashes.push(compute_data_hash(&data));
        info.chunk_boundary_offsets.push(data_end as u32);
        info.unpacked_chunk_offsets.push(unpacked);
        info.num_chunks += 1;
        pos = data_end;
    }

    Ok(info)
}

/// The aggregated xorb hash for walked/parsed metadata.
fn xorb_hash_from_info(info: &XorbObjectInfoV1) -> MerkleHash {
    let mut prev = 0u32;
    let hashes_and_sizes: Vec<(MerkleHash, u64)> = info
        .chunk_hashes
        .iter()
        .zip(&info.unpacked_chunk_offsets)
        .map(|(h, &end)| {
            let size = (end - prev) as u64;
            prev = end;
            (*h, size)
        })
        .collect();
    xorb_hash(&hashes_and_sizes)
}

/// Validate an uploaded xorb body against its declared hash and return its
/// chunk metadata. Every chunk hash is recomputed from the (decompressed)
/// data on both paths — the core CAS invariant.
pub(crate) fn validated_xorb_info(
    body: &[u8],
    expected: &MerkleHash,
) -> Result<XorbObjectInfoV1, AppError> {
    if XorbObject::deserialize(&mut Cursor::new(body)).is_ok() {
        // Parseable footer: it must fully validate or the upload is refused.
        let validated = XorbObject::validate_xorb_object(&mut Cursor::new(body), expected)
            .map_err(|e| AppError::BadRequest(format!("invalid xorb: {e}")))?;
        match validated {
            Some(xorb) if has_unpacked_offsets(&xorb.info) => Ok(xorb.info),
            // Valid legacy (v0) footer without unpacked offsets: rebuild the
            // full metadata from the already-verified chunk frames.
            Some(_) => walk_chunk_frames(body)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("rebuilding xorb metadata: {e}"))),
            None => Err(AppError::BadRequest(
                "xorb footer failed validation".to_string(),
            )),
        }
    } else {
        // No parseable footer: verify the chunk frames directly.
        let info = walk_chunk_frames(body)
            .map_err(|e| AppError::BadRequest(format!("invalid xorb: {e}")))?;

        if info.num_chunks == 0 {
            return Err(AppError::BadRequest("xorb contains no chunks".to_string()));
        }

        let computed = xorb_hash_from_info(&info);
        if computed != *expected {
            return Err(AppError::BadRequest(format!(
                "xorb hash mismatch: URL={}, computed={}",
                expected.hex(),
                computed.hex()
            )));
        }

        Ok(info)
    }
}

/// Parse the metadata of an already-stored (trusted) xorb: read the footer
/// when present (footers only get stored after full validation), otherwise
/// rebuild from the chunk frames.
pub(crate) fn xorb_info_from_stored(data: &[u8]) -> Result<XorbObjectInfoV1, AppError> {
    if let Ok(xorb) = XorbObject::deserialize(&mut Cursor::new(data))
        && has_unpacked_offsets(&xorb.info)
    {
        return Ok(xorb.info);
    }

    walk_chunk_frames(data)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("corrupt stored xorb: {e}")))
}

/// Build the index [`XorbLayout`] from parsed xorb metadata. Offsets are
/// cumulative ends, so per-chunk sizes are their differences; the physical
/// span includes each chunk's 8-byte header, which the layout's
/// `compressed_size` excludes.
pub(crate) fn layout_from_info(info: &XorbObjectInfoV1, num_bytes_on_disk: u32) -> XorbLayout {
    let mut chunks = Vec::with_capacity(info.num_chunks as usize);
    let mut prev_physical = 0u32;
    let mut prev_unpacked = 0u32;

    for i in 0..info.num_chunks as usize {
        let physical_end = info.chunk_boundary_offsets[i];
        let unpacked_end = info.unpacked_chunk_offsets[i];
        chunks.push(XorbChunk {
            chunk_hash: info.chunk_hashes[i].hex(),
            unpacked_size: unpacked_end - prev_unpacked,
            compressed_size: physical_end - prev_physical - XORB_CHUNK_HEADER_LENGTH as u32,
        });
        prev_physical = physical_end;
        prev_unpacked = unpacked_end;
    }

    XorbLayout {
        num_bytes_on_disk,
        chunks,
    }
}

/// Physical `(start, end)` byte positions of each chunk frame (header +
/// compressed data) within the serialized xorb.
pub(crate) fn chunk_byte_offsets(info: &XorbObjectInfoV1) -> Vec<(u64, u64)> {
    let mut offsets = Vec::with_capacity(info.num_chunks as usize);
    let mut prev = 0u64;
    for i in 0..info.num_chunks as usize {
        let end = info.chunk_boundary_offsets[i] as u64;
        offsets.push((prev, end));
        prev = end;
    }
    offsets
}

/// Per-chunk `(hash, unpacked_start, unpacked_size)` triples, the shape the
/// dedup response builder consumes.
pub(crate) fn chunk_info_triples(info: &XorbObjectInfoV1) -> Vec<(MerkleHash, u32, u32)> {
    let mut out = Vec::with_capacity(info.num_chunks as usize);
    let mut prev = 0u32;
    for i in 0..info.num_chunks as usize {
        let end = info.unpacked_chunk_offsets[i];
        out.push((info.chunk_hashes[i], prev, end - prev));
        prev = end;
    }
    out
}
