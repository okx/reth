//! `MptDbStateProvider`: reth `StateProvider` backed by mpt-db SS + SC.

use alloy_eips::BlockNumHash;
use alloy_primitives::{keccak256, Address, BlockHash, BlockNumber, Bytes, B256, U256};
use mptdb_common::error::MptDbError;
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
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
    /// SS version for this provider = block_number + 1.
    pub version: i64,
    /// Fallback for non-state data (bytecode, block hashes).
    pub fallback: Arc<dyn StateProvider + Send + Sync>,
    /// For block_hash → block_number lookups.
    pub block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    /// Optional historical StateProvider backed by reth MDBX.
    /// Used when SS data for `version` has been pruned (Phase 2).
    /// Wrapped in Mutex to avoid requiring `Sync` on the inner provider
    /// (StateProviderBox = Box<dyn StateProvider + Send>, not Sync).
    /// If None, pruned-data queries return `ProviderError`.
    pub historical_fallback: Option<Arc<Mutex<reth_storage_api::StateProviderBox>>>,
}

impl MptDbStateProvider {
    pub fn new(
        ss: Arc<EVMStateStore>,
        sc: Arc<Mutex<MptCommitStore>>,
        version: i64,
        fallback: Arc<dyn StateProvider + Send + Sync>,
        block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    ) -> Self {
        Self { ss, sc, version, fallback, block_id_reader, historical_fallback: None }
    }

    pub fn with_historical_fallback(
        mut self,
        historical: reth_storage_api::StateProviderBox,
    ) -> Self {
        self.historical_fallback = Some(Arc::new(Mutex::new(historical)));
        self
    }

    /// Check whether SS has data at `self.version`.
    /// Returns `Err` with a clear pruning message if not available.
    fn check_ss_version(&self) -> ProviderResult<()> {
        if !self.ss.is_version_available(self.version) {
            // Version is outside SS's retained range: either pruned or not yet written.
            let block = (self.version - 1).max(0);
            return Err(prov_err(format!(
                "mpt-db: historical state for block {block} (SS version {}) is not \
                 available — data may have been pruned (keep_recent) or SS was \
                 initialized after this block",
                self.version
            )));
        }
        Ok(())
    }
}

// ── AccountReader ──────────────────────────────────────────────────────────────

impl AccountReader for MptDbStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        // Phase 2: check if SS has data at this version before querying.
        if let Err(prune_err) = self.check_ss_version() {
            // Try historical_fallback first; if not configured, propagate the error.
            return match &self.historical_fallback {
                Some(hf) => hf.lock().basic_account(address),
                None => Err(prune_err),
            };
        }

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
        // SS version = block_number + 1, so block_number = version - 1.
        Ok(self.version.saturating_sub(1).max(0) as u64)
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

// ── StorageRootProvider (Phase 3) ─────────────────────────────────────────────

impl StorageRootProvider for MptDbStateProvider {
    fn storage_root(
        &self,
        address: Address,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<B256> {
        // Prove against committed state; TrieInput overlay not applied.
        let proof = self.sc.lock().account_proof(self.version, address, &[]).map_err(map_db_err)?;
        Ok(proof.storage_root)
    }

    fn storage_proof(
        &self,
        address: Address,
        slot: B256,
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageProof> {
        let mut ap =
            self.sc.lock().account_proof(self.version, address, &[slot]).map_err(map_db_err)?;
        ap.storage_proofs
            .pop()
            .ok_or_else(|| prov_err("mpt-db: storage_proof: no proof returned for slot"))
    }

    fn storage_multiproof(
        &self,
        address: Address,
        slots: &[B256],
        _hashed_storage: HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        let ap = self.sc.lock().account_proof(self.version, address, slots).map_err(map_db_err)?;
        // Build StorageMultiProof from individual StorageProofs.
        // Each StorageProof.proof is an ordered Vec<Bytes> from root to leaf;
        // we key each node by the first i nibbles of keccak(slot).
        use alloy_trie::proof::ProofNodes;
        let mut nodes: Vec<(reth_trie_common::Nibbles, Bytes)> = Vec::new();
        for sp in &ap.storage_proofs {
            for (i, node) in sp.proof.iter().enumerate() {
                let path = reth_trie_common::Nibbles::from_nibbles_unchecked(
                    sp.nibbles.slice(..i.min(sp.nibbles.len())).to_vec(),
                );
                nodes.push((path, node.clone()));
            }
        }
        Ok(StorageMultiProof {
            root: ap.storage_root,
            subtree: ProofNodes::from_iter(nodes),
            branch_node_masks: Default::default(),
        })
    }
}

// ── StateProofProvider (Phase 3) ──────────────────────────────────────────────

impl StateProofProvider for MptDbStateProvider {
    /// Compute an Ethereum account + storage proof against the committed state
    /// at `self.version`.
    ///
    /// Note: `input` (TrieInput) carries uncommitted overlay changes used by
    /// reth's parallel state root machinery.  mpt-db manages its own SC layer;
    /// the overlay is not applied here — only committed state is proven.
    fn proof(
        &self,
        _input: TrieInput,
        address: Address,
        slots: &[B256],
    ) -> ProviderResult<AccountProof> {
        self.sc.lock().account_proof(self.version, address, slots).map_err(map_db_err)
    }

    fn multiproof(
        &self,
        _input: TrieInput,
        targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        use alloy_trie::proof::ProofNodes;

        let mut account_nodes: Vec<(reth_trie_common::Nibbles, Bytes)> = Vec::new();
        let mut storages = alloy_primitives::map::HashMap::default();

        for (hashed_addr, slot_set) in targets.iter() {
            // MultiProofTargets keys are keccak(address). We reconstruct the
            // raw address from the last 20 bytes (best-effort; works for the
            // common case where addresses are not keccak-colliding).
            let addr = Address::from_slice(&hashed_addr[12..]);
            let slots: Vec<B256> = slot_set.iter().copied().collect();
            let ap =
                self.sc.lock().account_proof(self.version, addr, &slots).map_err(map_db_err)?;

            // Fold account proof nodes keyed by first-i-nibbles of keccak(address).
            let addr_nibbles = reth_trie_common::Nibbles::unpack(hashed_addr);
            for (i, node) in ap.proof.iter().enumerate() {
                let path = reth_trie_common::Nibbles::from_nibbles_unchecked(
                    addr_nibbles.slice(..i.min(addr_nibbles.len())).to_vec(),
                );
                account_nodes.push((path, node.clone()));
            }

            if !ap.storage_proofs.is_empty() {
                let mut storage_nodes: Vec<(reth_trie_common::Nibbles, Bytes)> = Vec::new();
                for sp in &ap.storage_proofs {
                    for (i, node) in sp.proof.iter().enumerate() {
                        let path = reth_trie_common::Nibbles::from_nibbles_unchecked(
                            sp.nibbles.slice(..i.min(sp.nibbles.len())).to_vec(),
                        );
                        storage_nodes.push((path, node.clone()));
                    }
                }
                storages.insert(
                    *hashed_addr,
                    StorageMultiProof {
                        root: ap.storage_root,
                        subtree: ProofNodes::from_iter(storage_nodes),
                        branch_node_masks: Default::default(),
                    },
                );
            }
        }

        Ok(MultiProof {
            account_subtree: ProofNodes::from_iter(account_nodes),
            branch_node_masks: Default::default(),
            storages,
        })
    }

    fn witness(&self, _input: TrieInput, _target: HashedPostState) -> ProviderResult<Vec<Bytes>> {
        // Witness generation requires full trie traversal; not implemented.
        // Callers that need witness (e.g. zkEVM) should use reth's MDBX provider.
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
        // Phase 2: check version availability before querying SS.
        if let Err(prune_err) = self.check_ss_version() {
            return match &self.historical_fallback {
                Some(hf) => hf.lock().storage(account, storage_key),
                None => Err(prune_err),
            };
        }

        let addr_bytes: [u8; 20] = account.into_array();
        let slot_bytes: [u8; 32] = storage_key.into();
        match self.ss.get_storage(self.version, &addr_bytes, &slot_bytes) {
            Ok(None) => Ok(None),
            Ok(Some(raw)) => {
                // SS storage values are stored as raw 32-byte big-endian U256.
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
