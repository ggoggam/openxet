use crate::MerkleHash;
use crate::merkle_tree::compute_merkle_root;

/// Blake3 key for file hashing: 32 zero bytes.
const FILE_HASH_KEY: [u8; 32] = [0u8; 32];

/// Compute the file hash from a list of (chunk_hash, chunk_size) pairs.
///
/// This follows the same merkle tree procedure as xorb hashing, then takes
/// the resulting root hash and computes a blake3 keyed hash with a zero key.
pub fn compute_file_hash(chunks: &[(MerkleHash, usize)]) -> MerkleHash {
    let merkle_root = compute_merkle_root(chunks);
    let hash = blake3::keyed_hash(&FILE_HASH_KEY, merkle_root.as_bytes());
    MerkleHash::from_bytes(*hash.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_hash_differs_from_merkle_root() {
        let chunks = vec![(MerkleHash::from_bytes([1u8; 32]), 100)];
        let merkle_root = compute_merkle_root(&chunks);
        let file_hash = compute_file_hash(&chunks);
        // File hash wraps merkle root with zero-key blake3, so they differ
        assert_ne!(merkle_root, file_hash);
    }
}
