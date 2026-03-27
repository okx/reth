//! `MptDbStateProvider`: reth `StateProvider` backed by mpt-db SS + SC.

use alloy_eips::BlockNumHash;
use alloy_primitives::{keccak256, Address, BlockHash, BlockNumber, Bytes, B256, U256};
use mptdb_common::error::MptDbError;
use mptdb_sc::mpt::MptCommitStore;
use mptdb_ss::evm::store::EVMStateStore;
use parking_lot::Mutex;
use reth_chainspec::ChainInfo;
use reth_primitives_traits::{Account, Bytecode};
use reth_storage_api::{
    errors::{db::DatabaseError, provider::ProviderResult},
    AccountReader, BlockHashReader, BlockIdReader, BlockNumReader, BytecodeReader,
    HashedPostStateProvider, StateProofProvider, StateProvider, StateRootProvider,
    StorageRootProvider,
};
use reth_trie_common::{
    updates::TrieUpdates, AccountProof, HashedPostState, HashedStorage, MultiProof,
    MultiProofTargets, StorageMultiProof, StorageProof, TrieInput,
};
use std::sync::Arc;

fn prov_err(e: impl std::fmt::Display) -> reth_storage_api::errors::provider::ProviderError {
    reth_storage_api::errors::provider::ProviderError::Database(DatabaseError::Other(e.to_string()))
}

fn map_db_err(e: MptDbError) -> reth_storage_api::errors::provider::ProviderError {
    prov_err(e)
}

/// reth `StateProvider` backed by mpt-db.
pub struct MptDbStateProvider {
    pub ss: Arc<EVMStateStore>,
    pub sc: Arc<Mutex<MptCommitStore>>,
    /// version = block_number as i64
    pub version: i64,
    pub fallback: Arc<dyn StateProvider + Send + Sync>,
    pub block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
}

impl MptDbStateProvider {
    pub fn new(
        ss: Arc<EVMStateStore>,
        sc: Arc<Mutex<MptCommitStore>>,
        version: i64,
        fallback: Arc<dyn StateProvider + Send + Sync>,
        block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    ) -> Self {
        Self { ss, sc, version, fallback, block_id_reader }
    }
}

// ── AccountReader ──────────────────────────────────────────────────────────────

impl AccountReader for MptDbStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        let addr_bytes: [u8; 20] = address.into_array();
        match self.ss.get_account(self.version, &addr_bytes) {
            Ok(None) => Ok(None),
            Ok(Some((nonce, balance_bytes, code_hash_bytes))) => {
                let balance = U256::from_be_bytes(balance_bytes);
                let code_hash = B256::from(code_hash_bytes);
                let keccak_empty = keccak256([]);
                Ok(Some(Account {
                    nonce,
                    balance,
                    bytecode_hash: if code_hash == keccak_empty { None } else { Some(code_hash) },
                }))
            }
            Err(e) => Err(map_db_err(e)),
        }
    }
}

// ── BlockNumReader ─────────────────────────────────────────────────────────────

impl BlockNumReader for MptDbStateProvider {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        self.block_id_reader.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(self.version.max(0) as u64)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.block_id_reader.last_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        self.block_id_reader.block_number(hash)
    }
}

// ── BlockHashReader ────────────────────────────────────────────────────────────

impl BlockHashReader for MptDbStateProvider {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.fallback.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.fallback.canonical_hashes_range(start, end)
    }
}

// ── BlockIdReader ──────────────────────────────────────────────────────────────

impl BlockIdReader for MptDbStateProvider {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.block_id_reader.pending_block_num_hash()
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.block_id_reader.safe_block_num_hash()
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.block_id_reader.finalized_block_num_hash()
    }
}

// ── BytecodeReader ─────────────────────────────────────────────────────────────

impl BytecodeReader for MptDbStateProvider {
    fn bytecode_by_hash(&self, code_hash: &B256) -> ProviderResult<Option<Bytecode>> {
        self.fallback.bytecode_by_hash(code_hash)
    }
}

// ── StateRootProvider ─────────────────────────────────────────────────────────

impl StateRootProvider for MptDbStateProvider {
    fn state_root(&self, hashed_state: HashedPostState) -> ProviderResult<B256> {
        self.sc.lock().apply_hashed_state_overlay(&hashed_state).map_err(map_db_err)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        let root = self.state_root(hashed_state)?;
        Ok((root, TrieUpdates::default()))
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        Err(prov_err("mpt-db: state_root_from_nodes not supported (Phase 1)"))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(prov_err("mpt-db: state_root_from_nodes_with_updates not supported (Phase 1)"))
    }
}

// ── StorageRootProvider (Phase 3 stub) ────────────────────────────────────────

impl StorageRootProvider for MptDbStateProvider {
    fn storage_root(
        &self,
        _address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        Err(prov_err("mpt-db: StorageRootProvider not yet implemented"))
    }

    fn storage_proof(
        &self,
        _address: Address,
        _slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        Err(prov_err("mpt-db: storage_proof not yet implemented"))
    }

    fn storage_multiproof(
        &self,
        _address: Address,
        _slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        Err(prov_err("mpt-db: storage_multiproof not yet implemented"))
    }
}

// ── StateProofProvider (Phase 3 stub) ─────────────────────────────────────────

impl StateProofProvider for MptDbStateProvider {
    fn proof(
        &self,
        _input: TrieInput,
        _address: Address,
        _slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        Err(prov_err("mpt-db: StateProofProvider not yet implemented"))
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        Err(prov_err("mpt-db: multiproof not yet implemented"))
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        Err(prov_err("mpt-db: witness not yet implemented"))
    }
}

// ── HashedPostStateProvider ───────────────────────────────────────────────────

impl HashedPostStateProvider for MptDbStateProvider {
    fn hashed_post_state(&self, bundle_state: &revm_database::BundleState) -> HashedPostState {
        // Sequential implementation to avoid rayon feature dependency on HashMap.
        let mut hps = HashedPostState::default();
        for (address, account) in &bundle_state.state {
            let hashed_addr = keccak256(address.as_slice());
            hps.accounts.insert(hashed_addr, account.info.as_ref().map(|i| i.into()));
            let storage = HashedStorage::from_plain_storage(
                account.status,
                account.storage.iter().map(|(slot, val)| (slot, &val.present_value)),
            );
            if !storage.is_empty() {
                hps.storages.insert(hashed_addr, storage);
            }
        }
        hps
    }
}

// ── StateProvider ──────────────────────────────────────────────────────────────

impl StateProvider for MptDbStateProvider {
    fn storage(
        &self,
        account: Address,
        storage_key: alloy_primitives::StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        let addr_bytes: [u8; 20] = account.into_array();
        let slot_bytes: [u8; 32] = storage_key.into();
        match self.ss.get_storage(self.version, &addr_bytes, &slot_bytes) {
            Ok(None) => Ok(None),
            Ok(Some(raw)) => {
                // SS storage values are stored as raw 32-byte big-endian U256
                // (from bundle_to_ss_changeset → to_be_bytes()).
                if raw.len() != 32 {
                    return Err(prov_err(format!("unexpected SS storage value len: {}", raw.len())));
                }
                let mut bytes = [0u8; 32];
                bytes.copy_from_slice(&raw);
                let value = U256::from_be_bytes(bytes);
                Ok(if value.is_zero() { None } else { Some(value) })
            }
            Err(e) => Err(map_db_err(e)),
        }
    }
}
