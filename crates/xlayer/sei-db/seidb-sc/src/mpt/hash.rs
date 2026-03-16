use alloy_primitives::{keccak256, B256};

/// Compute keccak256 hash of RLP-encoded node bytes.
pub fn hash_rlp(rlp_bytes: &[u8]) -> B256 {
    keccak256(rlp_bytes)
}

/// Compute trie root hash.
/// Empty trie returns EMPTY_ROOT_HASH.
/// Non-empty trie returns keccak256(root_node_rlp) — root is always hashed regardless of size.
pub fn root_hash(root_rlp: Option<&[u8]>) -> B256 {
    match root_rlp {
        None => alloy_trie::EMPTY_ROOT_HASH,
        Some(rlp) => keccak256(rlp),
    }
}

/// Whether a node should be inlined into its parent (rather than referenced by hash).
/// Rule: RLP length < 32 → inline, >= 32 → hash reference.
pub fn should_inline(rlp_bytes: &[u8]) -> bool {
    rlp_bytes.len() < 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t3_1_root_hash_empty() {
        assert_eq!(root_hash(None), alloy_trie::EMPTY_ROOT_HASH);
    }

    #[test]
    fn t3_2_hash_rlp_keccak() {
        let data = b"hello world";
        assert_eq!(hash_rlp(data), keccak256(data));
    }

    #[test]
    fn t3_3_should_inline_short() {
        assert!(should_inline(&[0u8; 31]));
    }

    #[test]
    fn t3_4_should_inline_long() {
        assert!(!should_inline(&[0u8; 32]));
    }

    #[test]
    fn t3_5_empty_root_hash_is_keccak_of_0x80() {
        // EMPTY_ROOT_HASH == keccak256(0x80)  (RLP of empty string)
        assert_eq!(alloy_trie::EMPTY_ROOT_HASH, keccak256(&[0x80]));
    }

    #[test]
    fn t3_6_root_hash_short_rlp_still_hashed() {
        // Even a very short RLP (< 32 bytes) is hashed for root
        let short = &[0xc1, 0x80]; // some small RLP
        assert_eq!(root_hash(Some(short)), keccak256(short));
    }
}
