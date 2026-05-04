use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A range of chunk indices: `[start, end)` (end-exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRange {
    pub start: usize,
    pub end: usize,
}

impl ChunkRange {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// Check if this range contains the other range entirely.
    pub fn contains_range(&self, other: &ChunkRange) -> bool {
        self.start <= other.start && self.end >= other.end
    }
}

/// A byte range `[start, end]` (both inclusive for HTTP Range header format).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

/// A term in a file reconstruction: specifies a xorb and a chunk range within it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CASReconstructionTerm {
    /// The xorb hash (64-character lowercase hex string).
    pub hash: String,
    /// Expected length after decompression.
    pub unpacked_length: u64,
    /// Chunk index range within the xorb (end-exclusive).
    pub range: ChunkRange,
}

/// Fetch information for downloading xorb data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CASReconstructionFetchInfo {
    /// Chunk index range this fetch info covers (end-exclusive).
    pub range: ChunkRange,
    /// Presigned URL for downloading the xorb data.
    pub url: String,
    /// Byte range for the HTTP Range header (both inclusive).
    pub url_range: ByteRange,
}

/// Full reconstruction response returned by `GET /v1/reconstructions/{file_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryReconstructionResponse {
    /// Byte offset into the first term's decoded data to skip.
    /// For full file downloads or range starting at 0, this is 0.
    pub offset_into_first_range: u64,
    /// Ordered list of reconstruction terms.
    pub terms: Vec<CASReconstructionTerm>,
    /// Map from xorb hash to fetch info entries.
    pub fetch_info: HashMap<String, Vec<CASReconstructionFetchInfo>>,
}
