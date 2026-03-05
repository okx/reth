//! QMDB-backed implementation of reth's `StateProvider` trait.
//!
//! Reads account/storage/bytecode from QMDB. Delegates block hash queries
//! to an optional MDBX-backed provider. Proof/trie methods return stubs
//! since QMDB uses SHA-256 roots internally.

use crate::store::QmdbStore;
use alloy_primitives::{Address, BlockNumber, Bytes, StorageKey, StorageValue, B256};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    AccountReader, BlockHashReader, BytecodeReader, HashedPostStateProvider, StateProofProvider,
    StateProvider, StateRootProvider, StorageRootProvider,
};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use reth_trie_common::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use revm_database::BundleState;
use std::sync::Arc;

/// A `StateProvider` backed by QMDB for state reads.
///
/// Block hash lookups are delegated to an optional inner provider (typically MDBX).
/// Trie proof methods are stubbed out since QMDB computes SHA-256 roots internally.
pub struct QmdbStateProvider {
    store: Arc<QmdbStore>,
    /// Optional delegate for block hash queries.
    block_hash_provider: Option<Box<dyn BlockHashReader + Send + Sync>>,
}

impl std::fmt::Debug for QmdbStateProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbStateProvider").field("store", &self.store).finish_non_exhaustive()
    }
}

impl QmdbStateProvider {
    /// Create a new `QmdbStateProvider` wrapping the given store.
    pub fn new(store: Arc<QmdbStore>) -> Self {
        Self { store, block_hash_provider: None }
    }

    /// Create a new `QmdbStateProvider` with a block hash delegate.
    pub fn with_block_hash_provider(
        store: Arc<QmdbStore>,
        block_hash_provider: Box<dyn BlockHashReader + Send + Sync>,
    ) -> Self {
        Self { store, block_hash_provider: Some(block_hash_provider) }
    }
}

impl AccountReader for QmdbStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        Ok(self.store.read_account(address))
    }
}

impl BytecodeReader for QmdbStateProvider {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        Ok(self.store.read_bytecode(code_hash))
    }
}

impl BlockHashReader for QmdbStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        if let Some(ref provider) = self.block_hash_provider {
            provider.block_hash(number)
        } else {
            Ok(None)
        }
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        if let Some(ref provider) = self.block_hash_provider {
            provider.canonical_hashes_range(start, end)
        } else {
            Ok(vec![])
        }
    }
}

impl StateRootProvider for QmdbStateProvider {
    fn state_root(&self, _hashed_state: HashedPostState) -> ProviderResult<B256> {
        Ok(self.store.state_root())
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Ok(self.store.state_root())
    }

    fn state_root_with_updates(
        &self,
        _hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((self.store.state_root(), TrieUpdates::default()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Ok((self.store.state_root(), TrieUpdates::default()))
    }
}

impl StorageRootProvider for QmdbStateProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        // QMDB does not support per-account storage roots.
        Ok(B256::ZERO)
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl StateProofProvider for QmdbStateProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(ProviderError::UnsupportedProvider)
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        Err(ProviderError::UnsupportedProvider)
    }
}

impl HashedPostStateProvider for QmdbStateProvider {
    fn hashed_post_state(&self, bundle_state: &BundleState) -> HashedPostState {
        // QMDB doesn't use hashed keys, but this is required by the trait.
        // Return an empty hashed post state since QMDB handles state internally.
        let _ = bundle_state;
        HashedPostState::default()
    }
}

impl StateProvider for QmdbStateProvider {
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        Ok(self.store.read_storage(&account, &storage_key))
    }
}
