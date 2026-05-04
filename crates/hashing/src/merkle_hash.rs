use std::fmt;

use serde::{Deserialize, Serialize};

/// A 32-byte hash used throughout the Xet protocol.
///
/// The Xet protocol uses a specific hex encoding: each 8-byte segment is treated
/// as a little-endian u64, with bytes reversed within each segment before hex encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MerkleHash(pub [u8; 32]);

impl MerkleHash {
    pub const ZERO: Self = Self([0u8; 32]);
    pub const MAX: Self = Self([0xFF; 32]);

    /// Create from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Encode to 64-char hex string using Xet's LE octet reversal.
    ///
    /// For every 8-byte segment, the byte order is reversed before hex encoding.
    pub fn to_hex(&self) -> String {
        let mut reordered = [0u8; 32];
        for segment in 0..4 {
            let base = segment * 8;
            for i in 0..8 {
                reordered[base + i] = self.0[base + 7 - i];
            }
        }
        hex::encode(reordered)
    }

    /// Decode from 64-char hex string with Xet's LE octet reversal.
    pub fn from_hex(s: &str) -> Result<Self, MerkleHashError> {
        let bytes = hex::decode(s).map_err(|_| MerkleHashError::InvalidHex)?;
        if bytes.len() != 32 {
            return Err(MerkleHashError::InvalidLength(bytes.len()));
        }

        let mut hash = [0u8; 32];
        for segment in 0..4 {
            let base = segment * 8;
            for i in 0..8 {
                hash[base + i] = bytes[base + 7 - i];
            }
        }
        Ok(Self(hash))
    }
}

impl fmt::Debug for MerkleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MerkleHash({})", self.to_hex())
    }
}

impl fmt::Display for MerkleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MerkleHashError {
    #[error("invalid hex string")]
    InvalidHex,
    #[error("invalid hash length: expected 32 bytes, got {0}")]
    InvalidLength(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_roundtrip() {
        let hash = MerkleHash::from_bytes([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ]);

        let hex_str = hash.to_hex();
        // Per the spec example: bytes [0..8] reversed = [7,6,5,4,3,2,1,0]
        assert_eq!(
            hex_str,
            "07060504030201000f0e0d0c0b0a09081716151413121110\
             1f1e1d1c1b1a1918"
        );

        let decoded = MerkleHash::from_hex(&hex_str).unwrap();
        assert_eq!(hash, decoded);
    }

    #[test]
    fn test_zero_hash() {
        let hex_str = MerkleHash::ZERO.to_hex();
        assert_eq!(hex_str, "0".repeat(64));
    }

    #[test]
    fn test_invalid_hex() {
        assert!(MerkleHash::from_hex("not_hex").is_err());
        assert!(MerkleHash::from_hex("0011").is_err()); // too short
    }
}
