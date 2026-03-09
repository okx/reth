//! State overlay for inter-frame state propagation in parallel execution.
//!
//! Provides a two-layer read path for parallel EVM threads:
//! 1. [`FrameStateOverlay`] (plain HashMap, immutable during frame execution)
//! 2. reth `StateProvider` fallback (cross-block cache + QMDB/MDBX)
//!
//! Unlike the previous DashMap-based approach, this design uses plain `HashMap`:
//! - Writes happen **sequentially** between frames (no locks needed)
//! - Reads within a frame go through **immutable** `&FrameStateOverlay` (no locks)
//! - Each parallel thread uses revm's `CacheDB` for per-tx caching (zero contention)
//!
//! This avoids DashMap shard lock overhead while QMDB already provides lock-free
//! concurrent reads at the storage layer.

use alloy_primitives::{Address, B256, U256};
use revm_state::AccountInfo;
use std::collections::HashMap;

/// Sequential state overlay for inter-frame state propagation.
///
/// Accumulates state changes from executed frames so that subsequent frames
/// see the correct post-execution state. This overlay is:
/// - **Written sequentially** between frames (via [`apply_evm_state`])
/// - **Read concurrently** within a frame through [`OverlayStateProvider`]
///
/// No locks are needed because writes and reads never overlap temporally.
#[derive(Debug, Default)]
pub struct FrameStateOverlay {
    /// Accumulated account state: Address -> Option<AccountInfo>
    /// None = account confirmed destroyed/non-existent after execution.
    accounts: HashMap<Address, Option<AccountInfo>>,
    /// Accumulated storage: (Address, slot) -> value
    storage: HashMap<(Address, U256), U256>,
    /// Accumulated bytecodes: code_hash -> Bytecode
    bytecodes: HashMap<B256, revm_bytecode::Bytecode>,
    /// Block hashes: block_number -> hash
    block_hashes: HashMap<u64, B256>,
}

impl FrameStateOverlay {
    /// Create an empty overlay.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply EVM state changes from a completed transaction.
    ///
    /// Call this between frames (sequentially) to accumulate state diffs.
    pub fn apply_evm_state(&mut self, state: &revm::state::EvmState) {
        for (address, account) in state {
            let info = AccountInfo {
                balance: account.info.balance,
                nonce: account.info.nonce,
                code_hash: account.info.code_hash,
                code: account.info.code.clone(),
                account_id: None,
            };
            self.accounts.insert(*address, Some(info));

            for (slot, value) in &account.storage {
                self.storage.insert((*address, *slot), value.present_value);
            }
        }
    }

    /// Get a cached account from the overlay.
    ///
    /// Returns `None` on overlay miss (not tracked).
    /// Returns `Some(None)` if the account is confirmed absent.
    /// Returns `Some(Some(info))` if the account state is known.
    pub fn get_account(&self, address: &Address) -> Option<Option<AccountInfo>> {
        self.accounts.get(address).map(|v| v.clone())
    }

    /// Get a cached storage value from the overlay.
    pub fn get_storage(&self, address: &Address, slot: &U256) -> Option<U256> {
        self.storage.get(&(*address, *slot)).copied()
    }

    /// Get a cached bytecode from the overlay.
    pub fn get_bytecode(&self, hash: &B256) -> Option<revm_bytecode::Bytecode> {
        self.bytecodes.get(hash).cloned()
    }

    /// Get a cached block hash.
    pub fn get_block_hash(&self, number: &u64) -> Option<B256> {
        self.block_hashes.get(number).copied()
    }

    /// Return number of accounts in the overlay.
    pub fn accounts_len(&self) -> usize {
        self.accounts.len()
    }

    /// Return number of storage slots in the overlay.
    pub fn storage_len(&self) -> usize {
        self.storage.len()
    }
}

// ---------------------------------------------------------------------------
// OverlayStateProvider: DatabaseRef implementation
// ---------------------------------------------------------------------------

use reth_storage_api::StateProvider;
use reth_storage_errors::provider::ProviderError;
use revm::DatabaseRef;

/// Wraps a [`FrameStateOverlay`] and a fallback [`StateProvider`] to implement
/// revm's [`DatabaseRef`].
///
/// Within a frame, multiple parallel threads share an immutable reference to
/// this provider. Each thread wraps it in `CacheDB::new(&provider)` for per-tx
/// caching — no concurrent writes to the overlay, no lock contention.
pub struct OverlayStateProvider<'a> {
    /// Immutable overlay with accumulated state from prior frames.
    overlay: &'a FrameStateOverlay,
    /// Fallback: reth StateProvider (MemoryOverlayStateProvider -> QMDB/MDBX)
    fallback: &'a (dyn StateProvider + Sync),
}

impl core::fmt::Debug for OverlayStateProvider<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OverlayStateProvider")
            .field("overlay_accounts", &self.overlay.accounts_len())
            .field("overlay_storage", &self.overlay.storage_len())
            .finish_non_exhaustive()
    }
}

impl<'a> OverlayStateProvider<'a> {
    /// Create a new provider with the given overlay and fallback.
    pub fn new(overlay: &'a FrameStateOverlay, fallback: &'a (dyn StateProvider + Sync)) -> Self {
        Self { overlay, fallback }
    }
}

impl DatabaseRef for OverlayStateProvider<'_> {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Check overlay first (state from prior frames / sequencer)
        if let Some(info) = self.overlay.get_account(&address) {
            return Ok(info);
        }
        // Fallback to StateProvider (parent block state via QMDB, lock-free)
        Ok(self.fallback.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm_bytecode::Bytecode, Self::Error> {
        if let Some(code) = self.overlay.get_bytecode(&code_hash) {
            return Ok(code);
        }
        Ok(self.fallback.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.overlay.get_storage(&address, &index) {
            return Ok(value);
        }
        Ok(self.fallback.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(hash) = self.overlay.get_block_hash(&number) {
            return Ok(hash);
        }
        Ok(reth_storage_api::BlockHashReader::block_hash(self.fallback, number)?
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overlay_new_is_empty() {
        let overlay = FrameStateOverlay::new();
        assert_eq!(overlay.accounts_len(), 0);
        assert_eq!(overlay.storage_len(), 0);
    }

    #[test]
    fn test_overlay_apply_evm_state() {
        let mut overlay = FrameStateOverlay::new();
        let addr = Address::with_last_byte(0x42);

        let mut state = revm::state::EvmState::default();
        let mut storage = revm_state::EvmStorage::default();
        storage.insert(
            U256::from(7),
            revm_state::EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(999),
                ..Default::default()
            },
        );

        let account = revm_state::Account {
            info: AccountInfo { balance: U256::from(1000), nonce: 5, ..Default::default() },
            original_info: Box::new(AccountInfo::default()),
            status: revm_state::AccountStatus::Touched,
            storage,
            transaction_id: 0,
        };
        state.insert(addr, account);

        overlay.apply_evm_state(&state);

        // Account should be in overlay
        let info = overlay.get_account(&addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(1000));
        assert_eq!(info.nonce, 5);

        // Storage should be in overlay
        assert_eq!(overlay.get_storage(&addr, &U256::from(7)), Some(U256::from(999)));

        // Non-existent entries should be None
        assert!(overlay.get_account(&Address::with_last_byte(0x99)).is_none());
        assert!(overlay.get_storage(&addr, &U256::from(8)).is_none());
    }

    #[test]
    fn test_overlay_accumulates_across_frames() {
        let mut overlay = FrameStateOverlay::new();
        let addr_a = Address::with_last_byte(0xA0);
        let addr_b = Address::with_last_byte(0xB0);

        // Frame 1: TxA modifies addr_a
        let mut state1 = revm::state::EvmState::default();
        state1.insert(
            addr_a,
            revm_state::Account {
                info: AccountInfo { balance: U256::from(100), nonce: 1, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage: Default::default(),
                transaction_id: 0,
            },
        );
        overlay.apply_evm_state(&state1);

        // Frame 2: TxB modifies addr_b
        let mut state2 = revm::state::EvmState::default();
        state2.insert(
            addr_b,
            revm_state::Account {
                info: AccountInfo { balance: U256::from(200), nonce: 2, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage: Default::default(),
                transaction_id: 0,
            },
        );
        overlay.apply_evm_state(&state2);

        // Both should be visible
        assert_eq!(overlay.get_account(&addr_a).unwrap().unwrap().balance, U256::from(100));
        assert_eq!(overlay.get_account(&addr_b).unwrap().unwrap().balance, U256::from(200));
    }

    #[test]
    fn test_overlay_later_frame_overwrites_earlier() {
        let mut overlay = FrameStateOverlay::new();
        let addr = Address::with_last_byte(0xA0);

        // Frame 1
        let mut state1 = revm::state::EvmState::default();
        state1.insert(
            addr,
            revm_state::Account {
                info: AccountInfo { balance: U256::from(100), nonce: 1, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage: Default::default(),
                transaction_id: 0,
            },
        );
        overlay.apply_evm_state(&state1);

        // Frame 2 updates same account
        let mut state2 = revm::state::EvmState::default();
        state2.insert(
            addr,
            revm_state::Account {
                info: AccountInfo { balance: U256::from(50), nonce: 2, ..Default::default() },
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage: Default::default(),
                transaction_id: 0,
            },
        );
        overlay.apply_evm_state(&state2);

        // Latest value should win
        let info = overlay.get_account(&addr).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(50));
        assert_eq!(info.nonce, 2);
    }

    #[test]
    fn test_overlay_storage_update() {
        let mut overlay = FrameStateOverlay::new();
        let addr = Address::with_last_byte(0xA0);

        let mut state = revm::state::EvmState::default();
        let mut storage = revm_state::EvmStorage::default();
        storage.insert(
            U256::from(1),
            revm_state::EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(42),
                ..Default::default()
            },
        );
        storage.insert(
            U256::from(2),
            revm_state::EvmStorageSlot {
                original_value: U256::ZERO,
                present_value: U256::from(99),
                ..Default::default()
            },
        );

        state.insert(
            addr,
            revm_state::Account {
                info: AccountInfo::default(),
                original_info: Box::new(AccountInfo::default()),
                status: revm_state::AccountStatus::Touched,
                storage,
                transaction_id: 0,
            },
        );
        overlay.apply_evm_state(&state);

        assert_eq!(overlay.get_storage(&addr, &U256::from(1)), Some(U256::from(42)));
        assert_eq!(overlay.get_storage(&addr, &U256::from(2)), Some(U256::from(99)));
        assert!(overlay.get_storage(&addr, &U256::from(3)).is_none());
    }

    #[test]
    fn test_overlay_bytecode() {
        let overlay = FrameStateOverlay {
            bytecodes: HashMap::from([(
                B256::with_last_byte(0xCC),
                revm_bytecode::Bytecode::new_raw(alloy_primitives::Bytes::from_static(&[
                    0x60, 0x00,
                ])),
            )]),
            ..Default::default()
        };

        assert!(overlay.get_bytecode(&B256::with_last_byte(0xCC)).is_some());
        assert!(overlay.get_bytecode(&B256::with_last_byte(0xDD)).is_none());
    }

    #[test]
    fn test_overlay_block_hash() {
        let overlay = FrameStateOverlay {
            block_hashes: HashMap::from([(42u64, B256::with_last_byte(0xAB))]),
            ..Default::default()
        };

        assert_eq!(overlay.get_block_hash(&42), Some(B256::with_last_byte(0xAB)));
        assert!(overlay.get_block_hash(&99).is_none());
    }
}
