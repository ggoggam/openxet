use crate::MerkleHash;

/// Blake3 key for term verification hashing.
const VERIFICATION_KEY: [u8; 32] = [
    127, 24, 87, 214, 206, 86, 237, 102, 18, 127, 249, 19, 231, 165, 195, 243, 164, 205, 38, 213,
    181, 219, 73, 230, 65, 36, 152, 127, 40, 251, 148, 195,
];

/// Compute the term verification hash from a slice of chunk hashes.
///
/// Takes the chunk hashes for a specific range of chunks in a term,
/// concatenates their raw 32-byte representations, and computes a
/// blake3 keyed hash with VERIFICATION_KEY.
pub fn compute_verification_hash(chunk_hashes: &[MerkleHash]) -> MerkleHash {
    let mut buffer = Vec::with_capacity(chunk_hashes.len() * 32);
    for hash in chunk_hashes {
        buffer.extend_from_slice(hash.as_bytes());
    }
    let hash = blake3::keyed_hash(&VERIFICATION_KEY, &buffer);
    MerkleHash::from_bytes(*hash.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verification_hash_deterministic() {
        let hashes = vec![
            MerkleHash::from_bytes([1u8; 32]),
            MerkleHash::from_bytes([2u8; 32]),
        ];
        let h1 = compute_verification_hash(&hashes);
        let h2 = compute_verification_hash(&hashes);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_verification_hash_order_matters() {
        let a = MerkleHash::from_bytes([1u8; 32]);
        let b = MerkleHash::from_bytes([2u8; 32]);

        let h1 = compute_verification_hash(&[a, b]);
        let h2 = compute_verification_hash(&[b, a]);
        assert_ne!(h1, h2);
    }
}
