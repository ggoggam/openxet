/// Compression type for a chunk in a xorb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionType {
    /// No compression — data stored as-is.
    None = 0,
    /// Standard LZ4 block compression.
    Lz4 = 1,
    /// Byte grouping with 4-byte groups followed by LZ4 compression.
    ByteGrouping4Lz4 = 2,
}

impl TryFrom<u8> for CompressionType {
    type Error = ChunkError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Lz4),
            2 => Ok(Self::ByteGrouping4Lz4),
            other => Err(ChunkError::UnknownCompression(other)),
        }
    }
}

/// Header for a single chunk within a xorb (8 bytes total).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    /// Protocol version (currently 0).
    pub version: u8,
    /// Size of data after compression.
    pub compressed_size: u32,
    /// Compression algorithm used.
    pub compression_type: CompressionType,
    /// Size of raw chunk data before compression.
    pub uncompressed_size: u32,
}

impl ChunkHeader {
    /// Header is always 8 bytes.
    pub const SIZE: usize = 8;

    /// Deserialize a chunk header from exactly 8 bytes.
    pub fn from_bytes(bytes: &[u8; 8]) -> Result<Self, ChunkError> {
        let version = bytes[0];
        if version != 0 {
            return Err(ChunkError::UnsupportedVersion(version));
        }

        let compressed_size =
            u32::from(bytes[1]) | (u32::from(bytes[2]) << 8) | (u32::from(bytes[3]) << 16);

        let compression_type = CompressionType::try_from(bytes[4])?;

        let uncompressed_size =
            u32::from(bytes[5]) | (u32::from(bytes[6]) << 8) | (u32::from(bytes[7]) << 16);

        Ok(Self {
            version,
            compressed_size,
            compression_type,
            uncompressed_size,
        })
    }

    /// Serialize to 8 bytes.
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0] = self.version;
        buf[1] = (self.compressed_size & 0xFF) as u8;
        buf[2] = ((self.compressed_size >> 8) & 0xFF) as u8;
        buf[3] = ((self.compressed_size >> 16) & 0xFF) as u8;
        buf[4] = self.compression_type as u8;
        buf[5] = (self.uncompressed_size & 0xFF) as u8;
        buf[6] = ((self.uncompressed_size >> 8) & 0xFF) as u8;
        buf[7] = ((self.uncompressed_size >> 16) & 0xFF) as u8;
        buf
    }
}

/// Decompress LZ4 framed data.
fn lz4_frame_decompress(compressed: &[u8]) -> Result<Vec<u8>, ChunkError> {
    use std::io::{Cursor, Read as _};
    let mut decoder = lz4_flex::frame::FrameDecoder::new(Cursor::new(compressed));
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(ChunkError::Io)?;
    Ok(decompressed)
}

/// Compress data using LZ4 framed format.
fn lz4_frame_compress(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// Decompress chunk data based on compression type.
pub fn decompress_chunk(
    compressed: &[u8],
    compression: CompressionType,
    _uncompressed_size: usize,
) -> Result<Vec<u8>, ChunkError> {
    match compression {
        CompressionType::None => Ok(compressed.to_vec()),
        CompressionType::Lz4 => lz4_frame_decompress(compressed),
        CompressionType::ByteGrouping4Lz4 => {
            let lz4_decompressed = lz4_frame_decompress(compressed)?;
            Ok(byte_ungroup4(&lz4_decompressed))
        }
    }
}

/// Compress chunk data with the given compression type (framed LZ4).
pub fn compress_chunk(data: &[u8], compression: CompressionType) -> Vec<u8> {
    match compression {
        CompressionType::None => data.to_vec(),
        CompressionType::Lz4 => lz4_frame_compress(data),
        CompressionType::ByteGrouping4Lz4 => {
            let grouped = byte_group4(data);
            lz4_frame_compress(&grouped)
        }
    }
}

/// Byte grouping phase for BG4: reorganize data by grouping bytes by their
/// position within each 4-byte group.
///
/// `[A1,A2,A3,A4, B1,B2,B3,B4, ...]` → `[A1,B1,..., A2,B2,..., A3,B3,..., A4,B4,...]`
///
/// If the total number of bytes is not a multiple of 4, remaining bytes
/// are distributed to the first 1-3 groups.
fn byte_group4(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    let full_groups = n / 4;
    let remainder = n % 4;

    // Each of the 4 groups gets `full_groups` bytes, plus the first `remainder` groups get +1
    let mut group_sizes = [full_groups; 4];
    for size in group_sizes.iter_mut().take(remainder) {
        *size += 1;
    }

    let mut groups: [Vec<u8>; 4] = [vec![], vec![], vec![], vec![]];
    for g in 0..4 {
        groups[g].reserve(group_sizes[g]);
    }

    let mut idx = 0;
    // Process full 4-byte groups
    while idx + 3 < n {
        groups[0].push(data[idx]);
        groups[1].push(data[idx + 1]);
        groups[2].push(data[idx + 2]);
        groups[3].push(data[idx + 3]);
        idx += 4;
    }

    // Process remaining bytes (0-3)
    for (g, group) in groups.iter_mut().enumerate() {
        if idx + g < n {
            group.push(data[idx + g]);
        }
    }

    let mut result = Vec::with_capacity(n);
    for group in &groups {
        result.extend_from_slice(group);
    }
    result
}

/// Reverse the byte grouping: reconstruct original data from grouped format.
fn byte_ungroup4(grouped: &[u8]) -> Vec<u8> {
    let n = grouped.len();
    let full_groups = n / 4;
    let remainder = n % 4;

    // Compute group sizes (same logic as grouping)
    let mut group_sizes = [full_groups; 4];
    for size in group_sizes.iter_mut().take(remainder) {
        *size += 1;
    }

    // Split grouped data into the 4 groups
    let mut offset = 0;
    let mut groups: [&[u8]; 4] = [&[]; 4];
    for g in 0..4 {
        groups[g] = &grouped[offset..offset + group_sizes[g]];
        offset += group_sizes[g];
    }

    // Interleave: take one byte from each group in round-robin
    let mut result = Vec::with_capacity(n);
    let max_group_size = group_sizes.iter().copied().max().unwrap_or(0);
    for i in 0..max_group_size {
        for group in &groups {
            if i < group.len() {
                result.push(group[i]);
            }
        }
    }

    result
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("unknown compression type: {0}")]
    UnknownCompression(u8),
    #[error("unsupported chunk version: {0}")]
    UnsupportedVersion(u8),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected end of data")]
    UnexpectedEof,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_header_roundtrip() {
        let header = ChunkHeader {
            version: 0,
            compressed_size: 12345,
            compression_type: CompressionType::Lz4,
            uncompressed_size: 65536,
        };
        let bytes = header.to_bytes();
        let decoded = ChunkHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_chunk_header_max_u24() {
        let header = ChunkHeader {
            version: 0,
            compressed_size: 0xFF_FFFF, // max 24-bit value
            compression_type: CompressionType::None,
            uncompressed_size: 0xFF_FFFF,
        };
        let bytes = header.to_bytes();
        let decoded = ChunkHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_byte_group_ungroup_roundtrip() {
        // Exact multiple of 4
        let data = b"ABCDEFGHIJKL";
        assert_eq!(byte_ungroup4(&byte_group4(data)), data);

        // Not a multiple of 4
        let data = b"ABCDEFGHIJK"; // 11 bytes
        assert_eq!(byte_ungroup4(&byte_group4(data)), data);

        let data = b"AB"; // 2 bytes
        assert_eq!(byte_ungroup4(&byte_group4(data)), data);

        let data = b"A"; // 1 byte
        assert_eq!(byte_ungroup4(&byte_group4(data)), data);
    }

    #[test]
    fn test_byte_group4_structure() {
        // [A1,A2,A3,A4, B1,B2,B3,B4] → [A1,B1, A2,B2, A3,B3, A4,B4]
        let data = [1, 2, 3, 4, 5, 6, 7, 8];
        let grouped = byte_group4(&data);
        assert_eq!(grouped, vec![1, 5, 2, 6, 3, 7, 4, 8]);
    }

    #[test]
    fn test_decompress_none() {
        let data = b"hello world";
        let result = decompress_chunk(data, CompressionType::None, data.len()).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn test_lz4_roundtrip() {
        let data = b"hello world hello world hello world";
        let compressed = compress_chunk(data, CompressionType::Lz4);
        let decompressed = decompress_chunk(&compressed, CompressionType::Lz4, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }
}
