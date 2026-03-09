//! Transaction read/write sets for conflict detection in parallel execution.
//!
//! Uses 10-byte truncated hashes (`ShortHash`) to compactly represent accessed
//! accounts and storage slots. The truncation trades a negligible collision
//! probability (~2^-80) for a 3.2x memory reduction vs full 32-byte hashes.

use alloy_primitives::{keccak256, Address, U256};
use revm::context::result::ResultAndState;

/// A 10-byte truncated keccak hash, balancing collision resistance with memory
/// efficiency for conflict-detection bitmaps.
pub type ShortHash = [u8; 10];

/// Conflict read/write sets extracted from a single transaction execution.
///
/// Tracks which accounts and storage slots were read or written, enabling
/// the Framer to detect data dependencies between transactions and build
/// conflict-free execution groups.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrwSets {
    /// Accounts that were read (appeared in the EVM state map).
    pub account_reads: Vec<ShortHash>,
    /// Accounts whose info was modified (balance, nonce, code, or destruction).
    pub account_writes: Vec<ShortHash>,
    /// Storage slots that were loaded but not modified.
    pub storage_reads: Vec<ShortHash>,
    /// Storage slots whose value changed during execution.
    pub storage_writes: Vec<ShortHash>,
}

/// Hash an address down to a 10-byte `ShortHash`.
pub fn short_hash_address(address: &Address) -> ShortHash {
    let hash = keccak256(address);
    let mut short = [0u8; 10];
    short.copy_from_slice(&hash[..10]);
    short
}

/// Hash an address + storage slot pair down to a 10-byte `ShortHash`.
///
/// Concatenates address (20 bytes) and slot (32 bytes big-endian) before
/// hashing, so the same slot on different contracts produces different hashes.
pub fn short_hash_slot(address: &Address, slot: &U256) -> ShortHash {
    let mut buf = [0u8; 52];
    buf[..20].copy_from_slice(address.as_slice());
    buf[20..].copy_from_slice(&slot.to_be_bytes::<32>());
    let hash = keccak256(buf);
    let mut short = [0u8; 10];
    short.copy_from_slice(&hash[..10]);
    short
}

/// Extract read/write sets from a revm execution result.
///
/// Classification logic:
/// - **account_reads**: account was touched by the EVM (appeared in state map)
/// - **account_writes**: account info (balance/nonce/code) differs from original
/// - **storage_reads**: storage slot appears in account's storage map but value unchanged
/// - **storage_writes**: storage slot's present value differs from original value
pub fn extract_crw_sets<H>(result: &ResultAndState<H>) -> CrwSets {
    let mut sets = CrwSets::default();

    for (address, account) in &result.state {
        let addr_hash = short_hash_address(address);

        // Every account in the state map was accessed (read)
        sets.account_reads.push(addr_hash);

        // Determine if the account was written: compare current info to original
        let info_changed = account.info != *account.original_info;
        let status_indicates_write = account.is_selfdestructed() || account.is_created();

        if info_changed || status_indicates_write {
            sets.account_writes.push(addr_hash);
        }

        // Classify each storage slot
        for (slot, value) in &account.storage {
            let slot_hash = short_hash_slot(address, slot);
            if value.is_changed() {
                sets.storage_writes.push(slot_hash);
            } else {
                sets.storage_reads.push(slot_hash);
            }
        }
    }

    sets
}

impl CrwSets {
    /// Returns `true` if no accounts or storage slots were accessed.
    pub fn is_empty(&self) -> bool {
        self.account_reads.is_empty() &&
            self.account_writes.is_empty() &&
            self.storage_reads.is_empty() &&
            self.storage_writes.is_empty()
    }

    /// Iterate over every hash in all four sets.
    pub fn all_hashes(&self) -> impl Iterator<Item = &ShortHash> {
        self.account_reads
            .iter()
            .chain(self.account_writes.iter())
            .chain(self.storage_reads.iter())
            .chain(self.storage_writes.iter())
    }

    /// Merge another `CrwSets` into this one (used when combining ExeTasks).
    pub fn merge(&mut self, other: &CrwSets) {
        self.account_reads.extend_from_slice(&other.account_reads);
        self.account_writes.extend_from_slice(&other.account_writes);
        self.storage_reads.extend_from_slice(&other.storage_reads);
        self.storage_writes.extend_from_slice(&other.storage_writes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use revm::context::result::{ExecutionResult, Output, SuccessReason};
    use revm_state::{Account, AccountInfo, AccountStatus, EvmStorageSlot};

    fn test_address(byte: u8) -> Address {
        Address::new([byte; 20])
    }

    #[test]
    fn short_hash_address_is_consistent() {
        let addr = test_address(0xAA);
        let h1 = short_hash_address(&addr);
        let h2 = short_hash_address(&addr);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 10);
    }

    #[test]
    fn short_hash_address_different_for_different_addresses() {
        let h1 = short_hash_address(&test_address(0x01));
        let h2 = short_hash_address(&test_address(0x02));
        assert_ne!(h1, h2);
    }

    #[test]
    fn short_hash_slot_different_for_different_slots() {
        let addr = test_address(0xBB);
        let h1 = short_hash_slot(&addr, &U256::from(1));
        let h2 = short_hash_slot(&addr, &U256::from(2));
        assert_ne!(h1, h2);
    }

    #[test]
    fn short_hash_slot_different_for_different_addresses_same_slot() {
        let slot = U256::from(42);
        let h1 = short_hash_slot(&test_address(0x01), &slot);
        let h2 = short_hash_slot(&test_address(0x02), &slot);
        assert_ne!(h1, h2);
    }

    /// Build a `ResultAndState` with a successful result and the given state.
    /// Uses `()` as the halt reason since `extract_crw_sets` is generic over it.
    fn make_success_result(state: revm_state::EvmState) -> ResultAndState<()> {
        ResultAndState {
            result: ExecutionResult::Success {
                reason: SuccessReason::Return,
                gas_used: 21000,
                gas_refunded: 0,
                logs: vec![],
                output: Output::Call(Default::default()),
            },
            state,
        }
    }

    #[test]
    fn extract_account_read_only() {
        // Account touched but info unchanged => read only
        let addr = test_address(0x01);
        let info = AccountInfo::default();
        let account = Account {
            info: info.clone(),
            original_info: Box::new(info),
            status: AccountStatus::Touched,
            storage: Default::default(),
            transaction_id: 0,
        };

        let mut state = revm_state::EvmState::default();
        state.insert(addr, account);

        let result = make_success_result(state);
        let sets = extract_crw_sets(&result);

        assert!(sets.account_reads.contains(&short_hash_address(&addr)));
        assert!(!sets.account_writes.contains(&short_hash_address(&addr)));
        assert!(sets.storage_reads.is_empty());
        assert!(sets.storage_writes.is_empty());
    }

    #[test]
    fn extract_account_write_when_balance_changed() {
        let addr = test_address(0x02);
        let original = AccountInfo { balance: U256::from(100), ..Default::default() };
        let modified = AccountInfo { balance: U256::from(50), ..Default::default() };

        let account = Account {
            info: modified,
            original_info: Box::new(original),
            status: AccountStatus::Touched,
            storage: Default::default(),
            transaction_id: 0,
        };

        let mut state = revm_state::EvmState::default();
        state.insert(addr, account);

        let result = make_success_result(state);
        let sets = extract_crw_sets(&result);

        assert!(sets.account_reads.contains(&short_hash_address(&addr)));
        assert!(sets.account_writes.contains(&short_hash_address(&addr)));
    }

    #[test]
    fn extract_storage_read_and_write() {
        let addr = test_address(0x03);
        let info = AccountInfo::default();

        let read_slot = U256::from(10);
        let write_slot = U256::from(20);

        // Slot that was read but not changed (present == original)
        let read_storage = EvmStorageSlot {
            original_value: U256::from(5),
            present_value: U256::from(5),
            ..Default::default()
        };

        // Slot that was written (present != original)
        let write_storage = EvmStorageSlot::new_changed(U256::from(5), U256::from(99), 0);

        let mut storage = revm_state::EvmStorage::default();
        storage.insert(read_slot, read_storage);
        storage.insert(write_slot, write_storage);

        let account = Account {
            info: info.clone(),
            original_info: Box::new(info),
            status: AccountStatus::Touched,
            storage,
            transaction_id: 0,
        };

        let mut state = revm_state::EvmState::default();
        state.insert(addr, account);

        let result = make_success_result(state);
        let sets = extract_crw_sets(&result);

        let expected_read_hash = short_hash_slot(&addr, &read_slot);
        let expected_write_hash = short_hash_slot(&addr, &write_slot);

        assert!(sets.storage_reads.contains(&expected_read_hash));
        assert!(!sets.storage_writes.contains(&expected_read_hash));
        assert!(sets.storage_writes.contains(&expected_write_hash));
        assert!(!sets.storage_reads.contains(&expected_write_hash));
    }

    #[test]
    fn merge_combines_two_crw_sets() {
        let mut a = CrwSets {
            account_reads: vec![[1u8; 10]],
            account_writes: vec![[2u8; 10]],
            storage_reads: vec![[3u8; 10]],
            storage_writes: vec![],
        };

        let b = CrwSets {
            account_reads: vec![[4u8; 10]],
            account_writes: vec![],
            storage_reads: vec![],
            storage_writes: vec![[5u8; 10]],
        };

        a.merge(&b);

        assert_eq!(a.account_reads.len(), 2);
        assert_eq!(a.account_writes.len(), 1);
        assert_eq!(a.storage_reads.len(), 1);
        assert_eq!(a.storage_writes.len(), 1);
        assert!(a.account_reads.contains(&[1u8; 10]));
        assert!(a.account_reads.contains(&[4u8; 10]));
        assert!(a.storage_writes.contains(&[5u8; 10]));
    }

    #[test]
    fn is_empty_on_default() {
        let sets = CrwSets::default();
        assert!(sets.is_empty());
    }

    #[test]
    fn is_empty_false_when_populated() {
        let sets = CrwSets { account_reads: vec![[0u8; 10]], ..Default::default() };
        assert!(!sets.is_empty());
    }

    #[test]
    fn all_hashes_iterates_all_sets() {
        let sets = CrwSets {
            account_reads: vec![[1u8; 10]],
            account_writes: vec![[2u8; 10]],
            storage_reads: vec![[3u8; 10]],
            storage_writes: vec![[4u8; 10]],
        };
        let all: Vec<_> = sets.all_hashes().collect();
        assert_eq!(all.len(), 4);
    }
}
