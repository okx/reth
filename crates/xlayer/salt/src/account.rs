//! Account encoding/decoding between reth and SALT formats.
//!
//! SALT stores state as plain key-value pairs:
//! - Account: plain_key = address (20 bytes), value = encoded account (40 or 72 bytes)
//! - Storage: plain_key = address || storage_key (52 bytes), value = slot value (32 bytes)
//!
//! Account encoding format (compact, big-endian):
//! - nonce: 8 bytes
//! - balance: 32 bytes
//! - (if contract) code_hash: 32 bytes
//!
//! EOA accounts use 40 bytes; contract accounts use 72 bytes.

use alloy_primitives::{Address, B256, U256};
use reth_primitives_traits::Account;

/// Size of an EOA account encoding: 8 (nonce) + 32 (balance).
const EOA_ENCODED_SIZE: usize = 40;

/// Size of a contract account encoding: 8 (nonce) + 32 (balance) + 32 (code_hash).
const CONTRACT_ENCODED_SIZE: usize = 72;

/// Storage plain key size: 20 (address) + 32 (storage_key).
pub const STORAGE_PLAIN_KEY_SIZE: usize = 52;

/// Encode an address as a SALT plain key (20 bytes).
#[inline]
pub fn account_plain_key(address: &Address) -> Vec<u8> {
    address.as_slice().to_vec()
}

/// Encode a storage slot as a SALT plain key (52 bytes = address || storage_key).
#[inline]
pub fn storage_plain_key(address: &Address, slot: &B256) -> Vec<u8> {
    let mut key = Vec::with_capacity(STORAGE_PLAIN_KEY_SIZE);
    key.extend_from_slice(address.as_slice());
    key.extend_from_slice(slot.as_slice());
    key
}

/// Encode a reth `Account` into SALT value bytes.
///
/// Returns 40 bytes for EOA, 72 bytes for contracts (with `code_hash`).
pub fn encode_account(account: &Account) -> Vec<u8> {
    let has_code =
        account.bytecode_hash.is_some_and(|h| h != alloy_consensus::constants::KECCAK_EMPTY);

    let size = if has_code { CONTRACT_ENCODED_SIZE } else { EOA_ENCODED_SIZE };
    let mut buf = Vec::with_capacity(size);

    buf.extend_from_slice(&account.nonce.to_be_bytes());
    buf.extend_from_slice(&account.balance.to_be_bytes::<32>());

    if has_code {
        buf.extend_from_slice(account.bytecode_hash.unwrap().as_slice());
    }

    buf
}

/// Decode SALT value bytes back into a reth `Account`.
///
/// Returns `None` if the byte slice has an invalid length.
pub fn decode_account(value: &[u8]) -> Option<Account> {
    match value.len() {
        EOA_ENCODED_SIZE => {
            let nonce = u64::from_be_bytes(value[..8].try_into().ok()?);
            let balance = U256::from_be_slice(&value[8..40]);
            Some(Account { nonce, balance, bytecode_hash: None })
        }
        CONTRACT_ENCODED_SIZE => {
            let nonce = u64::from_be_bytes(value[..8].try_into().ok()?);
            let balance = U256::from_be_slice(&value[8..40]);
            let code_hash = B256::from_slice(&value[40..72]);
            Some(Account { nonce, balance, bytecode_hash: Some(code_hash) })
        }
        _ => None,
    }
}

/// Encode a storage value as SALT value bytes (32 bytes, big-endian).
#[inline]
pub fn encode_storage_value(value: &U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

/// Decode SALT value bytes into a storage `U256`.
#[inline]
pub fn decode_storage_value(value: &[u8]) -> Option<U256> {
    if value.len() == 32 {
        Some(U256::from_be_slice(value))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eoa_account_roundtrip() {
        let account = Account { nonce: 42, balance: U256::from(1_000_000u64), bytecode_hash: None };

        let encoded = encode_account(&account);
        assert_eq!(encoded.len(), EOA_ENCODED_SIZE);

        let decoded = decode_account(&encoded).unwrap();
        assert_eq!(decoded.nonce, account.nonce);
        assert_eq!(decoded.balance, account.balance);
        assert_eq!(decoded.bytecode_hash, None);
    }

    #[test]
    fn test_contract_account_roundtrip() {
        let code_hash = B256::from([0xab; 32]);
        let account =
            Account { nonce: 1, balance: U256::from(500u64), bytecode_hash: Some(code_hash) };

        let encoded = encode_account(&account);
        assert_eq!(encoded.len(), CONTRACT_ENCODED_SIZE);

        let decoded = decode_account(&encoded).unwrap();
        assert_eq!(decoded.nonce, account.nonce);
        assert_eq!(decoded.balance, account.balance);
        assert_eq!(decoded.bytecode_hash, Some(code_hash));
    }

    #[test]
    fn test_storage_value_roundtrip() {
        let value = U256::from(0xdeadbeef_u64);
        let encoded = encode_storage_value(&value);
        assert_eq!(encoded.len(), 32);

        let decoded = decode_storage_value(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn test_storage_plain_key() {
        let address = Address::from([0x11; 20]);
        let slot = B256::from([0x22; 32]);
        let key = storage_plain_key(&address, &slot);
        assert_eq!(key.len(), STORAGE_PLAIN_KEY_SIZE);
        assert_eq!(&key[..20], address.as_slice());
        assert_eq!(&key[20..], slot.as_slice());
    }

    #[test]
    fn test_eoa_with_keccak_empty_treated_as_no_code() {
        let account = Account {
            nonce: 0,
            balance: U256::ZERO,
            bytecode_hash: Some(alloy_consensus::constants::KECCAK_EMPTY),
        };
        let encoded = encode_account(&account);
        assert_eq!(encoded.len(), EOA_ENCODED_SIZE);
    }
}
