use std::io::{self, Read, Write};

use openxet_hashing::MerkleHash;

use crate::chunk::{ChunkError, ChunkHeader, CompressionType, compress_chunk, decompress_chunk};

/// Maximum serialized xorb size: 64 MiB.
pub const MAX_XORB_SIZE: usize = 64 * 1024 * 1024;

/// Soft limit for incremental xorb building. Once the accumulated buffer
/// exceeds this threshold, a new xorb should be started.  The 4 MiB
/// headroom accommodates the last chunk added (max 128 KiB uncompressed)
/// plus frame overhead.
pub const XORB_SOFT_LIMIT: usize = 60 * 1024 * 1024;

/// A deserialized chunk from a xorb with its raw (decompressed) data.
#[derive(Debug, Clone)]
pub struct XorbChunk {
    pub data: Vec<u8>,
}

/// Deserialize a xorb from its binary format, returning all chunks.
///
/// The xorb format is a sequence of `[ChunkHeader (8 bytes)][CompressedData]` pairs.
pub fn deserialize_xorb(data: &[u8]) -> Result<Vec<XorbChunk>, XorbError> {
    let mut reader = io::Cursor::new(data);
    let mut chunks = Vec::new();

    while (reader.position() as usize) < data.len() {
        let remaining = data.len() - reader.position() as usize;
        if remaining < ChunkHeader::SIZE {
            break; // Trailing bytes after last chunk (possible padding)
        }

        let mut header_bytes = [0u8; ChunkHeader::SIZE];
        reader.read_exact(&mut header_bytes)?;

        // An invalid header version signals end-of-chunks (e.g. xet-core's
        // CasObjectInfoV1 footer starts with "XETBLOB" whose first byte 0x58
        // is not a valid chunk version).
        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };

        let compressed_size = header.compressed_size as usize;
        let remaining = data.len() - reader.position() as usize;
        if remaining < compressed_size {
            return Err(XorbError::Chunk(ChunkError::UnexpectedEof));
        }

        let mut compressed = vec![0u8; compressed_size];
        reader.read_exact(&mut compressed)?;

        let decompressed = decompress_chunk(
            &compressed,
            header.compression_type,
            header.uncompressed_size as usize,
        )?;

        if decompressed.len() != header.uncompressed_size as usize {
            return Err(XorbError::SizeMismatch {
                expected: header.uncompressed_size as usize,
                actual: decompressed.len(),
            });
        }

        chunks.push(XorbChunk { data: decompressed });
    }

    Ok(chunks)
}

/// Deserialize only a range of chunks from a xorb.
///
/// `range` is a half-open interval `[start, end)` of chunk indices.
/// Chunks before `start` are skipped (headers parsed but data not decompressed).
/// Reading stops after chunk at index `end - 1`.
pub fn deserialize_xorb_range(
    data: &[u8],
    start: usize,
    end: usize,
) -> Result<Vec<XorbChunk>, XorbError> {
    let mut reader = io::Cursor::new(data);
    let mut chunks = Vec::with_capacity(end - start);
    let mut index = 0;

    while (reader.position() as usize) < data.len() && index < end {
        let remaining = data.len() - reader.position() as usize;
        if remaining < ChunkHeader::SIZE {
            break;
        }

        let mut header_bytes = [0u8; ChunkHeader::SIZE];
        reader.read_exact(&mut header_bytes)?;

        let Ok(header) = ChunkHeader::from_bytes(&header_bytes) else {
            break;
        };

        let compressed_size = header.compressed_size as usize;
        let remaining = data.len() - reader.position() as usize;
        if remaining < compressed_size {
            return Err(XorbError::Chunk(ChunkError::UnexpectedEof));
        }

        if index >= start {
            let mut compressed = vec![0u8; compressed_size];
            reader.read_exact(&mut compressed)?;

            let decompressed = decompress_chunk(
                &compressed,
                header.compression_type,
                header.uncompressed_size as usize,
            )?;

            chunks.push(XorbChunk { data: decompressed });
        } else {
            // Skip this chunk's data without decompressing
            reader.set_position(reader.position() + compressed_size as u64);
        }

        index += 1;
    }

    Ok(chunks)
}

/// Serialize a single chunk into xorb wire format (header + compressed data).
///
/// Returns the raw bytes that would be appended to a xorb buffer.
pub fn serialize_single_chunk(
    chunk_data: &[u8],
    compression: CompressionType,
) -> Result<Vec<u8>, XorbError> {
    let compressed = compress_chunk(chunk_data, compression);

    let (final_data, final_compression) = if compressed.len() >= chunk_data.len() {
        (chunk_data.to_vec(), CompressionType::None)
    } else {
        (compressed, compression)
    };

    let header = ChunkHeader {
        version: 0,
        compressed_size: final_data.len() as u32,
        compression_type: final_compression,
        uncompressed_size: chunk_data.len() as u32,
    };

    let mut buf = Vec::with_capacity(ChunkHeader::SIZE + final_data.len());
    buf.write_all(&header.to_bytes())?;
    buf.write_all(&final_data)?;
    Ok(buf)
}

/// Serialize chunks into xorb binary format.
///
/// Each chunk is compressed with the given compression type and preceded by its header.
pub fn serialize_xorb(
    chunks: &[&[u8]],
    compression: CompressionType,
) -> Result<Vec<u8>, XorbError> {
    let mut buffer = Vec::new();

    for chunk_data in chunks {
        let compressed = compress_chunk(chunk_data, compression);

        // If compression made it bigger, store uncompressed
        let (final_data, final_compression) = if compressed.len() >= chunk_data.len() {
            (chunk_data.to_vec(), CompressionType::None)
        } else {
            (compressed, compression)
        };

        let header = ChunkHeader {
            version: 0,
            compressed_size: final_data.len() as u32,
            compression_type: final_compression,
            uncompressed_size: chunk_data.len() as u32,
        };

        buffer.write_all(&header.to_bytes())?;
        buffer.write_all(&final_data)?;
    }

    if buffer.len() > MAX_XORB_SIZE {
        return Err(XorbError::TooLarge(buffer.len()));
    }

    Ok(buffer)
}

/// Compute xorb hash from chunk hashes and sizes.
pub fn compute_xorb_hash(chunk_hashes_and_sizes: &[(MerkleHash, usize)]) -> MerkleHash {
    openxet_hashing::compute_merkle_root(chunk_hashes_and_sizes)
}

#[derive(Debug, thiserror::Error)]
pub enum XorbError {
    #[error("chunk error: {0}")]
    Chunk(#[from] ChunkError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("decompressed size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: usize, actual: usize },
    #[error("xorb too large: {0} bytes (max {MAX_XORB_SIZE})")]
    TooLarge(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xorb_roundtrip_no_compression() {
        let chunks: Vec<Vec<u8>> = vec![
            b"hello world".to_vec(),
            b"foo bar baz".to_vec(),
            vec![42u8; 1000],
        ];
        let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();

        let serialized = serialize_xorb(&chunk_refs, CompressionType::None).unwrap();
        let deserialized = deserialize_xorb(&serialized).unwrap();

        assert_eq!(deserialized.len(), 3);
        assert_eq!(deserialized[0].data, b"hello world");
        assert_eq!(deserialized[1].data, b"foo bar baz");
        assert_eq!(deserialized[2].data, vec![42u8; 1000]);
    }

    #[test]
    fn test_xorb_roundtrip_lz4() {
        let chunks: Vec<Vec<u8>> = vec![vec![0u8; 10_000], vec![1u8; 20_000]];
        let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();

        let serialized = serialize_xorb(&chunk_refs, CompressionType::Lz4).unwrap();
        let deserialized = deserialize_xorb(&serialized).unwrap();

        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized[0].data, vec![0u8; 10_000]);
        assert_eq!(deserialized[1].data, vec![1u8; 20_000]);
    }

    #[test]
    fn test_xorb_range_deserialization() {
        let chunks: Vec<Vec<u8>> = (0..10).map(|i| vec![i as u8; 1000]).collect();
        let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();

        let serialized = serialize_xorb(&chunk_refs, CompressionType::None).unwrap();

        // Get chunks [3, 7)
        let range_chunks = deserialize_xorb_range(&serialized, 3, 7).unwrap();
        assert_eq!(range_chunks.len(), 4);
        assert_eq!(range_chunks[0].data, vec![3u8; 1000]);
        assert_eq!(range_chunks[3].data, vec![6u8; 1000]);
    }

    #[test]
    fn test_xorb_too_large() {
        // Create chunks that exceed 64 MiB total
        let big_chunk = vec![0u8; MAX_XORB_SIZE + 1];
        let result = serialize_xorb(&[&big_chunk], CompressionType::None);
        assert!(result.is_err());
    }
}
