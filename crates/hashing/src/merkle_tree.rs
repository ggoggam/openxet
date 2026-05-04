use crate::MerkleHash;

/// Blake3 key for merkle tree internal node hashing.
const INTERNAL_NODE_KEY: [u8; 32] = [
    1, 126, 197, 199, 165, 71, 41, 150, 253, 148, 102, 102, 180, 138, 2, 230, 93, 221, 83, 111, 55,
    199, 109, 210, 248, 99, 82, 230, 74, 83, 113, 63,
];

/// Mean branching factor for the aggregated merkle tree.
/// Matches xet-core's AGGREGATED_HASHES_MEAN_TREE_BRANCHING_FACTOR.
const BRANCHING_FACTOR: u64 = 4;

/// Compute the merkle root hash from a list of (chunk_hash, chunk_size) pairs.
///
/// Uses the xet-core variable-branching aggregation scheme:
/// - Groups are determined by examining hash values (hash % BRANCHING_FACTOR == 0)
/// - Each group of 2-8 entries is merged into a single (hash, total_size) pair
/// - Process repeats until a single root remains
pub fn compute_merkle_root(chunks: &[(MerkleHash, usize)]) -> MerkleHash {
    assert!(
        !chunks.is_empty(),
        "cannot compute merkle root of empty list"
    );

    // Convert to (MerkleHash, u64) for internal processing
    let mut hv: Vec<(MerkleHash, u64)> = chunks.iter().map(|(h, s)| (*h, *s as u64)).collect();

    while hv.len() > 1 {
        let mut write_idx = 0;
        let mut read_idx = 0;

        while read_idx != hv.len() {
            let next_cut = read_idx + next_merge_cut(&hv[read_idx..]);
            hv[write_idx] = merged_hash_of_sequence(&hv[read_idx..next_cut]);
            write_idx += 1;
            read_idx = next_cut;
        }

        hv.truncate(write_idx);
    }

    hv[0].0
}

/// Determine the next merge cut point in the hash sequence.
///
/// Scans from index 2 up to min(2*BRANCHING_FACTOR+1, len) looking for a hash
/// where hash % BRANCHING_FACTOR == 0. Returns the position after that hash
/// (so the group includes it). If no such hash is found, returns the max allowed.
fn next_merge_cut(hashes: &[(MerkleHash, u64)]) -> usize {
    if hashes.len() <= 2 {
        return hashes.len();
    }

    let end = ((2 * BRANCHING_FACTOR as usize) + 1).min(hashes.len());

    for (i, (h, _)) in hashes.iter().enumerate().take(end).skip(2) {
        if hash_mod(h, BRANCHING_FACTOR) == 0 {
            return i + 1;
        }
    }

    end
}

/// Compute hash % modulus using the last 8 bytes of the hash as a little-endian u64.
/// Matches xet-core's MerkleHash Rem<u64> implementation.
fn hash_mod(hash: &MerkleHash, modulus: u64) -> u64 {
    let bytes = hash.as_bytes();
    let last_u64 = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    last_u64 % modulus
}

/// Merge a sequence of (hash, size) pairs into a single (hash, total_size).
///
/// Formats each entry as `"{hash_hex} : {size}\n"`, concatenates, and
/// computes blake3 keyed hash with INTERNAL_NODE_KEY.
fn merged_hash_of_sequence(entries: &[(MerkleHash, u64)]) -> (MerkleHash, u64) {
    let mut buffer = String::new();
    let mut total_len: u64 = 0;

    for (hash, size) in entries {
        buffer.push_str(&hash.to_hex());
        buffer.push_str(" : ");
        buffer.push_str(&size.to_string());
        buffer.push('\n');
        total_len += *size;
    }

    let hash = blake3::keyed_hash(&INTERNAL_NODE_KEY, buffer.as_bytes());
    (MerkleHash::from_bytes(*hash.as_bytes()), total_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merkle_root_single_chunk() {
        let chunk_hash = MerkleHash::from_bytes([42u8; 32]);
        let root = compute_merkle_root(&[(chunk_hash, 1000)]);
        // Single entry: merged_hash_of_sequence of one entry
        assert_ne!(root, MerkleHash::ZERO);
    }

    #[test]
    fn test_merkle_root_deterministic() {
        let chunks = vec![
            (MerkleHash::from_bytes([1u8; 32]), 100),
            (MerkleHash::from_bytes([2u8; 32]), 200),
        ];
        let r1 = compute_merkle_root(&chunks);
        let r2 = compute_merkle_root(&chunks);
        assert_eq!(r1, r2);
    }

    #[test]
    fn test_merkle_root_order_matters() {
        let a = (MerkleHash::from_bytes([1u8; 32]), 100);
        let b = (MerkleHash::from_bytes([2u8; 32]), 200);

        let r1 = compute_merkle_root(&[a, b]);
        let r2 = compute_merkle_root(&[b, a]);
        assert_ne!(r1, r2);
    }

    #[test]
    fn test_next_merge_cut_small() {
        // <= 2 entries: returns len
        let entries = vec![(MerkleHash::ZERO, 0u64)];
        assert_eq!(next_merge_cut(&entries), 1);

        let entries = vec![(MerkleHash::ZERO, 0), (MerkleHash::ZERO, 0)];
        assert_eq!(next_merge_cut(&entries), 2);
    }

    #[test]
    fn test_next_merge_cut_max_9() {
        // With 100 entries where none trigger cut, should cap at 2*4+1 = 9
        let entries: Vec<(MerkleHash, u64)> = (0..100)
            .map(|_| {
                // Create hash where last 8 bytes % 4 != 0
                let mut bytes = [0u8; 32];
                bytes[24] = 1; // makes last u64 = 1, 1 % 4 = 1
                (MerkleHash::from_bytes(bytes), 100)
            })
            .collect();
        assert_eq!(next_merge_cut(&entries), 9);
    }
}
