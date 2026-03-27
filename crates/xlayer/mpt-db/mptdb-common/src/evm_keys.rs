//! EVM key parsing and construction for the mpt-db keyspace.
//!
//! These are immutable on-disk format markers; changing them would break
//! all existing state.

/// Length of an Ethereum address in bytes.
pub const ADDRESS_LEN: usize = 20;

/// Length of a storage slot in bytes.
pub const SLOT_LEN: usize = 32;

/// Prefix byte for storage state keys (address || slot).
pub const STATE_KEY_PREFIX: u8 = 0x03;

/// Prefix byte for contract code keys.
pub const CODE_KEY_PREFIX: u8 = 0x07;

/// Prefix byte for code-hash keys.
pub const CODE_HASH_KEY_PREFIX: u8 = 0x08;

/// Prefix byte for code-size keys (treated as legacy).
pub const CODE_SIZE_KEY_PREFIX: u8 = 0x09;

/// Prefix byte for nonce keys.
pub const NONCE_KEY_PREFIX: u8 = 0x0a;

/// Prefix byte for combined account keys (nonce + balance + code_hash).
///
/// Replaces the separate Nonce (0x0a) and CodeHash (0x08) sub-DBs for reth
/// integration: reth's `basic_account` returns all three fields in one call,
/// so merging them eliminates a read IO.  Value layout (72 bytes, fixed):
///   [0..8]   nonce:     u64 big-endian
///   [8..40]  balance:   U256 big-endian (32 bytes)
///   [40..72] code_hash: B256 (32 bytes)
pub const ACCOUNT_KEY_PREFIX: u8 = 0x0b;

/// Identifies the family of an EVM key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvmKeyKind {
    /// Returned only for zero-length keys.
    Empty = 0,
    /// Stripped key: 20-byte address.
    Nonce = 1,
    /// Stripped key: 20-byte address.
    CodeHash = 2,
    /// Stripped key: 20-byte address.
    Code = 3,
    /// Stripped key: addr || slot (20 + 32 bytes).
    Storage = 4,
    /// Full original key preserved (address mappings, codesize, etc.).
    Legacy = 5,
    /// Stripped key: 20-byte address.  Value: nonce(8) + balance(32) + code_hash(32).
    /// Added for reth integration; supersedes Nonce and CodeHash sub-DBs.
    Account = 6,
}

/// Parses an EVM key and returns its kind and the stripped key bytes.
///
/// For optimised keys (nonce, code, codehash, storage), the returned slice is
/// the key with its one-byte prefix stripped. For legacy keys the full original
/// key is returned. Zero-length input yields `(Empty, &[])`.
pub fn parse_evm_key(key: &[u8]) -> (EvmKeyKind, &[u8]) {
    if key.is_empty() {
        return (EvmKeyKind::Empty, &[]);
    }

    match key[0] {
        NONCE_KEY_PREFIX => {
            if key.len() != 1 + ADDRESS_LEN {
                return (EvmKeyKind::Legacy, key);
            }
            (EvmKeyKind::Nonce, &key[1..])
        }
        CODE_HASH_KEY_PREFIX => {
            if key.len() != 1 + ADDRESS_LEN {
                return (EvmKeyKind::Legacy, key);
            }
            (EvmKeyKind::CodeHash, &key[1..])
        }
        CODE_KEY_PREFIX => {
            if key.len() != 1 + ADDRESS_LEN {
                return (EvmKeyKind::Legacy, key);
            }
            (EvmKeyKind::Code, &key[1..])
        }
        STATE_KEY_PREFIX => {
            if key.len() != 1 + ADDRESS_LEN + SLOT_LEN {
                return (EvmKeyKind::Legacy, key);
            }
            (EvmKeyKind::Storage, &key[1..])
        }
        ACCOUNT_KEY_PREFIX => {
            if key.len() != 1 + ADDRESS_LEN {
                return (EvmKeyKind::Legacy, key);
            }
            (EvmKeyKind::Account, &key[1..])
        }
        // All other EVM keys go to legacy store (address mappings, codesize, etc.)
        _ => (EvmKeyKind::Legacy, key),
    }
}

/// Returns the expected internal (stripped) key length for a given kind.
pub fn internal_key_len(kind: EvmKeyKind) -> usize {
    match kind {
        EvmKeyKind::Storage => ADDRESS_LEN + SLOT_LEN, // 52
        EvmKeyKind::Nonce | EvmKeyKind::CodeHash | EvmKeyKind::Code | EvmKeyKind::Account => {
            ADDRESS_LEN // 20
        }
        _ => 0,
    }
}

/// Builds a raw Account key (with prefix) from a 20-byte address.
pub fn make_account_key(address: &[u8; ADDRESS_LEN]) -> [u8; 1 + ADDRESS_LEN] {
    let mut key = [0u8; 1 + ADDRESS_LEN];
    key[0] = ACCOUNT_KEY_PREFIX;
    key[1..].copy_from_slice(address);
    key
}

/// Builds a raw Storage key (with prefix) from a 20-byte address and 32-byte slot.
pub fn make_storage_key(
    address: &[u8; ADDRESS_LEN],
    slot: &[u8; SLOT_LEN],
) -> [u8; 1 + ADDRESS_LEN + SLOT_LEN] {
    let mut key = [0u8; 1 + ADDRESS_LEN + SLOT_LEN];
    key[0] = STATE_KEY_PREFIX;
    key[1..1 + ADDRESS_LEN].copy_from_slice(address);
    key[1 + ADDRESS_LEN..].copy_from_slice(slot);
    key
}

/// Returns the storage state key prefix (`&[0x03]`).
pub fn state_key_prefix() -> &'static [u8] {
    &[STATE_KEY_PREFIX]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_addr() -> [u8; ADDRESS_LEN] {
        let mut addr = [0u8; ADDRESS_LEN];
        for (i, b) in addr.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        addr
    }

    fn make_slot() -> [u8; SLOT_LEN] {
        let mut slot = [0u8; SLOT_LEN];
        for (i, b) in slot.iter_mut().enumerate() {
            *b = (0xa0 + i) as u8;
        }
        slot
    }

    #[test]
    fn test_parse_nonce_key() {
        let addr = make_addr();
        let mut key = vec![NONCE_KEY_PREFIX];
        key.extend_from_slice(&addr);

        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::Nonce);
        assert_eq!(stripped, &addr);
    }

    #[test]
    fn test_parse_codehash_key() {
        let addr = make_addr();
        let mut key = vec![CODE_HASH_KEY_PREFIX];
        key.extend_from_slice(&addr);

        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::CodeHash);
        assert_eq!(stripped, &addr);
    }

    #[test]
    fn test_parse_code_key() {
        let addr = make_addr();
        let mut key = vec![CODE_KEY_PREFIX];
        key.extend_from_slice(&addr);

        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::Code);
        assert_eq!(stripped, &addr);
    }

    #[test]
    fn test_parse_storage_key() {
        let addr = make_addr();
        let slot = make_slot();
        let mut key = vec![STATE_KEY_PREFIX];
        key.extend_from_slice(&addr);
        key.extend_from_slice(&slot);

        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::Storage);
        assert_eq!(stripped.len(), ADDRESS_LEN + SLOT_LEN);
        assert_eq!(&stripped[..ADDRESS_LEN], &addr);
        assert_eq!(&stripped[ADDRESS_LEN..], &slot);
    }

    #[test]
    fn test_parse_legacy_key() {
        // 0x01 does not match any known prefix
        let key = vec![0x01, 0xaa, 0xbb, 0xcc];
        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::Legacy);
        assert_eq!(stripped, &key[..]);
    }

    #[test]
    fn test_parse_empty_key() {
        let (kind, stripped) = parse_evm_key(&[]);
        assert_eq!(kind, EvmKeyKind::Empty);
        assert!(stripped.is_empty());
    }

    #[test]
    fn test_parse_malformed_key() {
        // Correct nonce prefix but wrong length (10 bytes instead of 20)
        let mut key = vec![NONCE_KEY_PREFIX];
        key.extend_from_slice(&[0xffu8; 10]);

        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::Legacy);
        assert_eq!(stripped, &key[..]);
    }

    #[test]
    fn test_parse_single_byte_key() {
        // Just the state prefix byte with no payload
        let key = [STATE_KEY_PREFIX];
        let (kind, stripped) = parse_evm_key(&key);
        assert_eq!(kind, EvmKeyKind::Legacy);
        assert_eq!(stripped, &key[..]);
    }

    #[test]
    fn test_internal_key_len() {
        assert_eq!(internal_key_len(EvmKeyKind::Storage), 52);
        assert_eq!(internal_key_len(EvmKeyKind::Nonce), 20);
        assert_eq!(internal_key_len(EvmKeyKind::CodeHash), 20);
        assert_eq!(internal_key_len(EvmKeyKind::Code), 20);
        assert_eq!(internal_key_len(EvmKeyKind::Empty), 0);
        assert_eq!(internal_key_len(EvmKeyKind::Legacy), 0);
    }
}
