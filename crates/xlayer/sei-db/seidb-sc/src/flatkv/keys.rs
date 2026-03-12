use seidb_common::error::{Result, SeiDbError};

pub const ADDRESS_LEN: usize = 20;
pub const CODE_HASH_LEN: usize = 32;
pub const SLOT_LEN: usize = 32;
pub const BALANCE_LEN: usize = 32;
pub const NONCE_LEN: usize = 8;
pub const ACCOUNT_VALUE_EOA_LEN: usize = BALANCE_LEN + NONCE_LEN; // 40
pub const ACCOUNT_VALUE_CONTRACT_LEN: usize = BALANCE_LEN + NONCE_LEN + CODE_HASH_LEN; // 72
pub const LOCAL_META_SIZE: usize = 8;
pub const DB_LOCAL_META_KEY: &[u8] = &[0x00];

pub type Address = [u8; ADDRESS_LEN];
pub type CodeHash = [u8; CODE_HASH_LEN];
pub type Slot = [u8; SLOT_LEN];
pub type Balance = [u8; BALANCE_LEN];

// =============================================================================
// AccountValue
// =============================================================================

/// Account record stored in the account DB.
///
/// Encoding is variable-length to save space for EOA accounts:
/// - EOA (no code):      balance(32) || nonce(8)                    = 40 bytes
/// - Contract (has code): balance(32) || nonce(8) || codehash(32)   = 72 bytes
///
/// `CodeHash == [0u8; 32]` means the account has no code (EOA).
/// Note: empty code contracts have `CodeHash = keccak256("")` which is non-zero.
#[derive(Clone, Debug)]
pub struct AccountValue {
    pub balance: Balance,
    pub nonce: u64,
    pub code_hash: CodeHash,
}

impl Default for AccountValue {
    fn default() -> Self {
        Self { balance: [0u8; BALANCE_LEN], nonce: 0, code_hash: [0u8; CODE_HASH_LEN] }
    }
}

impl AccountValue {
    /// Returns true if the account has code (is a contract).
    pub fn has_code(&self) -> bool {
        self.code_hash != [0u8; CODE_HASH_LEN]
    }

    /// Encode this account value to bytes.
    pub fn encode(&self) -> Vec<u8> {
        encode_account_value(self)
    }
}

/// Encodes an `AccountValue` into a variable-length byte vector.
/// EOA accounts (no code) are encoded as 40 bytes, contracts as 72 bytes.
pub fn encode_account_value(v: &AccountValue) -> Vec<u8> {
    if !v.has_code() {
        // EOA: balance(32) || nonce(8)
        let mut b = Vec::with_capacity(ACCOUNT_VALUE_EOA_LEN);
        b.extend_from_slice(&v.balance);
        b.extend_from_slice(&v.nonce.to_be_bytes());
        b
    } else {
        // Contract: balance(32) || nonce(8) || codehash(32)
        let mut b = Vec::with_capacity(ACCOUNT_VALUE_CONTRACT_LEN);
        b.extend_from_slice(&v.balance);
        b.extend_from_slice(&v.nonce.to_be_bytes());
        b.extend_from_slice(&v.code_hash);
        b
    }
}

/// Decodes a variable-length account record.
/// Returns an error if the length is neither 40 (EOA) nor 72 (contract) bytes.
pub fn decode_account_value(b: &[u8]) -> Result<AccountValue> {
    match b.len() {
        ACCOUNT_VALUE_EOA_LEN => {
            let mut v = AccountValue::default();
            v.balance.copy_from_slice(&b[..BALANCE_LEN]);
            v.nonce =
                u64::from_be_bytes(b[BALANCE_LEN..BALANCE_LEN + NONCE_LEN].try_into().unwrap());
            Ok(v)
        }
        ACCOUNT_VALUE_CONTRACT_LEN => {
            let mut v = AccountValue::default();
            v.balance.copy_from_slice(&b[..BALANCE_LEN]);
            v.nonce =
                u64::from_be_bytes(b[BALANCE_LEN..BALANCE_LEN + NONCE_LEN].try_into().unwrap());
            v.code_hash.copy_from_slice(&b[BALANCE_LEN + NONCE_LEN..]);
            Ok(v)
        }
        other => Err(SeiDbError::Other(format!(
            "invalid account value length: got {}, want {} (EOA) or {} (contract)",
            other, ACCOUNT_VALUE_EOA_LEN, ACCOUNT_VALUE_CONTRACT_LEN
        ))),
    }
}

// =============================================================================
// DB Key Builders
// =============================================================================

/// Returns the accountDB key for the given address.
/// Key format: addr(20)
pub fn account_key(addr: &Address) -> Vec<u8> {
    addr.to_vec()
}

/// Returns the storageDB key for (addr, slot).
/// Key format: addr(20) || slot(32) = 52 bytes
pub fn storage_key(addr: &Address, slot: &Slot) -> Vec<u8> {
    let mut key = Vec::with_capacity(ADDRESS_LEN + SLOT_LEN);
    key.extend_from_slice(addr);
    key.extend_from_slice(slot);
    key
}

/// Returns the exclusive upper bound for prefix iteration, or `None` if the
/// prefix is empty or consists entirely of 0xFF bytes.
pub fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        return None;
    }
    let mut b = prefix.to_vec();
    for i in (0..b.len()).rev() {
        if b[i] != 0xFF {
            b[i] += 1;
            b.truncate(i + 1);
            return Some(b);
        }
    }
    None
}

// =============================================================================
// LocalMeta
// =============================================================================

/// Per-DB version tracking metadata, stored at `DB_LOCAL_META_KEY`.
#[derive(Clone, Debug, Default)]
pub struct LocalMeta {
    pub committed_version: i64,
}

/// Encodes `LocalMeta` as fixed 8 bytes (big-endian).
pub fn marshal_local_meta(m: &LocalMeta) -> Vec<u8> {
    (m.committed_version as u64).to_be_bytes().to_vec()
}

/// Decodes `LocalMeta` from bytes. Expects exactly 8 bytes.
pub fn unmarshal_local_meta(data: &[u8]) -> Result<LocalMeta> {
    if data.len() != LOCAL_META_SIZE {
        return Err(SeiDbError::Other(format!(
            "invalid LocalMeta size: got {}, want {}",
            data.len(),
            LOCAL_META_SIZE
        )));
    }
    let committed_version = i64::from_be_bytes(data[..LOCAL_META_SIZE].try_into().unwrap());
    Ok(LocalMeta { committed_version })
}

/// Returns the iterator lower bound that excludes `DB_LOCAL_META_KEY`.
/// Lexicographically: `[0x00]` (1 byte) < `[0x00, 0x00]` (2 bytes) < any user key (>=20 bytes).
pub fn meta_key_lower_bound() -> Vec<u8> {
    vec![0x00, 0x00]
}

// =============================================================================
// Conversion helpers
// =============================================================================

/// Converts a byte slice to an `Address` if exactly 20 bytes.
pub fn address_from_bytes(b: &[u8]) -> Option<Address> {
    b.try_into().ok()
}

/// Converts a byte slice to a `Slot` if exactly 32 bytes.
pub fn slot_from_bytes(b: &[u8]) -> Option<Slot> {
    b.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_value_roundtrip_eoa() {
        let mut v = AccountValue::default();
        v.balance[31] = 42;
        v.nonce = 7;
        // code_hash stays all-zero (EOA)

        let encoded = encode_account_value(&v);
        assert_eq!(encoded.len(), ACCOUNT_VALUE_EOA_LEN);

        let decoded = decode_account_value(&encoded).unwrap();
        assert_eq!(decoded.balance, v.balance);
        assert_eq!(decoded.nonce, v.nonce);
        assert_eq!(decoded.code_hash, [0u8; CODE_HASH_LEN]);
        assert!(!decoded.has_code());
    }

    #[test]
    fn test_account_value_roundtrip_contract() {
        let mut v = AccountValue::default();
        v.balance[0] = 1;
        v.balance[31] = 0xFF;
        v.nonce = 1000;
        v.code_hash[0] = 0xAB;
        v.code_hash[31] = 0xCD;

        let encoded = encode_account_value(&v);
        assert_eq!(encoded.len(), ACCOUNT_VALUE_CONTRACT_LEN);

        let decoded = decode_account_value(&encoded).unwrap();
        assert_eq!(decoded.balance, v.balance);
        assert_eq!(decoded.nonce, v.nonce);
        assert_eq!(decoded.code_hash, v.code_hash);
        assert!(decoded.has_code());
    }

    #[test]
    fn test_account_value_invalid_length() {
        assert!(decode_account_value(&[0u8; 10]).is_err());
        assert!(decode_account_value(&[0u8; 50]).is_err());
        assert!(decode_account_value(&[]).is_err());
    }

    #[test]
    fn test_account_value_has_code() {
        let eoa = AccountValue::default();
        assert!(!eoa.has_code());

        let mut contract = AccountValue::default();
        contract.code_hash[15] = 1;
        assert!(contract.has_code());
    }

    #[test]
    fn test_account_value_nonce_big_endian() {
        let mut v = AccountValue::default();
        v.nonce = 0x0102030405060708;

        let encoded = encode_account_value(&v);
        // Nonce starts at offset BALANCE_LEN (32)
        assert_eq!(
            &encoded[BALANCE_LEN..BALANCE_LEN + NONCE_LEN],
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
    }

    #[test]
    fn test_local_meta_roundtrip() {
        let m = LocalMeta { committed_version: 42 };
        let data = marshal_local_meta(&m);
        assert_eq!(data.len(), LOCAL_META_SIZE);
        let m2 = unmarshal_local_meta(&data).unwrap();
        assert_eq!(m2.committed_version, 42);

        // Negative version
        let m3 = LocalMeta { committed_version: -1 };
        let data3 = marshal_local_meta(&m3);
        let m4 = unmarshal_local_meta(&data3).unwrap();
        assert_eq!(m4.committed_version, -1);
    }

    #[test]
    fn test_prefix_end_basic() {
        assert_eq!(prefix_end(&[0x01, 0x02]), Some(vec![0x01, 0x03]));
    }

    #[test]
    fn test_prefix_end_carry() {
        assert_eq!(prefix_end(&[0x01, 0xFF]), Some(vec![0x02]));
    }

    #[test]
    fn test_prefix_end_all_ff() {
        assert_eq!(prefix_end(&[0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    fn test_storage_key_length() {
        let addr = [0u8; ADDRESS_LEN];
        let slot = [0u8; SLOT_LEN];
        let key = storage_key(&addr, &slot);
        assert_eq!(key.len(), ADDRESS_LEN + SLOT_LEN); // 52
    }

    #[test]
    fn test_address_from_bytes() {
        let valid = [0xABu8; ADDRESS_LEN];
        assert_eq!(address_from_bytes(&valid), Some(valid));

        // Wrong length
        assert_eq!(address_from_bytes(&[0u8; 19]), None);
        assert_eq!(address_from_bytes(&[0u8; 21]), None);
        assert_eq!(address_from_bytes(&[]), None);
    }
}
