//! `MptDbStateProvider`: reth `StateProvider` backed by mpt-db SC.
//!
//! EVM reads (`basic_account`, `storage`) are delegated to `fallback`, which
//! in production is the reth `BlockchainProvider` default state provider
//! (PlainAccountState / PlainStorageState via MDBX).  SC is used only for
//! state root computation and proof generation.

use alloy_eips::BlockNumHash;
use alloy_primitives::{keccak256, Address, BlockNumber, Bytes, B256};
use mptdb_common::error::MptDbError;
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
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
use std::{
    collections::HashSet,
    sync::{
        mpsc::{sync_channel, SyncSender, TryRecvError},
        Arc, Mutex as StdMutex,
    },
};

fn prov_err(e: impl std::fmt::Display) -> reth_storage_api::errors::provider::ProviderError {
    reth_storage_api::errors::provider::ProviderError::Database(DatabaseError::Other(e.to_string()))
}

fn map_db_err(e: MptDbError) -> reth_storage_api::errors::provider::ProviderError {
    prov_err(e)
}

/// Background dispatcher: receives touched accounts and prewarms SC storage tries.
///
/// Dropping the dispatcher closes the channel (`tx` is dropped), which causes
/// the worker's `rx.recv()` to return `Err` and the thread to exit.  The
/// `JoinHandle` is stored so `Drop` can join the thread and guarantee the
/// worker has fully stopped before the next iteration starts.
pub struct ScPrewarmDispatcher {
    tx: Option<SyncSender<B256>>,
    pending: Arc<StdMutex<HashSet<B256>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ScPrewarmDispatcher {
    fn drop(&mut self) {
        // Close the channel first: this signals the worker to exit.
        drop(self.tx.take());
        // Join the worker so no cross-iteration backlog remains.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl ScPrewarmDispatcher {
    /// Spawn the background prewarm worker.
    ///
    /// The worker exits automatically when the `ScPrewarmDispatcher` is dropped:
    /// dropping the dispatcher drops the only `SyncSender`, which causes `rx.recv()`
    /// to return `Err(_)` and breaks the worker loop.
    /// NOTE: the worker must NOT hold a cloned sender — doing so prevents shutdown.
    pub fn spawn(
        sc: Arc<Mutex<MptCommitStore>>,
        queue_capacity: usize,
        batch_size: usize,
    ) -> std::io::Result<Arc<Self>> {
        let capacity = queue_capacity.max(64);
        let batch = batch_size.max(16);
        let (tx, rx) = sync_channel::<B256>(capacity);
        let pending = Arc::new(StdMutex::new(HashSet::with_capacity(capacity)));
        let pending_for_worker = Arc::clone(&pending);
        // No tx clone in the worker — the dispatcher is the sole sender.
        // Dropping the dispatcher drops tx, closing the channel → worker exits.

        let handle =
            std::thread::Builder::new().name("mptdb-sc-prewarm".to_string()).spawn(move || {
                loop {
                    let first = match rx.recv() {
                        Ok(v) => v,
                        Err(_) => break, // dispatcher dropped → all senders gone → exit
                    };

                    let mut batch_keys = Vec::with_capacity(batch);
                    batch_keys.push(first);
                    while batch_keys.len() < batch {
                        match rx.try_recv() {
                            Ok(v) => batch_keys.push(v),
                            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                        }
                    }

                    // Two-phase processing to avoid holding the SC lock for the
                    // entire batch:
                    //
                    // Phase 1 (brief lock): refresh the published view once for
                    // the whole batch.  This is a fast atomic-read check; the
                    // lock is released immediately after.
                    // Phase 1: check-only view refresh (no I/O, no reload).
                    // Skips reload intentionally — see maybe_refresh_published_view_for_prewarm.
                    if let Some(mut guard) = sc.try_lock() {
                        guard.maybe_refresh_published_view_for_prewarm();
                    }

                    // Phase 2 (one lock per item): each per-key call is fast —
                    // O(1) index check + optional short MPT walk.  Releasing the
                    // lock between items lets SC commit interleave freely.
                    for hashed in &batch_keys {
                        if let Some(mut guard) = sc.try_lock() {
                            let _ = guard.prewarm_storage_trie_by_hashed_address(*hashed);
                            // Guard drops here, lock released before next item.
                        }
                        // If SC is busy, skip this item (best-effort).
                    }

                    let mut pending =
                        pending_for_worker.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    for hashed in batch_keys {
                        pending.remove(&hashed);
                    }
                }
            })?;

        Ok(Arc::new(Self { tx: Some(tx), pending, handle: Some(handle) }))
    }

    pub fn enqueue_address(&self, address: Address) {
        self.enqueue_hashed(keccak256(address.as_slice()));
    }

    pub fn enqueue_hashed(&self, hashed_address: B256) {
        let Some(ref tx) = self.tx else { return };
        {
            let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if !pending.insert(hashed_address) {
                return;
            }
        }
        if tx.try_send(hashed_address).is_err() {
            // Channel full or disconnected: remove from pending so future
            // enqueue attempts are not silently dropped.
            let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.remove(&hashed_address);
        }
    }
}

/// reth `StateProvider` backed by mpt-db SC.
///
/// EVM reads (`basic_account`, `storage`) are served by `fallback` — the reth
/// default state provider injected via `StateProviderOverride`.  In production
/// this is a `BlockchainProvider`-derived view of PlainAccountState /
/// PlainStorageState (MDBX), which is in-memory-aware for un-persisted blocks.
///
/// `state_root` / proof generation delegates to SC (`MptCommitStore`).
pub struct MptDbStateProvider {
    pub sc: Arc<Mutex<MptCommitStore>>,
    /// SC version for this provider = block_number + 1.
    /// Used for proof generation; reads use `fallback` instead.
    pub version: i64,
    /// State provider for EVM reads (basic_account, storage) and non-state
    /// data (bytecode, block hashes).  In production this is the reth engine's
    /// `default_provider` from the `StateProviderOverride` callback.
    pub fallback: Arc<dyn StateProvider + Send + Sync>,
    /// For block_hash → block_number lookups.
    pub block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
}

impl MptDbStateProvider {
    pub fn new(
        sc: Arc<Mutex<MptCommitStore>>,
        version: i64,
        fallback: Arc<dyn StateProvider + Send + Sync>,
        block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    ) -> Self {
        Self { sc, version, fallback, block_id_reader }
    }
}

// ── AccountReader ──────────────────────────────────────────────────────────────

impl AccountReader for MptDbStateProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.fallback.basic_account(address)
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
        // SC maintains only the latest committed state; state_root always runs
        // against the current SC base regardless of self.version.
        //
        // The check below guards against providers created via
        // `history_by_block_number` / `make_provider(N)` where version N ≠
        // sc.version().  In production, `reth-mptdb` gates StateProviderOverride
        // so SC is used only for the latest canonical hash; historical hash
        // requests are delegated to reth's default provider.
        // Full historical state_root correctness in SC would still require
        // per-version snapshots (not yet implemented).
        // Hold a single lock for version check + root computation to avoid
        // TOCTOU races where SC advances between two separate lock acquisitions.
        let mut sc = self.sc.lock();
        let sc_version = sc.version();
        if self.version != sc_version {
            return Err(prov_err(format!(
                "mpt-db: state_root requires version == latest SC version ({sc_version}); \
                 self.version={} — SC has no per-version MPT snapshots",
                self.version
            )));
        }
        // Known correctness risk for cold accounts: `apply_hashed_state_overlay`
        // treats any storage trie not present in SC's handle map as an empty trie
        // (`StorageTrieCow::empty()`).  Accounts that exist in MDBX but were
        // never written to SC will get an incorrect storage root.  Fix requires SC
        // to load historical storage tries on demand when the handle is absent.
        sc.apply_hashed_state_overlay(&hashed_state).map_err(map_db_err)
    }

    fn state_root_with_updates(
        &self,
        hashed_state: HashedPostState,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        // Returns empty TrieUpdates: SC manages its own MPT representation and
        // does not produce MDBX trie-table updates (HashedAccountState,
        // AccountsTrie, StoragesTrie, etc.).
        //
        // Known limitation: reth's execution pipeline uses TrieUpdates to write
        // trie nodes back to MDBX (provider.rs write_trie_updates).  Returning
        // empty updates means MDBX trie tables remain unpopulated.  This is
        // acceptable in the Plan C architecture because:
        //   - EVM reads come from PlainState (not trie tables)
        //   - SC provides state root computation independently
        //   - Code paths that require MDBX trie nodes (e.g. reth's own proof generation, historical
        //     sync) will need to use reth's native path
        let root = self.state_root(hashed_state)?;
        Ok((root, TrieUpdates::default()))
    }

    fn state_root_from_nodes(&self, _input: TrieInput) -> ProviderResult<B256> {
        // Used by reth's parallel state root path (e.g. engine tree speculative
        // execution).  SC does not accept pre-hashed trie node overlays; it
        // computes state root via apply_hashed_state_overlay instead.
        // Callers that need this path must use reth's native MDBX provider.
        Err(prov_err(
            "mpt-db: state_root_from_nodes not supported — \
             use state_root(HashedPostState) instead",
        ))
    }

    fn state_root_from_nodes_with_updates(
        &self,
        _input: TrieInput,
    ) -> ProviderResult<(B256, TrieUpdates)> {
        Err(prov_err(
            "mpt-db: state_root_from_nodes_with_updates not supported — \
             use state_root_with_updates(HashedPostState) instead",
        ))
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
        _targets: MultiProofTargets,
    ) -> ProviderResult<MultiProof> {
        // MultiProofTargets keys are keccak256(address).  SC's account_proof API
        // requires the original Address, and keccak256 is not reversible.
        // Implementing multiproof correctly requires SC to expose a hashed-address
        // proof API.  Return an explicit error until that is available.
        Err(prov_err(
            "mpt-db: multiproof not yet supported (MultiProofTargets keys are \
             keccak256(address) but SC proof API requires raw Address)",
        ))
    }

    fn witness(
        &self,
        _input: TrieInput,
        _target: HashedPostState,
        _mode: reth_trie_common::ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
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
        self.fallback.storage(account, storage_key)
    }
}

// ── SyncProvider ──────────────────────────────────────────────────────────────
//
// Wraps `StateProviderBox` (`Box<dyn StateProvider + Send>`) in a `Mutex` to
// satisfy `Arc<dyn StateProvider + Send + Sync>`.  Used by
// `MptDbStateProviderFactory::make_provider` to build version-specific
// historical fallbacks from `historical_fallback_factory`.
//
// All calls lock the Mutex then delegate; since historical providers are
// accessed sequentially (one RPC request at a time), the Mutex is
// uncontended in practice.

pub struct SyncProvider(pub Mutex<reth_storage_api::StateProviderBox>);

impl SyncProvider {
    pub fn new(p: reth_storage_api::StateProviderBox) -> Arc<Self> {
        Arc::new(Self(Mutex::new(p)))
    }
}

impl AccountReader for SyncProvider {
    fn basic_account(&self, address: &Address) -> ProviderResult<Option<Account>> {
        self.0.lock().basic_account(address)
    }
}
impl BlockHashReader for SyncProvider {
    fn block_hash(&self, n: BlockNumber) -> ProviderResult<Option<B256>> {
        self.0.lock().block_hash(n)
    }
    fn canonical_hashes_range(&self, s: BlockNumber, e: BlockNumber) -> ProviderResult<Vec<B256>> {
        self.0.lock().canonical_hashes_range(s, e)
    }
}
impl reth_storage_api::BytecodeReader for SyncProvider {
    fn bytecode_by_hash(&self, h: &B256) -> ProviderResult<Option<Bytecode>> {
        self.0.lock().bytecode_by_hash(h)
    }
}
impl StateRootProvider for SyncProvider {
    fn state_root(&self, h: HashedPostState) -> ProviderResult<B256> {
        self.0.lock().state_root(h)
    }
    fn state_root_from_nodes(&self, i: TrieInput) -> ProviderResult<B256> {
        self.0.lock().state_root_from_nodes(i)
    }
    fn state_root_with_updates(
        &self,
        h: HashedPostState,
    ) -> ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)> {
        self.0.lock().state_root_with_updates(h)
    }
    fn state_root_from_nodes_with_updates(
        &self,
        i: TrieInput,
    ) -> ProviderResult<(B256, reth_trie_common::updates::TrieUpdates)> {
        self.0.lock().state_root_from_nodes_with_updates(i)
    }
}
impl StorageRootProvider for SyncProvider {
    fn storage_root(&self, a: Address, h: reth_trie_common::HashedStorage) -> ProviderResult<B256> {
        self.0.lock().storage_root(a, h)
    }
    fn storage_proof(
        &self,
        a: Address,
        s: B256,
        h: reth_trie_common::HashedStorage,
    ) -> ProviderResult<StorageProof> {
        self.0.lock().storage_proof(a, s, h)
    }
    fn storage_multiproof(
        &self,
        a: Address,
        slots: &[B256],
        h: reth_trie_common::HashedStorage,
    ) -> ProviderResult<StorageMultiProof> {
        self.0.lock().storage_multiproof(a, slots, h)
    }
}
impl StateProofProvider for SyncProvider {
    fn proof(&self, i: TrieInput, a: Address, s: &[B256]) -> ProviderResult<AccountProof> {
        self.0.lock().proof(i, a, s)
    }
    fn multiproof(&self, i: TrieInput, t: MultiProofTargets) -> ProviderResult<MultiProof> {
        self.0.lock().multiproof(i, t)
    }
    fn witness(
        &self,
        i: TrieInput,
        t: HashedPostState,
        m: reth_trie_common::ExecutionWitnessMode,
    ) -> ProviderResult<Vec<Bytes>> {
        self.0.lock().witness(i, t, m)
    }
}
impl HashedPostStateProvider for SyncProvider {
    fn hashed_post_state(&self, b: &revm_database::BundleState) -> HashedPostState {
        self.0.lock().hashed_post_state(b)
    }
}
impl StateProvider for SyncProvider {
    fn storage(
        &self,
        account: Address,
        storage_key: alloy_primitives::StorageKey,
    ) -> ProviderResult<Option<alloy_primitives::StorageValue>> {
        self.0.lock().storage(account, storage_key)
    }
}
