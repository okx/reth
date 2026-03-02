//! SALT-based state root computation compatible with reth's pipeline.
//!
//! This module provides [`SaltStateRoot`], which wraps SALT's `StateRoot` engine
//! and adapts it for use in reth's block processing pipeline.
//!
//! # Workflow
//!
//! 1. Convert `BundleState` to SALT plain k-v updates via [`bundle_state_to_plain_kv`]
//! 2. Apply updates through `EphemeralSaltState::update_fin()` to get `StateUpdates`
//! 3. Persist state with `store.update_state()`
//! 4. Compute state root with `StateRoot::update_fin()`
//! 5. Persist trie updates with `store.update_trie()`
//!
//! [`bundle_state_to_plain_kv`]: crate::convert::bundle_state_to_plain_kv

use alloy_primitives::B256;
use salt::{
    traits::{StateReader, TrieReader},
    types::ScalarBytes,
    EphemeralSaltState, MemStore, StateRoot, StateUpdates, TrieUpdates,
};
use std::time::{Duration, Instant};

/// Result of a SALT state root computation, including timing data.
#[derive(Debug)]
pub struct SaltStateRootOutcome {
    /// The computed state root (32 bytes), as `B256` for reth compatibility.
    pub state_root: B256,
    /// SALT-internal `StateUpdates` (bucket-level changes).
    pub state_updates: StateUpdates,
    /// SALT-internal `TrieUpdates` (commitment changes).
    pub trie_updates: TrieUpdates,
    /// Time spent converting `BundleState` to SALT plain k-v + applying via `EphemeralSaltState`.
    pub state_update_duration: Duration,
    /// Time spent computing the state root via `StateRoot::update_fin`.
    pub root_compute_duration: Duration,
}

/// Errors during SALT state root computation.
#[derive(Debug, thiserror::Error)]
pub enum SaltStateRootError {
    /// Error from salt state operations.
    #[error("salt state error: {0}")]
    State(String),
    /// Error from salt trie operations.
    #[error("salt trie error: {0}")]
    Trie(String),
}

/// Compute SALT state root from a `BundleState` using the given store.
///
/// This is the main entry point for SALT state root computation in reth's pipeline.
///
/// # Arguments
/// - `store`: A SALT store implementing `StateReader + TrieReader` (e.g., `MemStore`)
/// - `salt_root`: A mutable `StateRoot` engine (maintains incremental trie state)
/// - `bundle_state`: The revm `BundleState` from block execution
///
/// # Returns
/// A [`SaltStateRootOutcome`] with the root hash, updates, and timing data.
pub fn compute_salt_state_root<Store>(
    store: &Store,
    salt_root: &mut StateRoot<'_, Store>,
    bundle_state: &revm_database::BundleState,
) -> Result<SaltStateRootOutcome, SaltStateRootError>
where
    Store: TrieReader + StateReader<Error = <Store as TrieReader>::Error>,
    <Store as TrieReader>::Error: std::fmt::Debug + Send + Sync + std::error::Error + 'static,
{
    let kvs = crate::convert::bundle_state_to_plain_kv(bundle_state);

    let state_start = Instant::now();
    let mut ephemeral = EphemeralSaltState::new(store);
    let state_updates =
        ephemeral.update_fin(&kvs).map_err(|e| SaltStateRootError::State(format!("{e:?}")))?;
    let state_update_duration = state_start.elapsed();

    let root_start = Instant::now();
    let (root_bytes, trie_updates) = salt_root
        .update_fin(&state_updates)
        .map_err(|e| SaltStateRootError::Trie(format!("{e:?}")))?;
    let root_compute_duration = root_start.elapsed();

    let state_root = scalar_to_b256(root_bytes);

    Ok(SaltStateRootOutcome {
        state_root,
        state_updates,
        trie_updates,
        state_update_duration,
        root_compute_duration,
    })
}

/// Convenience function: compute SALT state root using in-memory `MemStore`.
///
/// Creates a fresh `MemStore` and `StateRoot` engine, applies the updates,
/// and returns the outcome. Useful for benchmarks and one-shot testing.
pub fn compute_salt_state_root_mem(
    store: &MemStore,
    bundle_state: &revm_database::BundleState,
) -> Result<SaltStateRootOutcome, SaltStateRootError> {
    let mut salt_root = StateRoot::new(store);
    compute_salt_state_root(store, &mut salt_root, bundle_state)
}

/// Convert a SALT `ScalarBytes` (32 bytes) to an alloy `B256`.
#[inline]
fn scalar_to_b256(scalar: ScalarBytes) -> B256 {
    B256::from(scalar)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::constants::KECCAK_EMPTY;
    use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, U256};
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
    use revm_state::AccountInfo;

    fn make_test_bundle(
        num_accounts: usize,
        slots_per_account: usize,
    ) -> revm_database::BundleState {
        let mut state: PrimitivesHashMap<Address, revm_database::BundleAccount> =
            PrimitivesHashMap::default();

        for i in 0..num_accounts {
            let mut addr_bytes = [0u8; 20];
            addr_bytes[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            let addr = Address::from(addr_bytes);

            let info = AccountInfo {
                nonce: i as u64,
                balance: U256::from(1000 * (i + 1)),
                code_hash: KECCAK_EMPTY,
                account_id: None,
                code: None,
            };

            let mut storage = StorageWithOriginalValues::default();
            for j in 0..slots_per_account {
                let mut slot_bytes = [0u8; 32];
                slot_bytes[0..8].copy_from_slice(&(j as u64).to_be_bytes());
                let slot_key = alloy_primitives::B256::from(slot_bytes);
                storage.insert(
                    slot_key.into(),
                    StorageSlot::new_changed(U256::ZERO, U256::from(j + 1)),
                );
            }

            state.insert(
                addr,
                revm_database::BundleAccount {
                    info: Some(info),
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage,
                },
            );
        }

        revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    #[test]
    fn test_salt_state_root_basic() {
        let store = MemStore::new();
        let bundle = make_test_bundle(5, 3);

        let outcome = compute_salt_state_root_mem(&store, &bundle).unwrap();

        assert_ne!(outcome.state_root, B256::ZERO);
        assert!(!outcome.state_updates.is_empty());
        assert!(!outcome.trie_updates.is_empty());
    }

    #[test]
    fn test_salt_state_root_deterministic() {
        let store1 = MemStore::new();
        let store2 = MemStore::new();
        let bundle = make_test_bundle(10, 5);

        let outcome1 = compute_salt_state_root_mem(&store1, &bundle).unwrap();
        let outcome2 = compute_salt_state_root_mem(&store2, &bundle).unwrap();

        assert_eq!(outcome1.state_root, outcome2.state_root);
    }

    #[test]
    fn test_salt_state_root_incremental() {
        let store = MemStore::new();
        let bundle1 = make_test_bundle(5, 2);

        let mut salt_root = StateRoot::new(&store);
        let outcome1 = compute_salt_state_root(&store, &mut salt_root, &bundle1).unwrap();

        store.update_state(outcome1.state_updates);
        store.update_trie(outcome1.trie_updates);

        let bundle2 = make_test_bundle(3, 1);
        let outcome2 = compute_salt_state_root(&store, &mut salt_root, &bundle2).unwrap();

        assert_ne!(outcome2.state_root, B256::ZERO);
        assert_ne!(outcome1.state_root, outcome2.state_root);
    }
}
