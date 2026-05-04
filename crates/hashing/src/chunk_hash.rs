use crate::MerkleHash;

/// Blake3 key for chunk hashing.
const DATA_KEY: [u8; 32] = [
    102, 151, 245, 119, 91, 149, 80, 222, 49, 53, 203, 172, 165, 151, 24, 28, 157, 228, 33, 16,
    155, 235, 43, 88, 180, 208, 176, 75, 147, 173, 242, 41,
];

/// Compute the hash of a chunk using blake3 keyed hash with DATA_KEY.
pub fn compute_chunk_hash(data: &[u8]) -> MerkleHash {
    let hash = blake3::keyed_hash(&DATA_KEY, data);
    MerkleHash::from_bytes(*hash.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_hash_deterministic() {
        let data = b"hello world";
        let h1 = compute_chunk_hash(data);
        let h2 = compute_chunk_hash(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_chunk_hash_different_data() {
        let h1 = compute_chunk_hash(b"foo");
        let h2 = compute_chunk_hash(b"bar");
        assert_ne!(h1, h2);
    }
}
