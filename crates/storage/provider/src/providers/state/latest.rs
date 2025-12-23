use crate::{
    providers::state::macros::delegate_provider_impls, AccountReader, BlockHashReader,
    HashedPostStateProvider, StateProvider, StateRootProvider,
};
use alloy_primitives::{Address, BlockNumber, Bytes, StorageKey, StorageValue, B256, U256};
use reth_db_api::{cursor::DbDupCursorRO, tables, transaction::DbTx};
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{BytecodeReader, DBProvider, PlainPostState, StateProofProvider, StorageRootProvider};
use reth_storage_errors::provider::{ProviderError, ProviderResult};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use reth_trie::{
    proof::{Proof, StorageProof},
    updates::TrieUpdates,
    witness::TrieWitness,
    AccountProof, HashedPostState, HashedStorage, KeccakKeyHasher, MultiProof, MultiProofTargets,
    StateRoot, StorageMultiProof, StorageRoot, TrieInput,
};
use reth_trie_db::{
    DatabaseProof, DatabaseStateRoot, DatabaseStorageProof, DatabaseStorageRoot,
    DatabaseTrieWitness,
};
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_trie::EMPTY_ROOT_HASH;
use triedb::{
    account::Account as TrieDBAccount,
    overlay::{OverlayStateMut, OverlayValue},
    path::{AddressPath, StoragePath},
};

/// Static storage for the triedb provider instance
static TRIEDB_PROVIDER: OnceLock<Arc<crate::providers::triedb::TriedbProvider>> = OnceLock::new();

/// Initialize the static triedb provider
pub fn set_triedb_provider(provider: Arc<crate::providers::triedb::TriedbProvider>) -> Result<(), Arc<crate::providers::triedb::TriedbProvider>> {
    TRIEDB_PROVIDER.set(provider)
}

/// Get the static triedb provider
pub fn get_triedb_provider() -> Option<&'static Arc<crate::providers::triedb::TriedbProvider>> {
    TRIEDB_PROVIDER.get()
}

/// State provider over latest state that takes tx reference.
///
/// Wraps a [`DBProvider`] to get access to database.
#[derive(Debug)]
pub struct LatestStateProviderRef<'b, Provider>(&'b Provider);

impl<'b, Provider: DBProvider> LatestStateProviderRef<'b, Provider> {
    /// Create new state provider
    pub const fn new(provider: &'b Provider) -> Self {
        Self(provider)
    }

    fn tx(&self) -> &Provider::Tx {
        self.0.tx_ref()
    }
}

impl<Provider: DBProvider> AccountReader for LatestStateProviderRef<'_, Provider> {
    /// Get basic account information.
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.tx().get_by_encoded_key::<tables::PlainAccountState>(address).map_err(Into::into)
    }
}

impl<Provider: BlockHashReader> BlockHashReader for LatestStateProviderRef<'_, Provider> {
    /// Get block hash by number.
    fn block_hash(&self, number: u64) -> ProviderResult<Option<B256>> {
        self.0.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.0.canonical_hashes_range(start, end)
    }
}

impl<Provider: DBProvider + Sync> StateRootProvider for LatestStateProviderRef<'_, Provider> {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        StateRoot::overlay_root(self.tx(), hashed_state)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn state_root_from_nodes(&self, input: TrieInput) -> ProviderResult<B256> {
        StateRoot::overlay_root_from_nodes(self.tx(), input)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        StateRoot::overlay_root_with_updates(self.tx(), hashed_state)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        StateRoot::overlay_root_from_nodes_with_updates(self.tx(), input)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn state_root_with_updates_triedb(
        &self,
        plain_state: PlainPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        tracing::info!("latest_state_provider state_root_with_updates_triedb");
        let triedb_provider = get_triedb_provider()
            .ok_or_else(|| ProviderError::UnsupportedProvider)?;
        let start = Instant::now();
        let mut overlay_mut = OverlayStateMut::new();
        
        for (address, account_opt) in &plain_state.accounts {
            let address_path = AddressPath::for_address(*address);
            
            if let Some(account) = account_opt {
                let trie_account = TrieDBAccount::new(
                    account.nonce,
                    account.balance,
                    EMPTY_ROOT_HASH, // Storage root will be computed from storage overlay
                    account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
                );
                overlay_mut.insert(address_path.clone().into(), Some(OverlayValue::Account(trie_account)));
            } else {
                // Account is being destroyed
                overlay_mut.insert(address_path.clone().into(), None);
            }
        }
        
        for (address, storage) in &plain_state.storages {
            let address_path = AddressPath::for_address(*address);
            
            for (storage_key, storage_value) in storage {
                let raw_slot = U256::from_be_slice(storage_key.as_slice());
                let storage_path = StoragePath::for_address_path_and_slot(
                    address_path.clone(),
                    StorageKey::from(raw_slot),
                );
                
                if storage_value.is_zero() {
                    overlay_mut.insert(storage_path.clone().into(), None);
                } else {
                    overlay_mut.insert(
                        storage_path.clone().into(),
                        Some(OverlayValue::Storage(StorageValue::from_be_slice(
                            storage_value.to_be_bytes::<32>().as_slice()
                        ))),
                    );
                }
            }
        }
        
        let overlay = overlay_mut.freeze();
        let elapsed = start.elapsed().as_millis();
        tracing::info!("latest_state_provider overlay prepare elapsed: {elapsed:?}");

        let start = Instant::now();
        let mut tx = triedb_provider.inner.begin_ro()
            .map_err(|e| ProviderError::TrieWitnessError(format!("Failed to begin triedb transaction: {e:?}")))?;

        let result = tx.compute_root_with_overlay(overlay)
            .map_err(|e| ProviderError::TrieWitnessError(format!("Failed to compute triedb root: {e:?}")))?;
        let elapsed = start.elapsed().as_millis();
        tracing::info!("latest_state_provider compute_root_with_overlay elapsed: {elapsed:?}");

        let start = Instant::now();
        tx.commit()
            .map_err(|e| ProviderError::TrieWitnessError(format!("Failed to commit triedb transaction: {e:?}")))?;
        let elapsed = start.elapsed().as_millis();
        tracing::info!("latest_state_provider commit elapsed: {elapsed:?}");
        Ok((result.root, TrieUpdates::default()))
    }
}


impl<Provider: DBProvider + Sync> StorageRootProvider for LatestStateProviderRef<'_, Provider> {
    fn storage_root(
        &self,
        address: Address,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        StorageRoot::overlay_root(self.tx(), address, hashed_storage)
            .map_err(|err| ProviderError::Database(err.into()))
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        hashed_storage: HashedStorage,
    ) -> ProviderResult<reth_trie::StorageProof> {
        StorageProof::overlay_storage_proof(self.tx(), address, slot, hashed_storage)
            .map_err(ProviderError::from)
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        StorageProof::overlay_storage_multiproof(self.tx(), address, slots, hashed_storage)
            .map_err(ProviderError::from)
    }
}

impl<Provider: DBProvider + Sync> StateProofProvider for LatestStateProviderRef<'_, Provider> {
    fn proof(
        &self,
        input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        let proof = <Proof<_, _> as DatabaseProof>::from_tx(self.tx());
        proof.overlay_account_proof(input, address, slots).map_err(ProviderError::from)
    }

    fn multiproof(
        &self,
        input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        let proof = <Proof<_, _> as DatabaseProof>::from_tx(self.tx());
        proof.overlay_multiproof(input, targets).map_err(ProviderError::from)
    }

    fn witness(&self, input: TrieInput, target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        TrieWitness::overlay_witness(self.tx(), input, target)
            .map_err(ProviderError::from)
            .map(|hm| hm.into_values().collect())
    }
}

impl<Provider: DBProvider + Sync> HashedPostStateProvider for LatestStateProviderRef<'_, Provider> {
    fn hashed_post_state(&self, bundle_state: &revm_database::BundleState) -> HashedPostState {
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle_state.state())
    }
}

impl<Provider: DBProvider + BlockHashReader> StateProvider
    for LatestStateProviderRef<'_, Provider>
{
    /// Get storage.
    fn storage(
        &self,
        account: Address,
        storage_key: StorageKey,
    ) -> ProviderResult<Option<StorageValue>> {
        let mut cursor = self.tx().cursor_dup_read::<tables::PlainStorageState>()?;
        if let Some(entry) = cursor.seek_by_key_subkey(account, storage_key)? &&
            entry.key == storage_key
        {
            return Ok(Some(entry.value))
        }
        Ok(None)
    }
}

impl<Provider: DBProvider + BlockHashReader> BytecodeReader
    for LatestStateProviderRef<'_, Provider>
{
    /// Get account code by its hash
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.tx().get_by_encoded_key::<tables::Bytecodes>(code_hash).map_err(Into::into)
    }
}

/// Trait for accessing TrieDB provider
pub trait TriedbProviderAccess {
    /// Returns reference to TrieDB provider if available
    fn triedb_provider(&self) -> Option<&Arc<crate::providers::triedb::TriedbProvider>>;
}

/// State provider for the latest state.
#[derive(Debug)]
pub struct LatestStateProvider<Provider>(Provider);

impl<Provider: DBProvider> LatestStateProvider<Provider> {
    /// Create new state provider
    pub const fn new(db: Provider) -> Self {
        Self(db)
    }

    /// Returns a new provider that takes the `TX` as reference
    #[inline(always)]
    const fn as_ref(&self) -> LatestStateProviderRef<'_, Provider> {
        LatestStateProviderRef::new(&self.0)
    }

    /// Returns reference to TrieDB provider if available
    pub fn triedb_provider(&self) -> Option<&Arc<crate::providers::triedb::TriedbProvider>>
    where
        Provider: TriedbProviderAccess,
    {
        self.0.triedb_provider()
    }
}

// Delegates all provider impls to [LatestStateProviderRef]
delegate_provider_impls!(LatestStateProvider<Provider> where [Provider: DBProvider + BlockHashReader ]);

#[cfg(test)]
mod tests {
    use super::*;

    const fn assert_state_provider<T: StateProvider>() {}
    #[expect(dead_code)]
    const fn assert_latest_state_provider<T: DBProvider + BlockHashReader>() {
        assert_state_provider::<LatestStateProvider<T>>();
    }
}
