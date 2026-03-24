use alloy_primitives::{Address, B256};
use alloy_rlp::Encodable;
use alloy_trie::{Nibbles, EMPTY_ROOT_HASH};
use fs4::fs_std::FileExt;
use mptdb_common::error::{MptDbError, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use reth_trie_common::AccountProof;
use revm_database::BundleState;
use schnellru::{ByLength, LruMap};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use super::{
    config::MptConfig,
    gc,
    manifest::VersionManifest,
    parallel::ParallelismThresholds,
    persisted::{self, PersistedTrieStore},
    proof,
    published_baseline::{
        BulkSegmentWriter, IoRateLimiter, PublishedBaselineManager, PublishedBaselineMeta,
        PublishedBaselineReader,
    },
    r#trait::{CommitFrontier, MptCommitter, MptGcStats, MptSnapshotExporter, MptSnapshotImporter},
    segment::StorageTrieSegment,
    snapshot::{SnapshotExporter, SnapshotImporter},
    state::{self, DirtyAccount},
    storage_cow::{CowRootRef, StorageTrieCow},
    tree::MptTree,
    tree_algo,
    wal::{CommitWalAccountChange, CommitWalEntry, CommitWalStore},
};

#[cfg(test)]
use super::storage_cow::CowLazyNodeRef;

#[cfg(test)]
use alloy_primitives::U256;

/// Test-only failure injection points for deterministic failure testing.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFailPoint {
    BeforePersist,
    AfterPersistBeforeManifest,
    ManifestSave,
}

/// Intermediate result from parallel storage trie root computation.
struct StorageTrieCommitArtifacts {
    hashed_address: B256,
    storage_root: B256,
    node_blobs: Vec<(B256, Vec<u8>)>,
    publish_view: StorageTriePublishView,
    hash_elapsed: Duration,
    segment_elapsed: Duration,
    /// The working trie after root computation, returned for cross-block caching.
    trie: StorageTrieCow,
}

enum StorageTriePublishView {
    DeferredRoot(B256),
}

#[derive(Clone)]
struct StorageTrieHandle {
    base: StorageTrieCow,
    base_version: i64,
    working: Option<StorageTrieCow>,
    working_version: Option<i64>,
    /// Pre-computed root hash + committed trie from merged apply+hash phase.
    /// Set during apply_dirty_accounts_inner (wal_first mode), consumed in commit.
    /// Eliminates the second cache load of trie data between phases.
    pre_computed: Option<(B256, StorageTrieCow)>,
}

impl StorageTrieHandle {
    fn activate(base_version: i64, working_version: i64, trie: StorageTrieCow) -> Self {
        Self {
            base: trie.clone(),
            base_version,
            working: Some(trie),
            working_version: Some(working_version),
            pre_computed: None,
        }
    }

    fn snapshot(base_version: i64, trie: StorageTrieCow) -> Self {
        Self { base: trie, base_version, working: None, working_version: None, pre_computed: None }
    }

    /// Whether this handle has a working copy (eager, lazy-deferred, or pre-computed).
    fn has_working(&self) -> bool {
        self.working.is_some() || self.working_version.is_some() || self.pre_computed.is_some()
    }

    fn has_working_for_version(&self, version: i64) -> bool {
        self.working_version == Some(version) || self.pre_computed.is_some()
    }

    /// Lazy checkout: records the intent to write but does NOT clone the base.
    ///
    /// The actual clone is deferred to `take_working_or_base_for_version`,
    /// which runs inside a rayon parallel section.  This turns 5000 sequential
    /// heap allocations (30K HashMap/Vec allocs × ~25μs each) into parallel
    /// work, matching sei-db's model where checkout is O(1) and the COW cost
    /// is paid on the write path.
    fn checkout_for_write(&mut self, working_version: i64) {
        if self.working_version == Some(working_version) {
            return;
        }
        // Just record the version — defer base.clone() to the parallel path.
        self.working_version = Some(working_version);
    }

    fn set_committed_base(&mut self, committed_version: i64, mut trie: StorageTrieCow) {
        self.working = None;
        self.pre_computed = None;
        trie.clear_pending_lazy();
        self.base = trie;
        self.base.snapshot();
        self.base_version = committed_version;
        self.working_version = None;
    }

    fn restore_working(&mut self, working_version: i64, trie: StorageTrieCow) {
        self.working = Some(trie);
        self.working_version = Some(working_version);
        self.pre_computed = None;
    }

    fn take_working_for_version(&mut self, version: i64) -> Option<StorageTrieCow> {
        if self.working_version != Some(version) {
            return None;
        }
        self.working_version = None;
        // Lazy materialisation: if checkout_for_write deferred the clone,
        // perform it now (typically inside a rayon parallel section).
        Some(self.working.take().unwrap_or_else(|| self.base.clone()))
    }

    fn take_working_or_base_for_version(&mut self, version: i64) -> StorageTrieCow {
        self.take_working_for_version(version).unwrap_or_else(|| self.base.clone())
    }

    fn take_committed_base_for_retire(&mut self) -> StorageTrieCow {
        std::mem::replace(&mut self.base, StorageTrieCow::empty())
    }
}

#[derive(Clone)]
struct AccountTrieHandle {
    base: StorageTrieCow,
    base_version: i64,
    working: Option<StorageTrieCow>,
    working_version: Option<i64>,
}

impl AccountTrieHandle {
    fn snapshot(base_version: i64, trie: StorageTrieCow) -> Self {
        Self { base: trie, base_version, working: None, working_version: None }
    }

    fn committed(&self) -> &StorageTrieCow {
        &self.base
    }

    fn checkout_for_write(&mut self, working_version: i64) {
        if self.working_version == Some(working_version) {
            return;
        }
        if self.working.is_none() {
            self.working = Some(self.base.clone());
        }
        self.working_version = Some(working_version);
    }

    fn take_working_or_base_for_version(&mut self, version: i64) -> StorageTrieCow {
        if self.working_version == Some(version) {
            self.working_version = None;
            self.working.take().unwrap_or_else(|| self.base.clone())
        } else {
            self.base.clone()
        }
    }

    fn current_for_read(&self, version: i64) -> &StorageTrieCow {
        if self.working_version == Some(version) {
            self.working.as_ref().unwrap_or(&self.base)
        } else {
            &self.base
        }
    }

    fn set_committed_base(&mut self, committed_version: i64, trie: StorageTrieCow) {
        self.working = None;
        self.base = trie;
        self.base.snapshot();
        self.base_version = committed_version;
        self.working_version = None;
    }
}

/// A persist job sent to the background worker thread.
struct PersistJob {
    barrier_only: bool,
    replay_from_wal: bool,
    blobs: Vec<(B256, Vec<u8>)>,
    published_puts: Vec<(B256, StorageTrieSegment)>,
    deferred_published_roots: Vec<(B256, B256)>,
    published_deletes: Vec<B256>,
    publish_baseline: bool,
    state_root: B256,
    manifest: VersionManifest,
    manifest_path: PathBuf,
    /// If true, the worker saves the manifest after persist_batch.
    /// WAL-first jobs skip this because the frontend already saved.
    save_manifest: bool,
    /// The version this persist job makes durable. Used to update `durable_version`.
    version: i64,
    /// If set, the background worker signals completion on this channel after
    /// finishing the persist. Used by `flush_persist()` to wait for drain.
    done: Option<crossbeam_channel::Sender<Result<()>>>,
    /// Snapshot clones of committed storage tries for the worker to build
    /// segments from in background.  Used in wal_first mode — matching
    /// sei-db's model where the commit critical path is pure in-memory
    /// and serialization is deferred to a background goroutine via COW
    /// tree clone.  After `snapshot()`, cloning is O(1) (Arc clone).
    committed_tries: Vec<(B256, B256, StorageTrieCow)>,
}

struct PublishedRewriteJob {
    barrier_only: bool,
    target_version: i64,
    state_root: B256,
    /// Pre-built segments from the persist worker's in-memory materializer.
    /// When present, the rewrite worker uses these directly instead of
    /// loading the full trie from disk.
    segments: Option<Vec<(B256, StorageTrieSegment)>>,
    done: Option<crossbeam_channel::Sender<Result<()>>>,
}

struct PreparedStorageVersion {
    new_version: i64,
    state_root: B256,
    manifest: VersionManifest,
    deleted_accounts: HashSet<B256>,
    published_deletes: Vec<B256>,
    use_async: bool,
}

struct SavedStorageVersion {
    new_version: i64,
    manifest: VersionManifest,
    deleted_accounts: HashSet<B256>,
    use_async: bool,
    published_puts: Vec<(B256, StorageTrieSegment)>,
    wal_append_elapsed: Duration,
    persist_elapsed: Duration,
    persist_batch_elapsed: Duration,
    manifest_save_elapsed: Duration,
    publish_generation_elapsed: Duration,
    open_published_store_elapsed: Duration,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BulkLoadOptions {
    pub retain_only_latest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkLoadSummary {
    pub chunks_committed: u64,
    pub final_version: i64,
    pub final_root: B256,
}

#[derive(Clone, Copy)]
struct CommitExecutionMode {
    wal_first: bool,
    allow_async: bool,
    save_manifest: bool,
    publish_baseline: bool,
}

#[derive(Debug, Clone, Copy)]
struct BulkLoadState {
    retain_only_latest: bool,
    chunks_committed: u64,
}

const LIGHT_STORAGE_TRIE_CACHE_MULTIPLIER: usize = 4;

#[derive(Debug, Clone, Default)]
pub struct CommitProfile {
    pub apply_bundle_state: Duration,
    pub apply_collect_dirty_accounts: Duration,
    pub apply_get_or_load_storage_tries: Duration,
    pub apply_storage_slot_updates: Duration,
    pub apply_l3_latest_load: Duration,
    pub apply_l3_published_load: Duration,
    pub apply_l3_into_tree: Duration,
    pub apply_published_refreshes: u64,
    pub apply_l2_hits: u64,
    pub apply_l3_latest_hits: u64,
    pub apply_l3_published_hits: u64,
    pub apply_l3_published_post_flush_hits: u64,
    pub apply_node_fallback_loads: u64,
    pub apply_slot_inserts: u64,
    pub apply_slot_deletes: u64,
    pub apply_leaf_splits: u64,
    pub apply_extension_splits: u64,
    pub apply_branch_collapse_to_empty: u64,
    pub apply_branch_collapse_to_leaf: u64,
    pub apply_branch_collapse_to_extension: u64,
    pub apply_extension_leaf_merges: u64,
    pub apply_extension_extension_merges: u64,
    pub storage_roots: Duration,
    pub storage_roots_prefill: Duration,
    pub storage_roots_take_handles: Duration,
    pub storage_roots_fast_path_collect: Duration,
    pub storage_roots_fast_path_extract: Duration,
    pub storage_roots_fast_path_release: Duration,
    pub storage_roots_fast_path_drop: Duration,
    pub storage_roots_fallback_collect: Duration,
    pub storage_roots_merge: Duration,
    pub storage_roots_working_handles: u64,
    pub storage_roots_precomputed_handles: u64,
    pub storage_roots_rehashed_handles: u64,
    pub storage_root_hashing: Duration,
    pub storage_segment_build: Duration,
    pub account_updates: Duration,
    pub account_root_and_blobs: Duration,
    pub wal_append: Duration,
    pub wal_append_lock_wait: Duration,
    pub wal_append_write: Duration,
    pub wal_replay: Duration,
    pub durable_materialize: Duration,
    pub published_materialize: Duration,
    pub durable_version_lag: i64,
    pub published_version_lag: i64,
    pub persist_and_manifest: Duration,
    pub persist_batch: Duration,
    pub manifest_save: Duration,
    pub publish_generation: Duration,
    pub open_published_store: Duration,
    pub cache_publish: Duration,
    pub total_commit: Duration,
    /// Time to checkout (clone) the account trie for writing in apply phase.
    pub apply_account_trie_checkout: Duration,
    /// Time spent inside ensure_working_storage_tries (L2/L3 cache lookups).
    pub apply_ensure_storage: Duration,
    /// Time spent looking up storage_root from the account trie for L2 misses.
    pub apply_storage_root_lookup: Duration,
    /// Time to freeze the account trie in set_committed_base after commit.
    pub commit_account_set_base: Duration,
    /// Time to prepare + cache 5000 storage tries back into L2 after commit.
    pub commit_cache_storage_prep: Duration,
}

#[derive(Default)]
struct StorageTrieLoadStats {
    l2_hits: u64,
    l3_latest_hits: u64,
    l3_published_hits: u64,
    l3_published_post_flush_hits: u64,
    node_fallback_loads: u64,
    storage_root_lookup: Duration,
    l3_latest_load: Duration,
    l3_published_load: Duration,
}

#[derive(Clone, Serialize, Deserialize)]
struct AccountTrieCheckpoint {
    version: i64,
    root: B256,
    trie: MptTree,
}

/// MPT-based commit store with persistence, rollback, and recovery support.
pub struct MptCommitStore {
    #[allow(dead_code)]
    dir: PathBuf,
    manifest_path: PathBuf,

    account_trie: AccountTrieHandle,
    /// Long-lived per-account storage trie handles. Working block-local state is carried in the
    /// handle's optional `working` trie rather than a separate map.
    storage_trie_handles: HashMap<B256, StorageTrieHandle>,
    /// Cross-block LRU index for resident storage trie bases.
    storage_trie_cache: LruMap<B256, (), ByLength>,
    dirty_accounts: Vec<DirtyAccount>,

    persisted: Arc<PersistedTrieStore>,
    published_baseline: Arc<PublishedBaselineManager>,
    published_meta: Option<PublishedBaselineMeta>,
    published_store: Option<PublishedBaselineReader>,
    manifest: VersionManifest,
    wal_store: Option<Arc<Mutex<CommitWalStore>>>,

    version: i64,
    /// When > 1 and current version == 0, the first commit jumps to this
    /// version instead of 1.  Mirrors sei-db's `initialVersion`.
    initial_version: i64,
    applied_this_block: bool,
    poisoned: bool,
    read_only: bool,
    replay_materializer: bool,
    file_lock: Option<File>,

    parallelism: ParallelismThresholds,
    config: MptConfig,
    bulk_load: Option<BulkLoadState>,
    /// Streaming segment writer active during bulk_load (sei-db model).
    bulk_segment_writer: Option<BulkSegmentWriter>,

    /// Latest version whose nodes and manifest are confirmed on stable storage.
    durable_version: Arc<AtomicI64>,
    /// Latest version whose published snapshot has been installed.
    published_version: Arc<AtomicI64>,
    last_wal_replay_micros: Arc<AtomicU64>,
    last_durable_materialize_micros: Arc<AtomicU64>,
    last_published_materialize_micros: Arc<AtomicU64>,

    /// Channel to send persist jobs to the background worker.
    persist_tx: Option<crossbeam_channel::Sender<PersistJob>>,
    /// Handle to the background persist worker thread.
    persist_handle: Option<JoinHandle<()>>,
    /// Channel to send low-frequency published snapshot rewrite jobs.
    published_rewrite_tx: Option<crossbeam_channel::Sender<PublishedRewriteJob>>,
    /// Handle to the background published snapshot rewrite worker.
    published_rewrite_handle: Option<JoinHandle<()>>,
    /// Channel to save account trie checkpoints off the shutdown path.
    checkpoint_save_tx: Option<crossbeam_channel::Sender<AccountTrieCheckpoint>>,
    /// Handle to the background checkpoint worker.
    checkpoint_save_handle: Option<JoinHandle<()>>,
    /// Latest checkpoint version durably written to disk.
    checkpoint_saved_version: Arc<AtomicI64>,
    async_error: Arc<AtomicBool>,
    async_error_detail: Arc<Mutex<Option<String>>>,
    pending_checkpoint: Option<AccountTrieCheckpoint>,
    last_apply_duration: Duration,
    last_apply_collect_dirty_accounts: Duration,
    last_apply_get_or_load_storage_tries: Duration,
    last_apply_account_trie_checkout: Duration,
    last_apply_ensure_storage: Duration,
    last_apply_storage_root_lookup: Duration,
    last_apply_storage_slot_updates: Duration,
    last_apply_l3_latest_load: Duration,
    last_apply_l3_published_load: Duration,
    last_apply_l3_into_tree: Duration,
    last_apply_published_refreshes: u64,
    last_apply_l2_hits: u64,
    last_apply_l3_latest_hits: u64,
    last_apply_l3_published_hits: u64,
    last_apply_l3_published_post_flush_hits: u64,
    last_apply_node_fallback_loads: u64,
    last_apply_slot_inserts: u64,
    last_apply_slot_deletes: u64,
    last_apply_leaf_splits: u64,
    last_apply_extension_splits: u64,
    last_apply_branch_collapse_to_empty: u64,
    last_apply_branch_collapse_to_leaf: u64,
    last_apply_branch_collapse_to_extension: u64,
    last_apply_extension_leaf_merges: u64,
    last_apply_extension_extension_merges: u64,
    last_wal_append_lock_wait: Duration,
    last_wal_append_write: Duration,
    last_commit_profile: CommitProfile,
    checkpoint_account_trie_nodes: Option<usize>,
    shutdown_complete: bool,
    #[cfg(test)]
    loaded_from_checkpoint: bool,

    #[cfg(test)]
    fail_point: Option<CommitFailPoint>,
    #[cfg(test)]
    async_fail_mode: Arc<std::sync::atomic::AtomicU8>,
}

impl MptCommitStore {
    fn diagnostics_enabled() -> bool {
        std::env::var_os("MPT_DEBUG_DIAGNOSTICS").is_some()
    }

    fn write_checkpoint_file(dir: &Path, checkpoint: &AccountTrieCheckpoint) -> Result<()> {
        let bytes = bincode::serialize(checkpoint)
            .map_err(|e| MptDbError::Other(format!("serialize account trie checkpoint: {e}")))?;
        let path = Self::checkpoint_path(dir);
        let tmp = path.with_extension("bin.tmp");
        fs::write(&tmp, bytes)
            .map_err(|e| MptDbError::Other(format!("write account trie checkpoint tmp: {e}")))?;
        fs::rename(&tmp, &path)
            .map_err(|e| MptDbError::Other(format!("rename account trie checkpoint: {e}")))?;
        Ok(())
    }

    fn start_checkpoint_worker(&mut self) -> Result<()> {
        if self.read_only || self.checkpoint_save_tx.is_some() {
            return Ok(());
        }

        let (tx, rx) = crossbeam_channel::bounded::<AccountTrieCheckpoint>(1);
        let dir = self.dir.clone();
        let saved_version = Arc::clone(&self.checkpoint_saved_version);
        let handle = std::thread::Builder::new()
            .name("mpt-checkpoint".to_string())
            .spawn(move || {
                while let Ok(checkpoint) = rx.recv() {
                    if Self::write_checkpoint_file(&dir, &checkpoint).is_ok() {
                        saved_version.store(checkpoint.version, Ordering::Release);
                    }
                }
            })
            .map_err(|e| MptDbError::Other(format!("spawn checkpoint thread: {e}")))?;

        self.checkpoint_save_tx = Some(tx);
        self.checkpoint_save_handle = Some(handle);
        Ok(())
    }

    fn build_account_checkpoint(&self) -> Result<Option<AccountTrieCheckpoint>> {
        if !self.should_save_checkpoint() || self.applied_this_block {
            return Ok(None);
        }
        let root = self.manifest.get_root(self.version).unwrap_or(EMPTY_ROOT_HASH);
        let trie =
            if let Some(tree) = self.account_trie.committed().clone_materialized_tree_if_ready() {
                tree
            } else {
                self.account_trie.committed().clone().into_materialized_tree(&self.persisted)?
            };
        Ok(Some(AccountTrieCheckpoint { version: self.version, root, trie }))
    }

    fn pump_pending_checkpoint(&mut self) {
        let Some(tx) = self.checkpoint_save_tx.as_ref() else {
            return;
        };
        let Some(checkpoint) = self.pending_checkpoint.take() else {
            return;
        };

        if self.checkpoint_saved_version.load(Ordering::Acquire) >= checkpoint.version {
            return;
        }

        match tx.try_send(checkpoint) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(checkpoint)) => {
                self.pending_checkpoint = Some(checkpoint);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_checkpoint)) => {}
        }
    }

    fn schedule_checkpoint_save(&mut self) -> Result<()> {
        if self.read_only || self.replay_materializer {
            return Ok(());
        }
        self.pump_pending_checkpoint();
        let Some(checkpoint) = self.build_account_checkpoint()? else {
            return Ok(());
        };
        if self.checkpoint_saved_version.load(Ordering::Acquire) >= checkpoint.version {
            return Ok(());
        }
        let Some(tx) = self.checkpoint_save_tx.as_ref() else {
            return Ok(());
        };
        match tx.try_send(checkpoint) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(checkpoint)) => {
                self.pending_checkpoint = Some(checkpoint);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_checkpoint)) => {}
        }
        Ok(())
    }

    /// Next version that will be assigned on commit.
    ///
    /// Mirrors sei-db's `nextVersionU32`: if `version == 0` and
    /// `initial_version > 1`, jump directly to `initial_version`.
    fn current_working_version(&self) -> i64 {
        if self.version == 0 && self.initial_version > 1 {
            self.initial_version
        } else {
            self.version + 1
        }
    }

    fn storage_trie_cache_limit(capacity: usize) -> usize {
        if capacity == 0 {
            0
        } else {
            capacity.saturating_mul(LIGHT_STORAGE_TRIE_CACHE_MULTIPLIER)
        }
    }

    fn new_storage_trie_cache(capacity: usize) -> LruMap<B256, (), ByLength> {
        let limit = Self::storage_trie_cache_limit(capacity);
        LruMap::new(ByLength::new(limit.max(1) as u32))
    }

    fn checkout_cached_storage_trie(&mut self, hashed_address: &B256) -> bool {
        let working_version = self.current_working_version();
        let Some(handle) = self.storage_trie_handles.get_mut(hashed_address) else {
            return false;
        };
        handle.checkout_for_write(working_version);
        if self.storage_trie_cache.peek(hashed_address).is_some() {
            let _ = self.storage_trie_cache.get(hashed_address);
        }
        true
    }

    fn evict_cached_storage_trie(&mut self, hashed_address: &B256) -> Option<StorageTrieCow> {
        self.storage_trie_cache.remove(hashed_address);
        self.storage_trie_handles
            .remove(hashed_address)
            .map(|handle| handle.working.unwrap_or(handle.base))
    }

    fn touch_cached_storage_trie(&mut self, hashed_address: B256) {
        let limit = Self::storage_trie_cache_limit(self.config.storage_trie_cache_capacity);
        if limit == 0 {
            return;
        }

        let evicted = if self.storage_trie_cache.peek(&hashed_address).is_none() &&
            self.storage_trie_cache.len() >= limit
        {
            self.storage_trie_cache.peek_oldest().map(|(oldest, _)| *oldest)
        } else {
            None
        };

        self.storage_trie_cache.remove(&hashed_address);
        let _ = self.storage_trie_cache.get_or_insert(hashed_address, || ());

        if let Some(evicted) = evicted {
            if evicted != hashed_address &&
                self.storage_trie_cache.peek(&evicted).is_none() &&
                self.storage_trie_handles.get(&evicted).is_some_and(|handle| {
                    !handle.has_working() &&
                        // In wal_first mode, only evict if the published segment
                        // has caught up to this handle's version.  Otherwise the
                        // trie data only exists in memory — evicting would lose it
                        // since RocksDB has no nodes in wal_first mode.
                        // Matches sei-db's invariant: trees stay resident until
                        // their data is durably persisted (snapshot rewrite).
                        (!self.config.wal_first_commit ||
                         self.published_version.load(Ordering::Acquire) >= handle.base_version)
                })
            {
                self.storage_trie_handles.remove(&evicted);
            }
        }
    }

    fn cache_storage_trie(&mut self, hashed_address: B256, trie: StorageTrieCow) {
        let committed_version = self.version;
        let already_cached = self.storage_trie_cache.peek(&hashed_address).is_some();
        if let Some(handle) = self.storage_trie_handles.get_mut(&hashed_address) {
            handle.set_committed_base(committed_version, trie);
        } else {
            self.storage_trie_handles
                .insert(hashed_address, StorageTrieHandle::snapshot(committed_version, trie));
        }
        if !already_cached {
            self.touch_cached_storage_trie(hashed_address);
        }
    }

    fn clear_storage_trie_state(&mut self) {
        self.storage_trie_cache.clear();
        self.storage_trie_handles.clear();
    }

    #[cfg(test)]
    fn storage_trie_cache_contains(&self, hashed_address: &B256) -> bool {
        self.storage_trie_cache.peek(hashed_address).is_some()
    }

    #[cfg(test)]
    fn clone_cached_storage_trie(&mut self, hashed_address: &B256) -> Option<StorageTrieCow> {
        if self.storage_trie_cache.peek(hashed_address).is_none() {
            return None;
        }
        let handle = self.storage_trie_handles.get(hashed_address)?;
        Some(handle.base.clone())
    }

    #[cfg(test)]
    fn storage_trie_cache_is_empty(&self) -> bool {
        self.storage_trie_cache.is_empty() && self.storage_trie_handles.is_empty()
    }

    #[cfg(test)]
    fn working_storage_tries_empty(&self) -> bool {
        self.storage_trie_handles.values().all(|handle| !handle.has_working())
    }

    #[cfg(test)]
    fn storage_trie_handle_versions(&self, hashed_address: &B256) -> Option<(i64, Option<i64>)> {
        self.storage_trie_handles
            .get(hashed_address)
            .map(|handle| (handle.base_version, handle.working_version))
    }

    #[cfg(test)]
    fn account_trie_handle_versions(&self) -> (i64, Option<i64>) {
        (self.account_trie.base_version, self.account_trie.working_version)
    }

    fn activate_snapshot_trie(&mut self, hashed_address: B256, trie: StorageTrieCow) {
        self.storage_trie_handles.insert(
            hashed_address,
            StorageTrieHandle::activate(self.version, self.current_working_version(), trie),
        );
        self.touch_cached_storage_trie(hashed_address);
    }

    fn activate_empty_trie(&mut self, hashed_address: B256) {
        self.storage_trie_handles.insert(
            hashed_address,
            StorageTrieHandle::activate(
                self.version,
                self.current_working_version(),
                StorageTrieCow::empty(),
            ),
        );
    }

    fn contains_working_trie(&self, hashed_address: &B256) -> bool {
        self.storage_trie_handles
            .get(hashed_address)
            .is_some_and(|handle| handle.has_working_for_version(self.current_working_version()))
    }

    fn take_working_handles(
        &mut self,
        hashed_addresses: impl IntoIterator<Item = B256>,
    ) -> Vec<(B256, StorageTrieHandle)> {
        let mut handles = Vec::new();
        for hashed_address in hashed_addresses {
            if let Some(handle) = self.storage_trie_handles.remove(&hashed_address) {
                handles.push((hashed_address, handle));
            }
        }
        handles
    }

    fn reinsert_handles(&mut self, handles: impl IntoIterator<Item = (B256, StorageTrieHandle)>) {
        self.storage_trie_handles.extend(handles);
    }

    fn ensure_working_storage_tries(
        &mut self,
        dirty_accounts: &[DirtyAccount],
    ) -> Result<StorageTrieLoadStats> {
        let mut stats = StorageTrieLoadStats::default();
        let mut latest_candidates: Vec<(B256, B256)> = Vec::new();

        // Lazy refresh: if the background rewrite worker has produced a newer
        // published snapshot, reload it so that L3 lookups see the new data.
        self.maybe_refresh_published_view()?;
        let published_current = self.has_current_published_view();

        // Two-phase approach: first check cheap filters (O(1) each),
        // then do expensive trie walks only for cache misses.
        // This avoids 5000× O(depth) account trie walks when 99%+ hit L2.
        let mut need_storage_root: Vec<&DirtyAccount> = Vec::new();
        for dirty in dirty_accounts {
            if dirty.storage_wiped || dirty.storage_changes.is_empty() {
                continue;
            }
            if self.contains_working_trie(&dirty.hashed_address) {
                continue;
            }
            if dirty.storage_known_empty {
                self.activate_empty_trie(dirty.hashed_address);
                continue;
            }
            // Check L2 cache BEFORE the expensive trie walk.
            if self.checkout_cached_storage_trie(&dirty.hashed_address) {
                stats.l2_hits += 1;
                continue;
            }
            need_storage_root.push(dirty);
        }

        // Only the cache-miss accounts need an account trie lookup.
        let storage_root_lookup_start = std::time::Instant::now();
        for dirty in need_storage_root {
            let existing_root = self.get_existing_storage_root(&dirty.hashed_address);
            if existing_root == EMPTY_ROOT_HASH {
                self.activate_empty_trie(dirty.hashed_address);
            } else {
                latest_candidates.push((dirty.hashed_address, existing_root));
            }
        }
        stats.storage_root_lookup = storage_root_lookup_start.elapsed();

        let mut storage_loads =
            self.load_latest_storage_tries(latest_candidates, published_current, &mut stats)?;
        storage_loads =
            self.load_published_storage_tries(storage_loads, published_current, &mut stats)?;
        self.load_persisted_storage_tries(storage_loads, published_current, &mut stats)?;

        Ok(stats)
    }

    fn load_latest_storage_tries(
        &mut self,
        latest_candidates: Vec<(B256, B256)>,
        _published_current: bool,
        stats: &mut StorageTrieLoadStats,
    ) -> Result<Vec<(B256, B256)>> {
        let _ = stats;
        Ok(latest_candidates)
    }

    fn load_published_storage_tries(
        &mut self,
        candidates: Vec<(B256, B256)>,
        _published_current: bool,
        stats: &mut StorageTrieLoadStats,
    ) -> Result<Vec<(B256, B256)>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let Some(ref store) = self.published_store else {
            return Ok(candidates);
        };

        let resolved = candidates
            .into_par_iter()
            .map(|(hashed_address, existing_root)| {
                match store.open_trie(&hashed_address, existing_root)? {
                    Some(loaded) => {
                        Ok((Some((hashed_address, loaded.trie, loaded.lookup_elapsed)), None))
                    }
                    None => Ok((None, Some((hashed_address, existing_root)))),
                }
            })
            .collect::<Result<Vec<_>>>()?;

        let mut remaining = Vec::new();
        for (loaded, fallback) in resolved {
            if let Some((hashed_address, trie, load_elapsed)) = loaded {
                self.activate_snapshot_trie(hashed_address, trie);
                stats.l3_published_hits += 1;
                stats.l3_published_load += load_elapsed;
            } else if let Some(load) = fallback {
                remaining.push(load);
            }
        }
        Ok(remaining)
    }

    fn load_persisted_storage_tries(
        &mut self,
        mut candidates: Vec<(B256, B256)>,
        mut published_current: bool,
        stats: &mut StorageTrieLoadStats,
    ) -> Result<()> {
        if candidates.is_empty() {
            return Ok(());
        }

        // Refresh the published view without blocking — the background worker
        // updates published_version atomically; pick up any new segments that
        // have been published since the last refresh.
        if !published_current {
            self.reload_published_view()?;
            published_current = self.has_current_published_view();
        }

        if published_current {
            candidates = self.load_published_storage_tries_after_flush(candidates, stats)?;
        }

        if !candidates.is_empty() {
            if self.config.wal_first_commit {
                // In wal_first mode, RocksDB has no trie nodes — the persisted
                // fallback will fail.  This should not happen if the eviction
                // guard in touch_cached_storage_trie works correctly.
                tracing::warn!(
                    count = candidates.len(),
                    published_version = self.published_version.load(Ordering::Acquire),
                    committed_version = self.version,
                    "wal_first: storage tries missing from both L2 cache and published segments"
                );
            }
            // Fallback to persisted nodes (RocksDB).  In wal_first mode this
            // creates lazy nodes that may fail on access — the warning above
            // flags the issue for investigation.
            stats.node_fallback_loads += candidates.len() as u64;
            for (hashed_address, existing_root) in candidates {
                self.activate_snapshot_trie(
                    hashed_address,
                    StorageTrieCow::from_persisted_root(existing_root),
                );
            }
        }

        Ok(())
    }

    fn load_published_storage_tries_after_flush(
        &mut self,
        candidates: Vec<(B256, B256)>,
        stats: &mut StorageTrieLoadStats,
    ) -> Result<Vec<(B256, B256)>> {
        let Some(ref store) = self.published_store else {
            return Ok(candidates);
        };

        let resolved = candidates
            .into_par_iter()
            .map(|(hashed_address, existing_root)| {
                match store.open_trie(&hashed_address, existing_root)? {
                    Some(loaded) => {
                        Ok((Some((hashed_address, loaded.trie, loaded.lookup_elapsed)), None))
                    }
                    None => Ok((None, Some((hashed_address, existing_root)))),
                }
            })
            .collect::<Result<Vec<_>>>()?;

        let mut remaining = Vec::new();
        for (loaded, fallback) in resolved {
            if let Some((hashed_address, trie, load_elapsed)) = loaded {
                self.activate_snapshot_trie(hashed_address, trie);
                stats.l3_published_post_flush_hits += 1;
                stats.l3_published_load += load_elapsed;
            } else if let Some(load) = fallback {
                remaining.push(load);
            }
        }

        Ok(remaining)
    }

    fn apply_storage_changes_to_working(
        mut trie: StorageTrieCow,
        persisted: &PersistedTrieStore,
        dirty: &DirtyAccount,
    ) -> Result<StorageTrieCow> {
        trie.apply_changes_batched(persisted, &dirty.storage_changes)?;
        Ok(trie)
    }

    fn prepare_storage_version(
        &self,
        state_root: B256,
        storage_roots: &HashMap<B256, B256>,
        storage_cache_candidates: &[(B256, StorageTrieCow)],
        all_blobs_len: usize,
        allow_async: bool,
    ) -> Result<PreparedStorageVersion> {
        let new_version = self.version + 1;
        let mut manifest = self.manifest.clone();
        manifest.add_version(new_version, state_root)?;

        let deleted_accounts: HashSet<B256> = self
            .dirty_accounts
            .iter()
            .filter(|d| d.info.is_none() && d.storage_wiped)
            .map(|d| d.hashed_address)
            .collect();
        let mut published_deletes = deleted_accounts.iter().copied().collect::<Vec<_>>();
        published_deletes.extend(storage_cache_candidates.iter().filter_map(|(addr, _)| {
            match storage_roots.get(addr).copied() {
                Some(root) if root == EMPTY_ROOT_HASH && !deleted_accounts.contains(addr) => {
                    Some(*addr)
                }
                _ => None,
            }
        }));

        let use_async = allow_async &&
            all_blobs_len < self.config.async_blob_threshold &&
            self.persist_tx.is_some();

        Ok(PreparedStorageVersion {
            new_version,
            state_root,
            manifest,
            deleted_accounts,
            published_deletes,
            use_async,
        })
    }

    fn prepare_cached_storage_trie(
        &self,
        trie: StorageTrieCow,
        _storage_root: B256,
        _published_segment: Option<&StorageTrieSegment>,
        _use_async: bool,
    ) -> Result<StorageTrieCow> {
        // Segment serialization already done eagerly in the storage_roots
        // rayon loop.  Just return the pre-cached trie.
        Ok(trie)
    }

    fn save_storage_version(
        &mut self,
        prepared: PreparedStorageVersion,
        all_blobs: Vec<(B256, Vec<u8>)>,
        storage_roots: &HashMap<B256, B256>,
        storage_cache_candidates: &mut [(B256, StorageTrieCow)],
        deferred_published_roots: Vec<(B256, B256)>,
        mode: CommitExecutionMode,
        storage_segment_build_elapsed: &mut Duration,
    ) -> Result<SavedStorageVersion> {
        let persist_start = std::time::Instant::now();
        let mut published_puts = Vec::new();
        let mut persist_batch_elapsed = Duration::ZERO;
        let mut manifest_save_elapsed = Duration::ZERO;
        let mut publish_generation_elapsed = Duration::ZERO;
        let mut open_published_store_elapsed = Duration::ZERO;

        if mode.wal_first || prepared.use_async {
            // WAL-first: frontend builds segments from in-memory tries,
            // background worker publishes them to mmap.  WAL + segments
            // provide full crash recovery — RocksDB is no longer on the
            // critical path.
            //
            // Async (non-wal_first): legacy path — blobs + deferred roots
            // are sent to the worker for RocksDB persist + segment build.
            self.wait_for_backpressure()?;

            // Freeze + clone in parallel: snapshot() consolidates the small
            // overlay into the frozen base (O(overlay) because the handle's
            // old base was already dropped by compute_storage_artifact, so
            // Arc::make_mut is in-place).  After freeze, clone is O(1).
            //
            // hash_only mode skips RLP caching during root hash, keeping
            // the overlay minimal — freeze only drains ~20 overlay entries
            // + ~20 hash_cache entries per trie, no rlp_cache to clear.
            //
            // Parallel via rayon: amortizes 5000 storage tries across cores,
            // adapting sei-db's single-tree COW model to Ethereum's two-layer
            // trie architecture.
            let committed_tries = if mode.wal_first && mode.publish_baseline {
                storage_cache_candidates
                    .par_iter_mut()
                    .filter_map(|(addr, trie)| {
                        let root = storage_roots.get(addr).copied().unwrap_or(EMPTY_ROOT_HASH);
                        if root == EMPTY_ROOT_HASH {
                            return None;
                        }
                        trie.snapshot();
                        Some((*addr, root, trie.clone()))
                    })
                    .collect()
            } else {
                Vec::new()
            };

            let (job_deferred_roots, job_blobs) = if mode.wal_first {
                // No blobs, no deferred roots — worker builds from trie clones.
                (Vec::new(), Vec::new())
            } else {
                // Legacy async: worker builds segments from RocksDB.
                (deferred_published_roots, all_blobs)
            };

            let tx = self.persist_tx.as_ref().unwrap();
            let job = PersistJob {
                barrier_only: false,
                replay_from_wal: false,
                blobs: job_blobs,
                published_puts: Vec::new(),
                deferred_published_roots: job_deferred_roots,
                published_deletes: prepared.published_deletes.clone(),
                publish_baseline: mode.publish_baseline,
                state_root: prepared.state_root,
                manifest: prepared.manifest.clone(),
                manifest_path: self.manifest_path.clone(),
                save_manifest: true,
                version: prepared.new_version,
                done: None,
                committed_tries,
            };
            tx.send(job).map_err(|e| MptDbError::Other(format!("send persist job: {e}")))?;
        } else {
            if mode.publish_baseline {
                let segment_build_start = std::time::Instant::now();
                published_puts = Self::build_publish_segments_from_tries(
                    storage_roots,
                    storage_cache_candidates,
                )?;
                *storage_segment_build_elapsed += segment_build_start.elapsed();
            }
            let persist_batch_start = std::time::Instant::now();
            self.persisted.persist_batch(&all_blobs, true)?;
            persist_batch_elapsed = persist_batch_start.elapsed();
            self.durable_version.store(prepared.new_version, Ordering::Release);
            if mode.save_manifest {
                let manifest_save_start = std::time::Instant::now();
                prepared.manifest.save(&self.manifest_path)?;
                manifest_save_elapsed = manifest_save_start.elapsed();
            }
            if mode.publish_baseline {
                if let Some(ref mut writer) = self.bulk_segment_writer {
                    // Streaming path: append pages to bulk file — no per-chunk
                    // delta/meta files, no mmap reopen.  Matches sei-db's
                    // snapshotWriter streaming directly to snapshot files.
                    let append_start = std::time::Instant::now();
                    writer.append_segments(&published_puts)?;
                    publish_generation_elapsed = append_start.elapsed();
                } else {
                    let publish_generation_start = std::time::Instant::now();
                    let published_meta = self.published_baseline.publish_generation(
                        self.published_meta.as_ref(),
                        prepared.new_version,
                        prepared.state_root,
                        &published_puts,
                        &prepared.published_deletes,
                    )?;
                    publish_generation_elapsed = publish_generation_start.elapsed();
                    self.published_meta = Some(published_meta.meta.clone());
                    let open_published_store_start = std::time::Instant::now();
                    self.published_store =
                        self.published_baseline.open_published_store(&published_meta.meta)?;
                    open_published_store_elapsed = open_published_store_start.elapsed();
                    self.published_version.store(prepared.new_version, Ordering::Release);
                }
            }
        }

        Ok(SavedStorageVersion {
            new_version: prepared.new_version,
            manifest: prepared.manifest,
            deleted_accounts: prepared.deleted_accounts,
            use_async: prepared.use_async || mode.wal_first,
            published_puts,
            wal_append_elapsed: Duration::ZERO,
            persist_elapsed: persist_start.elapsed(),
            persist_batch_elapsed,
            manifest_save_elapsed,
            publish_generation_elapsed,
            open_published_store_elapsed,
        })
    }

    fn current_async_error(detail: &Mutex<Option<String>>) -> MptDbError {
        MptDbError::Other(
            detail.lock().clone().unwrap_or_else(|| "mpt async persist failed".to_string()),
        )
    }

    fn build_publish_segments_from_roots(
        persisted: &PersistedTrieStore,
        deferred_roots: &[(B256, B256)],
    ) -> Result<Vec<(B256, StorageTrieSegment)>> {
        let built = deferred_roots
            .par_iter()
            .map(|(hashed_address, root)| -> Result<Option<(B256, StorageTrieSegment)>> {
                if *root == EMPTY_ROOT_HASH {
                    return Ok(None);
                }
                let tree = persisted::load_tree_from_root(persisted, *root)?;
                let segment = StorageTrieSegment::from_tree(&tree, *root)?;
                Ok(Some((*hashed_address, segment)))
            })
            .collect::<Vec<_>>();

        let mut segments = Vec::with_capacity(deferred_roots.len());
        for segment in built {
            if let Some(segment) = segment? {
                segments.push(segment);
            }
        }
        Ok(segments)
    }

    fn build_publish_segments_from_tries(
        storage_roots: &HashMap<B256, B256>,
        tries: &[(B256, StorageTrieCow)],
    ) -> Result<Vec<(B256, StorageTrieSegment)>> {
        let built = tries
            .par_iter()
            .map(|(hashed_address, trie)| -> Result<Option<(B256, StorageTrieSegment)>> {
                let Some(root) = storage_roots.get(hashed_address).copied() else {
                    return Ok(None);
                };
                if root == EMPTY_ROOT_HASH {
                    return Ok(None);
                }
                let nodes = trie.arena_nodes();
                let hashes = trie.arena_hash_cache();
                let segment =
                    StorageTrieSegment::from_parts(&nodes, &hashes, trie.root_index(), root)?;
                Ok(Some((*hashed_address, segment)))
            })
            .collect::<Vec<_>>();

        let mut segments = Vec::with_capacity(tries.len());
        for segment in built {
            if let Some(segment) = segment? {
                segments.push(segment);
            }
        }
        Ok(segments)
    }

    fn nibbles_path_to_b256(path: &[u8]) -> Result<B256> {
        if path.len() != 64 {
            return Err(MptDbError::Other(format!(
                "expected 64-nibble account key, got {}",
                path.len()
            )));
        }
        let mut out = [0u8; 32];
        for (idx, chunk) in path.chunks_exact(2).enumerate() {
            if chunk[0] > 0x0f || chunk[1] > 0x0f {
                return Err(MptDbError::Other("account key nibble out of range".to_string()));
            }
            out[idx] = (chunk[0] << 4) | chunk[1];
        }
        Ok(B256::from(out))
    }

    fn build_full_published_segments(
        persisted: &PersistedTrieStore,
        state_root: B256,
    ) -> Result<Vec<(B256, StorageTrieSegment)>> {
        let deferred_roots = Self::collect_deferred_roots_from_persisted(persisted, state_root)?;
        Self::build_publish_segments_from_roots(persisted, &deferred_roots)
    }

    fn collect_deferred_roots_from_persisted(
        persisted: &PersistedTrieStore,
        state_root: B256,
    ) -> Result<Vec<(B256, B256)>> {
        if state_root == EMPTY_ROOT_HASH {
            return Ok(Vec::new());
        }

        let account_tree = persisted::load_tree_from_root(persisted, state_root)?;
        let account_leaves = account_tree.collect_leaf_entries();
        let mut deferred_roots = Vec::new();
        for (path, value) in account_leaves {
            let hashed_address = Self::nibbles_path_to_b256(&path)?;
            let trie_account: alloy_trie::TrieAccount =
                alloy_rlp::Decodable::decode(&mut &value[..]).map_err(|e| {
                    MptDbError::Other(format!("decode account leaf during published rewrite: {e}"))
                })?;
            if trie_account.storage_root != EMPTY_ROOT_HASH {
                deferred_roots.push((hashed_address, trie_account.storage_root));
            }
        }
        Ok(deferred_roots)
    }

    /// Build full published segments using the materializer's in-memory account
    /// trie to enumerate accounts, and the persisted store (which is cache-hot
    /// right after persist_batch) to load storage tries.
    fn build_full_published_segments_from_memory(
        &self,
        state_root: B256,
    ) -> Result<Vec<(B256, StorageTrieSegment)>> {
        if state_root == EMPTY_ROOT_HASH {
            return Ok(Vec::new());
        }

        // Use the materializer's in-memory account trie to enumerate leaves.
        // This avoids reloading the entire account trie from disk.
        let account_trie = self.account_trie.committed();
        let account_leaves = account_trie.collect_leaf_entries();
        let mut deferred_roots = Vec::new();
        for (path, value) in account_leaves {
            let hashed_address = Self::nibbles_path_to_b256(&path)?;
            let trie_account: alloy_trie::TrieAccount =
                alloy_rlp::Decodable::decode(&mut &value[..]).map_err(|e| {
                    MptDbError::Other(format!("decode account leaf during segment build: {e}"))
                })?;
            if trie_account.storage_root != EMPTY_ROOT_HASH {
                deferred_roots.push((hashed_address, trie_account.storage_root));
            }
        }

        // Build segments from persisted store — nodes are cache-hot after persist_batch.
        Self::build_publish_segments_from_roots(&self.persisted, &deferred_roots)
    }

    fn should_rewrite_published_snapshot(
        config: &MptConfig,
        published_version: i64,
        durable_version: i64,
    ) -> bool {
        if durable_version <= 0 {
            return false;
        }
        if published_version <= 0 {
            true
        } else {
            durable_version.saturating_sub(published_version) >=
                config.published_snapshot_interval as i64
        }
    }

    fn rewrite_published_snapshot_at_version(
        persisted: &Arc<PersistedTrieStore>,
        published_baseline: &Arc<PublishedBaselineManager>,
        manifest: &VersionManifest,
        target_version: i64,
    ) -> Result<Option<PublishedBaselineMeta>> {
        if target_version < manifest.earliest_version || target_version > manifest.latest_version {
            return Ok(None);
        }
        let base_root = manifest.get_root(target_version).unwrap_or(EMPTY_ROOT_HASH);
        let full_puts = Self::build_full_published_segments(persisted, base_root)?;
        let staged_meta = published_baseline
            .stage_generation(None, target_version, base_root, &full_puts, &[])?
            .meta;
        Ok(Some(staged_meta))
    }

    fn wal_prune_floor_for_manifest(
        manifest: &VersionManifest,
        published_baseline: &PublishedBaselineManager,
    ) -> Result<i64> {
        let mut floor = manifest.earliest_version;
        if let Some(snapshot_floor) = published_baseline.earliest_snapshot_version()? {
            floor = floor.min(snapshot_floor);
        }
        Ok(floor)
    }

    /// Schedule a published rewrite job.  Returns `true` if the job was
    /// enqueued, `false` if the queue was full (a rewrite is in progress).
    /// The caller should track the `false` case and retry on the next commit.
    fn schedule_published_rewrite(
        rewrite_tx: &crossbeam_channel::Sender<PublishedRewriteJob>,
        target_version: i64,
        state_root: B256,
        segments: Option<Vec<(B256, StorageTrieSegment)>>,
    ) -> Result<bool> {
        match rewrite_tx.try_send(PublishedRewriteJob {
            barrier_only: false,
            target_version,
            state_root,
            segments,
            done: None,
        }) {
            Ok(()) => Ok(true),
            Err(crossbeam_channel::TrySendError::Full(_)) => Ok(false),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                Err(MptDbError::Other("published rewrite worker disconnected".to_string()))
            }
        }
    }

    fn spawn_published_rewrite_worker(
        persisted: Arc<PersistedTrieStore>,
        published_baseline: Arc<PublishedBaselineManager>,
        wal_store: Option<Arc<Mutex<CommitWalStore>>>,
        manifest_path: PathBuf,
        durable_version: Arc<AtomicI64>,
        published_version: Arc<AtomicI64>,
        last_published_materialize_micros: Arc<AtomicU64>,
        async_error: Arc<AtomicBool>,
        async_error_detail: Arc<Mutex<Option<String>>>,
        rewrite_timeout: Duration,
        snapshot_write_rate_mb_per_sec: u64,
        #[cfg(test)] async_fail_mode: Arc<std::sync::atomic::AtomicU8>,
    ) -> Result<(crossbeam_channel::Sender<PublishedRewriteJob>, JoinHandle<()>)> {
        let (tx, rx) = crossbeam_channel::bounded::<PublishedRewriteJob>(1);
        let handle = std::thread::Builder::new()
            .name("mpt-published-rewrite".to_string())
            .spawn(move || {
                while let Ok(mut job) = rx.recv() {
                    // Coalesce queued jobs — keep the one with highest target_version.
                    while let Ok(next) = rx.try_recv() {
                        if next.barrier_only {
                            if let Some(done) = next.done {
                                let result = if async_error.load(Ordering::Relaxed) {
                                    Err(Self::current_async_error(&async_error_detail))
                                } else {
                                    Ok(())
                                };
                                let _ = done.send(result);
                            }
                            continue;
                        }
                        if next.target_version > job.target_version {
                            job = next;
                        }
                    }

                    if async_error.load(Ordering::Relaxed) {
                        if let Some(done) = job.done {
                            let _ = done.send(Err(Self::current_async_error(&async_error_detail)));
                        }
                        continue;
                    }

                    let result = if job.barrier_only {
                        Ok(())
                    } else {
                        (|| -> Result<()> {
                            // Wait for durable_version to reach the target, with timeout.
                            let wait_start = std::time::Instant::now();
                            while durable_version.load(Ordering::Acquire) < job.target_version {
                                if async_error.load(Ordering::Relaxed) {
                                    return Err(Self::current_async_error(&async_error_detail));
                                }
                                if wait_start.elapsed() >= rewrite_timeout {
                                    tracing::warn!(
                                        target_version = job.target_version,
                                        "published rewrite: timed out waiting for durable_version, skipping"
                                    );
                                    return Ok(());
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }

                            #[cfg(test)]
                            if async_fail_mode.load(Ordering::Relaxed) == 3 {
                                return Err(MptDbError::Other(
                                    "forced async published baseline failure".to_string(),
                                ));
                            }

                            let published_materialize_start = std::time::Instant::now();

                            // The rewrite snapshot is a full baseline — it doesn't
                            // need the old incremental chain to be caught up.
                            // After activation, the persist worker's next
                            // publish_generation will build deltas on top of the
                            // new baseline.  This mirrors sei-db where the new
                            // snapshot is self-contained after catch-up.

                            // Use pre-built segments from the persist worker if available.
                            // Otherwise fall back to loading from persisted store.
                            // Apply IO rate limiting to prevent starving the frontend.
                            let mut rate_limiter =
                                IoRateLimiter::new(snapshot_write_rate_mb_per_sec);
                            let staged_meta = if let Some(segments) = job.segments {
                                let result = published_baseline
                                    .publish_generation_rate_limited(
                                        None,
                                        job.target_version,
                                        job.state_root,
                                        &segments,
                                        &[],
                                        rate_limiter.as_mut(),
                                    )?;
                                result.meta
                            } else {
                                let manifest = VersionManifest::load(&manifest_path)?;
                                match Self::rewrite_published_snapshot_at_version(
                                    &persisted,
                                    &published_baseline,
                                    &manifest,
                                    job.target_version,
                                )? {
                                    Some(meta) => meta,
                                    None => return Ok(()),
                                }
                            };

                            // Do compact/prune BEFORE activating meta so that
                            // if they fail, the old meta stays consistent.
                            let latest_manifest = VersionManifest::load(&manifest_path)?;
                            let _ = published_baseline.compact_for_manifest(&latest_manifest);
                            if let Some(wal_store) = wal_store.as_ref() {
                                let floor = Self::wal_prune_floor_for_manifest(
                                    &latest_manifest,
                                    &published_baseline,
                                )
                                .unwrap_or(0);
                                if floor > 0 {
                                    wal_store.lock().prune_before(floor)?;
                                }
                            }
                            // Never let a rewrite roll the published pointer
                            // backwards relative to the live incremental chain.
                            let live_published = published_version.load(Ordering::Acquire);
                            if live_published > staged_meta.version {
                                return Ok(());
                            }
                            // Activate meta last — atomic boundary.
                            published_baseline.activate_published_meta(&staged_meta)?;
                            last_published_materialize_micros.store(
                                published_materialize_start.elapsed().as_micros() as u64,
                                Ordering::Release,
                            );
                            let _ = published_version.fetch_update(
                                Ordering::Release,
                                Ordering::Relaxed,
                                |cur| {
                                    if staged_meta.version > cur {
                                        Some(staged_meta.version)
                                    } else {
                                        None
                                    }
                                },
                            );
                            Ok(())
                        })()
                    };

                    if let Err(err) = result {
                        Self::warn_nonfatal_async_error(&err);
                    }

                    if let Some(done) = job.done {
                        let result = if async_error.load(Ordering::Relaxed) {
                            Err(Self::current_async_error(&async_error_detail))
                        } else {
                            Ok(())
                        };
                        let _ = done.send(result);
                    }
                }
            })
            .map_err(|e| MptDbError::Other(format!("spawn published rewrite thread: {e}")))?;
        Ok((tx, handle))
    }

    /// Report a fatal async error that poisons the store.
    ///
    /// Use only for errors that compromise data durability (persist_batch
    /// failure, WAL replay divergence).  Published baseline failures are
    /// non-fatal and should use `warn_nonfatal_async_error` instead.
    fn report_async_error(
        async_error: &AtomicBool,
        detail: &Mutex<Option<String>>,
        err: &MptDbError,
    ) {
        if Self::diagnostics_enabled() {
            eprintln!("[mptdiag] async error (fatal): {err}");
        }
        *detail.lock() = Some(format!("mpt async persist failed: {err}"));
        async_error.store(true, Ordering::Relaxed);
    }

    /// Log a non-fatal async error.
    ///
    /// Published baseline / segment errors do not compromise committed state
    /// (WAL + persisted nodes are intact).  The system continues operating;
    /// L3 cache may degrade but correctness is maintained.  This mirrors
    /// sei-db where snapshot rewrite errors are logged but do not stop the
    /// database.
    fn warn_nonfatal_async_error(err: &MptDbError) {
        tracing::warn!(?err, "non-fatal background published baseline error (continuing)");
    }

    fn check_async_error(&self) -> Result<()> {
        if self.async_error.load(Ordering::Relaxed) {
            Err(Self::current_async_error(&self.async_error_detail))
        } else {
            Ok(())
        }
    }

    /// Block until the background workers have caught up enough to satisfy
    /// the configured lag limits.  This prevents committed_version from
    /// running unboundedly ahead of durable/published, keeping WAL size
    /// bounded and recovery time predictable.
    fn wait_for_backpressure(&self) -> Result<()> {
        let next_version = self.version + 1;

        if self.config.max_durable_lag > 0 {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                self.check_async_error()?;
                let durable = self.durable_version.load(Ordering::Acquire);
                if next_version - durable <= self.config.max_durable_lag {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        next_version,
                        durable,
                        max_lag = self.config.max_durable_lag,
                        "backpressure: timed out waiting for durable_version"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        if self.config.max_published_lag > 0 {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                self.check_async_error()?;
                let published = self.published_version.load(Ordering::Acquire);
                if next_version - published <= self.config.max_published_lag {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        next_version,
                        published,
                        max_lag = self.config.max_published_lag,
                        "backpressure: timed out waiting for published_version"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        if self.config.max_wal_bytes > 0 {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            loop {
                self.check_async_error()?;
                let Some(wal_store) = self.wal_store.as_ref() else {
                    break;
                };
                let wal_bytes = wal_store.lock().size_bytes();
                if wal_bytes <= self.config.max_wal_bytes {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        next_version,
                        wal_bytes,
                        max_wal_bytes = self.config.max_wal_bytes,
                        "backpressure: timed out waiting for WAL size to fall below limit"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        Ok(())
    }

    fn maybe_compact_segment_store(&mut self) -> Result<()> {
        let should_compact = self.version > 0 &&
            ((self.version as usize) % super::published_baseline::PUBLISHED_REWRITE_INTERVAL ==
                0 ||
                self.manifest.earliest_version > 0);
        if !should_compact {
            return Ok(());
        }

        let compacted = self.published_baseline.compact_for_manifest(&self.manifest)?;
        if compacted {
            self.reload_published_view()?;
        }
        Ok(())
    }

    fn checkpoint_path(dir: &Path) -> PathBuf {
        dir.join("account_trie_checkpoint.bin")
    }

    fn fast_storage_root(dir: &Path) -> PathBuf {
        dir.join("fast_storage")
    }

    fn cleanup_tmp_artifacts(path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(path)
            .map_err(|e| MptDbError::Other(format!("read dir {}: {e}", path.display())))?
        {
            let entry = entry.map_err(|e| {
                MptDbError::Other(format!("read dir entry {}: {e}", path.display()))
            })?;
            let entry_path = entry.path();
            let file_type = entry.file_type().map_err(|e| {
                MptDbError::Other(format!("read file type {}: {e}", entry_path.display()))
            })?;

            let is_tmp = entry_path.extension().is_some_and(|ext| ext == "tmp");
            if is_tmp {
                if file_type.is_dir() {
                    fs::remove_dir_all(&entry_path).map_err(|e| {
                        MptDbError::Other(format!("remove tmp dir {}: {e}", entry_path.display()))
                    })?;
                } else {
                    fs::remove_file(&entry_path).map_err(|e| {
                        MptDbError::Other(format!("remove tmp file {}: {e}", entry_path.display()))
                    })?;
                }
                continue;
            }

            if file_type.is_dir() {
                Self::cleanup_tmp_artifacts(&entry_path)?;
            }
        }

        Ok(())
    }

    fn try_load_checkpoint(dir: &Path, version: i64, root: B256) -> Result<Option<MptTree>> {
        let path = Self::checkpoint_path(dir);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        let checkpoint: AccountTrieCheckpoint = match bincode::deserialize(&bytes) {
            Ok(cp) => cp,
            Err(_) => return Ok(None),
        };
        if checkpoint.version != version || checkpoint.root != root {
            return Ok(None);
        }
        Ok(Some(checkpoint.trie))
    }

    fn save_checkpoint(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        if self.replay_materializer {
            return Ok(());
        }
        if self.applied_this_block {
            return Ok(());
        }
        if self.checkpoint_saved_version.load(Ordering::Acquire) >= self.version {
            return Ok(());
        }
        if !self.should_save_checkpoint() {
            return Ok(());
        }
        let Some(checkpoint) = self.build_account_checkpoint()? else {
            return Ok(());
        };
        Self::write_checkpoint_file(&self.dir, &checkpoint)?;
        self.checkpoint_saved_version.store(checkpoint.version, Ordering::Release);
        Ok(())
    }

    fn should_save_checkpoint(&self) -> bool {
        // In wal_first mode, always save checkpoint so cold starts can avoid
        // RocksDB materialization. The account trie is fully resident in memory,
        // so serialization is a fast in-memory copy + bincode encode.
        if self.config.wal_first_commit {
            return self.checkpoint_account_trie_nodes.is_some();
        }
        let Some(node_count) = self.checkpoint_account_trie_nodes else {
            return false;
        };
        node_count <= self.config.checkpoint_max_account_trie_nodes
    }

    fn clear_checkpoint_file(&self) -> Result<()> {
        let path = Self::checkpoint_path(&self.dir);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| {
                MptDbError::Other(format!("remove account trie checkpoint {}: {e}", path.display()))
            })?;
        }
        Ok(())
    }

    fn reset_derived_state_for_new_base(&mut self) -> Result<()> {
        self.clear_checkpoint_file()?;
        self.published_baseline.clear_meta()?;
        self.published_meta = None;
        self.published_store = None;
        self.published_version.store(0, Ordering::Release);
        self.checkpoint_account_trie_nodes = None;
        if let Some(wal_store) = self.wal_store.as_ref() {
            wal_store.lock().truncate_after(0)?;
        }
        Ok(())
    }

    /// Reload the published view from disk.
    ///
    /// In WAL-first mode the published version may lag behind the committed
    /// version.  This is safe because every individual `open_trie` call in the
    /// published store verifies the storage root — stale entries that no longer
    /// match simply miss and fall through to the persisted store.
    fn reload_published_view(&mut self) -> Result<()> {
        let loaded_meta = self.published_baseline.load_meta()?;
        match loaded_meta {
            Some(meta) if meta.version > 0 => {
                self.published_store = self.published_baseline.open_published_store(&meta)?;
                self.published_meta = Some(meta);
            }
            _ => {
                self.published_meta = None;
                self.published_store = None;
            }
        }
        Ok(())
    }

    /// Check if the background persist/rewrite worker has produced a newer
    /// published snapshot than what the frontend currently holds. If so, reload.
    fn maybe_refresh_published_view(&mut self) -> Result<()> {
        let bg_version = self.published_version.load(Ordering::Acquire);
        let current_version = self.published_meta.as_ref().map(|m| m.version).unwrap_or(0);
        if bg_version > current_version {
            self.reload_published_view()?;
        }
        Ok(())
    }

    fn has_current_published_view(&self) -> bool {
        self.published_meta.is_some() && self.published_store.is_some()
    }

    fn start_persist_worker(&mut self) -> Result<()> {
        if self.read_only || self.persist_tx.is_some() {
            return Ok(());
        }

        self.start_checkpoint_worker()?;

        if self.published_rewrite_tx.is_none() {
            let (rewrite_tx, rewrite_handle) = Self::spawn_published_rewrite_worker(
                Arc::clone(&self.persisted),
                Arc::clone(&self.published_baseline),
                self.wal_store.as_ref().map(Arc::clone),
                self.manifest_path.clone(),
                Arc::clone(&self.durable_version),
                Arc::clone(&self.published_version),
                Arc::clone(&self.last_published_materialize_micros),
                Arc::clone(&self.async_error),
                Arc::clone(&self.async_error_detail),
                Duration::from_secs(self.config.published_rewrite_timeout_secs),
                self.config.snapshot_write_rate_mb_per_sec,
                #[cfg(test)]
                Arc::clone(&self.async_fail_mode),
            )?;
            self.published_rewrite_tx = Some(rewrite_tx);
            self.published_rewrite_handle = Some(rewrite_handle);
        }

        let (tx, rx) = crossbeam_channel::bounded::<PersistJob>(self.config.async_queue_depth);
        let persisted_clone = Arc::clone(&self.persisted);
        let published_baseline_clone = Arc::clone(&self.published_baseline);
        let mut worker_published_meta = self.published_meta.clone();
        let async_error_clone = Arc::clone(&self.async_error);
        let async_error_detail_clone = Arc::clone(&self.async_error_detail);
        let durable_version_clone = Arc::clone(&self.durable_version);
        let published_version_clone = Arc::clone(&self.published_version);
        let wal_store_clone = self.wal_store.as_ref().map(Arc::clone);
        let published_rewrite_tx_clone = self.published_rewrite_tx.as_ref().cloned();
        let worker_config = self.config.clone();
        #[cfg(test)]
        let async_fail_mode_clone = Arc::clone(&self.async_fail_mode);

        let handle = std::thread::Builder::new()
            .name("mpt-persist".to_string())
            .spawn(move || {
                // Track a pending rewrite that was dropped due to full queue,
                // so it can be retried on the next durable version advance.
                let mut pending_rewrite: Option<(i64, B256)> = None;
                while let Ok(mut job) = rx.recv() {
                    if async_error_clone.load(Ordering::Relaxed) {
                        if let Some(done) = job.done {
                            let _ = done
                                .send(Err(Self::current_async_error(&async_error_detail_clone)));
                        }
                        continue;
                    }

                    if !job.barrier_only {
                        #[cfg(test)]
                        let forced_error = match async_fail_mode_clone.load(Ordering::Relaxed) {
                            1 => {
                                Some(MptDbError::Other("forced async persist failure".to_string()))
                            }
                            2 => {
                                Some(MptDbError::Other("forced async manifest failure".to_string()))
                            }
                            _ => None,
                        };

                        #[cfg(not(test))]
                        let forced_error: Option<MptDbError> = None;

                        // replay_from_wal is no longer used in normal operation.
                        // WAL-first commits send pre-computed blobs directly.
                        // This flag is kept only for backward compatibility.

                        let result = if let Some(err) = forced_error {
                            Err(err)
                        } else if worker_config.wal_first_commit {
                            // wal_first: skip RocksDB persist_batch — WAL +
                            // published segments provide full durability.
                            // Only save the manifest.
                            if job.save_manifest {
                                job.manifest.save(&job.manifest_path)
                            } else {
                                Ok(())
                            }
                        } else {
                            // Legacy path: persist blobs to RocksDB then manifest.
                            persisted_clone.persist_batch(&job.blobs, true).and_then(|_| {
                                if job.save_manifest {
                                    job.manifest.save(&job.manifest_path)
                                } else {
                                    Ok(())
                                }
                            })
                        };

                        if let Err(e) = result {
                            Self::report_async_error(
                                &async_error_clone,
                                &async_error_detail_clone,
                                &e,
                            );
                            tracing::error!(?e, "background persist failed");
                        } else {
                            if job.publish_baseline && !job.replay_from_wal {
                                let mut publish_puts = job.published_puts.clone();
                                let mut skip_publish = false;

                                // wal_first: build segments from COW trie clones
                                // (sei-db model: serialization in background).
                                //
                                // Freeze each clone first (sole owner → Arc::make_mut
                                // is in-place, O(overlay)).  Then build segments in
                                // parallel via rayon using zero-copy frozen refs —
                                // avoids the extra allocation of collect_all_nodes().
                                if !job.committed_tries.is_empty() {
                                    // Sequential freeze: O(overlay) per trie, cheap.
                                    for (_, _, trie) in &mut job.committed_tries {
                                        trie.snapshot();
                                    }
                                    // Parallel segment build via rayon.
                                    let built: Vec<_> = job
                                        .committed_tries
                                        .par_iter()
                                        .filter_map(|(addr, root, trie)| {
                                            StorageTrieSegment::from_parts(
                                                trie.frozen_arena_nodes_ref(),
                                                trie.frozen_arena_hash_cache_ref(),
                                                trie.root_index(),
                                                *root,
                                            )
                                            .ok()
                                            .map(|seg| (*addr, seg))
                                        })
                                        .collect();
                                    publish_puts.extend(built);
                                }

                                // Legacy path: build segments from RocksDB.
                                if !job.deferred_published_roots.is_empty() {
                                    match Self::build_publish_segments_from_roots(
                                        &persisted_clone,
                                        &job.deferred_published_roots,
                                    ) {
                                        Ok(mut rebuilt) => publish_puts.append(&mut rebuilt),
                                        Err(e) => {
                                            Self::warn_nonfatal_async_error(&e);
                                            skip_publish = true;
                                        }
                                    }
                                }
                                if skip_publish {
                                    // Persist succeeded but we can't build segments.
                                    // Skip publish for this version; L3 will be stale
                                    // but correctness is unaffected.
                                } else {
                                    #[cfg(test)]
                                    let publish_result =
                                        if async_fail_mode_clone.load(Ordering::Relaxed) == 3 {
                                            Err(MptDbError::Other(
                                                "forced async published baseline failure"
                                                    .to_string(),
                                            ))
                                        } else {
                                            published_baseline_clone.publish_generation(
                                                worker_published_meta.as_ref(),
                                                job.version,
                                                job.state_root,
                                                &publish_puts,
                                                &job.published_deletes,
                                            )
                                        };
                                    #[cfg(not(test))]
                                    let publish_result = published_baseline_clone
                                        .publish_generation(
                                            worker_published_meta.as_ref(),
                                            job.version,
                                            job.state_root,
                                            &publish_puts,
                                            &job.published_deletes,
                                        );

                                    let publish_result = match publish_result {
                                        Ok(result) => {
                                            worker_published_meta = Some(result.meta.clone());
                                            Some(result)
                                        }
                                        Err(e) => {
                                            Self::warn_nonfatal_async_error(&e);
                                            None
                                        }
                                    };

                                    let _ = publish_result;

                                    if (job.version as usize) %
                                        super::published_baseline::PUBLISHED_REWRITE_INTERVAL ==
                                        0 ||
                                        job.manifest.earliest_version > 0
                                    {
                                        if let Err(e) = published_baseline_clone
                                            .compact_for_manifest(&job.manifest)
                                        {
                                            Self::warn_nonfatal_async_error(&e);
                                        }
                                    }
                                } // close skip_publish else
                            }

                            if async_error_clone.load(Ordering::Relaxed) {
                                if let Some(done) = job.done {
                                    let _ = done.send(Err(Self::current_async_error(
                                        &async_error_detail_clone,
                                    )));
                                }
                                continue;
                            }

                            if !job.replay_from_wal && !job.barrier_only {
                                let _ = durable_version_clone.fetch_update(
                                    Ordering::Release,
                                    Ordering::Relaxed,
                                    |cur| if job.version > cur { Some(job.version) } else { None },
                                );
                                if let Some(wal_store) = wal_store_clone.as_ref() {
                                    // Quick lock: update durable_version in memory only
                                    // (no disk IO) to minimize Mutex hold time and avoid
                                    // blocking the frontend's append_entry path.
                                    let prune_floor = {
                                        let mut wal = wal_store.lock();
                                        if let Err(e) = wal.set_durable_version(job.version) {
                                            Self::report_async_error(
                                                &async_error_clone,
                                                &async_error_detail_clone,
                                                &e,
                                            );
                                            tracing::error!(
                                                ?e,
                                                "background wal durable watermark update failed"
                                            );
                                            None
                                        } else {
                                            let floor = Self::wal_prune_floor_for_manifest(
                                                &job.manifest,
                                                &published_baseline_clone,
                                            )
                                            .unwrap_or(0);
                                            if floor > 0 {
                                                Some(floor)
                                            } else {
                                                None
                                            }
                                        }
                                    }; // lock dropped here
                                       // Prune WAL segments outside the lock — this involves
                                       // file IO (segment rewrite/delete) and should not block
                                       // the frontend.
                                       // Prune every 16 versions to amortize the file IO cost.
                                       // prune_before rewrites segment files — holding the WAL
                                       // Mutex during this IO was the main contention source
                                       // that caused B4.2's first-block 11ms WAL spike.
                                    if let Some(floor) = prune_floor {
                                        if job.version % 16 == 0 {
                                            let mut wal = wal_store.lock();
                                            let _ = wal.prune_before(floor);
                                        }
                                    }
                                }
                                if worker_config.wal_first_commit {
                                    // In wal_first mode, incremental publish_generation
                                    // keeps segments up to date on every block.
                                    // Auto-consolidation at REWRITE_INTERVAL depth
                                    // bounds the delta chain.  No separate full rewrite
                                    // needed — this matches sei-db's model where
                                    // snapshot rewrite is triggered from the frontend
                                    // with a tree clone, not from a background worker
                                    // reading RocksDB.
                                    if let Some(rewrite_tx) = published_rewrite_tx_clone.as_ref() {
                                        let _ = rewrite_tx;
                                        // Rewrite scheduling disabled: the rewrite worker
                                        // would need RocksDB data that is no longer written
                                        // in wal_first mode. Incremental publish provides
                                        // equivalent coverage.
                                    }
                                } else if !worker_config.wal_first_commit {
                                    // Legacy path: schedule full rewrites from RocksDB.
                                    if let Some(rewrite_tx) = published_rewrite_tx_clone.as_ref() {
                                        let should_schedule = if pending_rewrite.is_some() {
                                            true
                                        } else {
                                            let published_cur =
                                                published_version_clone.load(Ordering::Acquire);
                                            Self::should_rewrite_published_snapshot(
                                                &worker_config,
                                                published_cur,
                                                job.version,
                                            )
                                        };
                                        if should_schedule {
                                            match Self::schedule_published_rewrite(
                                                rewrite_tx,
                                                job.version,
                                                job.state_root,
                                                None,
                                            ) {
                                                Ok(sent) => {
                                                    if sent {
                                                        pending_rewrite = None;
                                                    } else {
                                                        pending_rewrite =
                                                            Some((job.version, job.state_root));
                                                    }
                                                }
                                                Err(e) => {
                                                    Self::warn_nonfatal_async_error(&e);
                                                }
                                            }
                                        }
                                    }
                                }
                                if job.publish_baseline {
                                    published_version_clone.store(job.version, Ordering::Release);
                                }
                            }
                        }
                    }

                    if let Some(done) = job.done {
                        let result = if async_error_clone.load(Ordering::Relaxed) {
                            Err(Self::current_async_error(&async_error_detail_clone))
                        } else {
                            Ok(())
                        };
                        let _ = done.send(result);
                    }
                }
            })
            .map_err(|e| MptDbError::Other(format!("spawn persist thread: {e}")))?;

        self.persist_tx = Some(tx);
        self.persist_handle = Some(handle);
        Ok(())
    }

    fn shutdown(&mut self, best_effort: bool) -> Result<()> {
        if self.shutdown_complete {
            return if best_effort { Ok(()) } else { self.check_async_error() };
        }

        let diagnostics = Self::diagnostics_enabled();
        if best_effort {
            let flush_start = diagnostics.then(std::time::Instant::now);
            let _ = self.flush_persist();
            if let Some(start) = flush_start {
                eprintln!("[mptdiag] shutdown flush_persist(best_effort) {:?}", start.elapsed());
            }
        } else {
            let flush_start = diagnostics.then(std::time::Instant::now);
            self.flush_persist()?;
            if let Some(start) = flush_start {
                eprintln!("[mptdiag] shutdown flush_persist {:?}", start.elapsed());
            }
        }

        if self.checkpoint_saved_version.load(Ordering::Acquire) < self.version {
            let _ = self.schedule_checkpoint_save();
            self.pump_pending_checkpoint();
        }

        self.persist_tx.take();
        self.published_rewrite_tx.take();
        if let Some(handle) = self.persist_handle.take() {
            let join_start = diagnostics.then(std::time::Instant::now);
            if best_effort {
                let _ = handle.join();
            } else {
                handle
                    .join()
                    .map_err(|_| MptDbError::Other("persist worker panicked".to_string()))?;
            }
            if let Some(start) = join_start {
                eprintln!("[mptdiag] shutdown join_worker {:?}", start.elapsed());
            }
        }
        if let Some(handle) = self.published_rewrite_handle.take() {
            if best_effort {
                let _ = handle.join();
            } else {
                handle.join().map_err(|_| {
                    MptDbError::Other("published rewrite worker panicked".to_string())
                })?;
            }
        }
        self.checkpoint_save_tx.take();
        if let Some(handle) = self.checkpoint_save_handle.take() {
            let join_start = diagnostics.then(std::time::Instant::now);
            if best_effort {
                let _ = handle.join();
            } else {
                handle
                    .join()
                    .map_err(|_| MptDbError::Other("checkpoint worker panicked".to_string()))?;
            }
            if let Some(start) = join_start {
                eprintln!("[mptdiag] shutdown join checkpoint {:?}", start.elapsed());
            }
        }

        let checkpoint_start = diagnostics.then(std::time::Instant::now);
        if best_effort {
            let _ = self.save_checkpoint();
        } else {
            self.save_checkpoint()?;
        }
        if let Some(start) = checkpoint_start {
            if best_effort {
                eprintln!("[mptdiag] shutdown save_checkpoint(best_effort) {:?}", start.elapsed());
            } else {
                eprintln!("[mptdiag] shutdown save_checkpoint {:?}", start.elapsed());
            }
        }

        // Flush WAL meta now that all background work is done and
        // durable_version is final.  This was deferred from the worker's
        // set_durable_version to reduce Mutex contention.
        if let Some(wal_store) = self.wal_store.as_ref() {
            if best_effort {
                let _ = wal_store.lock().flush_meta();
            } else {
                wal_store.lock().flush_meta()?;
            }
        }

        if let Some(persisted) = Arc::get_mut(&mut self.persisted) {
            if best_effort {
                let _ = persisted.close();
            } else {
                persisted.close()?;
            }
        }

        self.file_lock = None;
        self.shutdown_complete = true;

        if best_effort {
            Ok(())
        } else {
            self.check_async_error()
        }
    }

    fn load_account_trie_snapshot(
        dir: &Path,
        persisted: &PersistedTrieStore,
        version: i64,
        root: B256,
    ) -> Result<(StorageTrieCow, bool)> {
        match Self::try_load_checkpoint(dir, version, root)? {
            Some(trie) => Ok((StorageTrieCow::from_tree(trie), true)),
            None => Ok((
                if root == EMPTY_ROOT_HASH {
                    StorageTrieCow::empty()
                } else {
                    // Fully materialize the account trie into memory at startup,
                    // matching sei-db's model where trees are always resident.
                    // This eliminates lazy loading during block processing:
                    // preload_paths, apply_change, and root_hash all become
                    // pure in-memory operations.
                    let tree = super::persisted::load_tree_from_root(persisted, root)?;
                    StorageTrieCow::from_tree(tree)
                },
                false,
            )),
        }
    }

    fn select_published_view_for_version(
        published_baseline: &PublishedBaselineManager,
        version: i64,
        root: B256,
    ) -> Result<(Option<PublishedBaselineMeta>, Option<PublishedBaselineReader>)> {
        let selected = match published_baseline.load_meta()? {
            Some(meta) if meta.version == version && meta.root == root => Some(meta),
            _ => published_baseline.meta_for_version(version, root)?,
        };
        if let Some(meta) = selected {
            let store = published_baseline.open_published_store(&meta)?;
            if store.is_some() {
                return Ok((Some(meta), store));
            }
        }
        Ok((None, None))
    }

    /// Find the best available snapshot version `<= target` that can serve as a
    /// replay base.  Returns `(snapshot_version, needs_wal_replay)`.
    ///
    /// This mirrors sei-db's `seekSnapshot(dir, targetVersion)`:
    /// - If the target version is directly loadable from persisted, use it.
    /// - Otherwise find the highest durable version `<= target` and replay WAL from there.
    ///
    /// NOTE: The current single-durable model treats `durable_version` as the
    /// only snapshot.  When periodic snapshot rewrite is implemented (producing
    /// multiple snapshot directories like sei-db), this must be extended to
    /// scan all available snapshots and pick `max(V) <= target`.
    fn seek_best_snapshot_version(
        &self,
        manifest: &VersionManifest,
        target_version: i64,
    ) -> (i64, bool) {
        let durable_version = if self.config.wal_first_commit {
            self.wal_recovery_base_version(manifest.latest_version)
        } else {
            manifest.latest_version
        };

        if durable_version >= target_version {
            // Target is already fully durable — load directly from persisted.
            (target_version, false)
        } else {
            // Target is ahead of durable — need WAL replay from durable.
            (durable_version, true)
        }
    }

    fn recover_target_state(
        &self,
        manifest: &VersionManifest,
        target_version: i64,
    ) -> Result<(StorageTrieCow, bool, i64)> {
        let (snapshot_version, needs_replay) =
            self.seek_best_snapshot_version(manifest, target_version);

        if needs_replay {
            // Verify WAL covers the replay range (snapshot_version, target_version].
            let wal_store = self.wal_store.as_ref().map(Arc::clone).ok_or_else(|| {
                MptDbError::Other("wal-first recovery requires wal store".to_string())
            })?;
            {
                let wal = wal_store.lock();
                let wal_earliest = wal.earliest_version();
                let wal_latest = wal.latest_version();
                if wal.is_empty() ||
                    wal_earliest > snapshot_version + 1 ||
                    wal_latest < target_version
                {
                    return Err(MptDbError::Other(format!(
                        "load_version {target_version}: no snapshot+WAL chain covers target \
                         (best snapshot={snapshot_version}, WAL range=[{wal_earliest}, {wal_latest}])"
                    )));
                }
            }

            let mut shadow = Self::open_replay_materializer_state(
                &self.dir,
                Arc::clone(&self.persisted),
                Arc::clone(&self.published_baseline),
                Some(wal_store),
                manifest.clone(),
                self.config.clone(),
                snapshot_version,
            )?;
            shadow.replay_wal_catchup_to(manifest, target_version)?;
            #[cfg(test)]
            let loaded_from_checkpoint = shadow.loaded_from_checkpoint;
            #[cfg(not(test))]
            let loaded_from_checkpoint = false;
            Ok((shadow.account_trie.committed().clone(), loaded_from_checkpoint, snapshot_version))
        } else {
            let root = manifest.get_root(target_version).unwrap_or(EMPTY_ROOT_HASH);
            let (account_trie, loaded_from_checkpoint) =
                Self::load_account_trie_snapshot(&self.dir, &self.persisted, target_version, root)?;
            let durable_version = if self.config.wal_first_commit {
                self.wal_recovery_base_version(manifest.latest_version)
            } else {
                manifest.latest_version
            };
            Ok((account_trie, loaded_from_checkpoint, durable_version))
        }
    }

    fn restore_version_state(
        &mut self,
        manifest: VersionManifest,
        version: i64,
        account_trie: StorageTrieCow,
        loaded_from_checkpoint: bool,
    ) -> Result<()> {
        #[cfg(not(test))]
        let _ = loaded_from_checkpoint;
        let checkpoint_account_trie_nodes = if loaded_from_checkpoint {
            Some(account_trie.arena_len())
        } else if matches!(account_trie.root_ref(), CowRootRef::Empty) {
            Some(0)
        } else {
            None
        };
        self.manifest = manifest;
        self.version = version;
        self.account_trie = AccountTrieHandle::snapshot(version, account_trie);
        self.checkpoint_account_trie_nodes = checkpoint_account_trie_nodes;
        self.dirty_accounts.clear();
        self.clear_storage_trie_state();
        let root = self.manifest.get_root(version).unwrap_or(EMPTY_ROOT_HASH);
        let (published_meta, published_store) =
            Self::select_published_view_for_version(&self.published_baseline, version, root)?;
        self.published_meta = published_meta;
        self.published_store = published_store;
        self.published_version.store(
            self.published_meta.as_ref().map(|meta| meta.version).unwrap_or(0),
            Ordering::Release,
        );
        self.applied_this_block = false;
        self.poisoned = false;
        #[cfg(test)]
        {
            self.loaded_from_checkpoint = loaded_from_checkpoint;
        }
        Ok(())
    }

    fn open_replay_materializer_state(
        dir: &Path,
        persisted: Arc<PersistedTrieStore>,
        published_baseline: Arc<PublishedBaselineManager>,
        wal_store: Option<Arc<Mutex<CommitWalStore>>>,
        manifest: VersionManifest,
        config: MptConfig,
        version: i64,
    ) -> Result<Self> {
        let manifest = Self::truncate_manifest_to_version(&manifest, version);
        let root = manifest.get_root(version).unwrap_or(EMPTY_ROOT_HASH);
        let (account_trie, loaded_from_checkpoint) =
            Self::load_account_trie_snapshot(dir, &persisted, version, root)?;
        #[cfg(not(test))]
        let _ = loaded_from_checkpoint;

        let (published_meta, published_store) =
            Self::select_published_view_for_version(&published_baseline, version, root)?;
        let published_version = published_meta.as_ref().map(|meta| meta.version).unwrap_or(0);

        let mut replay_config = config;
        replay_config.wal_first_commit = false;
        replay_config.wal_shadow_validate = false;
        replay_config.async_blob_threshold = 0;
        // Aggressive parallelism for replay materializer.
        replay_config.parallel_storage_tries_min = 4;
        replay_config.parallel_account_frontier_min = 2;
        let checkpoint_account_trie_nodes = if loaded_from_checkpoint {
            Some(account_trie.arena_len())
        } else if root == EMPTY_ROOT_HASH {
            Some(0)
        } else {
            None
        };

        Ok(Self {
            dir: dir.to_path_buf(),
            manifest_path: dir.join("manifest.json"),
            account_trie: AccountTrieHandle::snapshot(version, account_trie),
            storage_trie_handles: HashMap::new(),
            storage_trie_cache: Self::new_storage_trie_cache(
                replay_config.storage_trie_cache_capacity,
            ),
            dirty_accounts: Vec::new(),
            persisted,
            published_baseline,
            published_meta,
            published_store,
            manifest,
            wal_store,
            version,
            initial_version: 1,
            applied_this_block: false,
            poisoned: false,
            read_only: false,
            replay_materializer: true,
            file_lock: None,
            parallelism: ParallelismThresholds {
                storage_tries_min: replay_config.parallel_storage_tries_min,
                account_frontier_min: replay_config.parallel_account_frontier_min,
            },
            config: replay_config,
            bulk_load: None,
            bulk_segment_writer: None,
            durable_version: Arc::new(AtomicI64::new(version)),
            published_version: Arc::new(AtomicI64::new(published_version)),
            last_wal_replay_micros: Arc::new(AtomicU64::new(0)),
            last_durable_materialize_micros: Arc::new(AtomicU64::new(0)),
            last_published_materialize_micros: Arc::new(AtomicU64::new(0)),
            persist_tx: None,
            persist_handle: None,
            published_rewrite_tx: None,
            published_rewrite_handle: None,
            checkpoint_save_tx: None,
            checkpoint_save_handle: None,
            checkpoint_saved_version: Arc::new(AtomicI64::new(if loaded_from_checkpoint {
                version
            } else {
                0
            })),
            async_error: Arc::new(AtomicBool::new(false)),
            async_error_detail: Arc::new(Mutex::new(None)),
            pending_checkpoint: None,
            last_apply_duration: Duration::ZERO,
            last_apply_collect_dirty_accounts: Duration::ZERO,
            last_apply_get_or_load_storage_tries: Duration::ZERO,
            last_apply_account_trie_checkout: Duration::ZERO,
            last_apply_ensure_storage: Duration::ZERO,
            last_apply_storage_root_lookup: Duration::ZERO,
            last_apply_storage_slot_updates: Duration::ZERO,
            last_apply_l3_latest_load: Duration::ZERO,
            last_apply_l3_published_load: Duration::ZERO,
            last_apply_l3_into_tree: Duration::ZERO,
            last_apply_published_refreshes: 0,
            last_apply_l2_hits: 0,
            last_apply_l3_latest_hits: 0,
            last_apply_l3_published_hits: 0,
            last_apply_l3_published_post_flush_hits: 0,
            last_apply_node_fallback_loads: 0,
            last_apply_slot_inserts: 0,
            last_apply_slot_deletes: 0,
            last_apply_leaf_splits: 0,
            last_apply_extension_splits: 0,
            last_apply_branch_collapse_to_empty: 0,
            last_apply_branch_collapse_to_leaf: 0,
            last_apply_branch_collapse_to_extension: 0,
            last_apply_extension_leaf_merges: 0,
            last_apply_extension_extension_merges: 0,
            last_wal_append_lock_wait: Duration::ZERO,
            last_wal_append_write: Duration::ZERO,
            last_commit_profile: CommitProfile::default(),
            checkpoint_account_trie_nodes,
            shutdown_complete: false,

            #[cfg(test)]
            loaded_from_checkpoint,
            #[cfg(test)]
            fail_point: None,
            #[cfg(test)]
            async_fail_mode: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        })
    }

    fn truncate_manifest_to_version(
        manifest: &VersionManifest,
        target_version: i64,
    ) -> VersionManifest {
        let mut truncated = manifest.clone();
        truncated.truncate_after(target_version);
        truncated
    }

    /// Batch size for pre-loading WAL entries during replay.  Entries are
    /// loaded in chunks to amortize lock acquisition and disk I/O while
    /// keeping memory bounded.
    const REPLAY_PREFETCH_BATCH: usize = 64;

    fn replay_wal_catchup_to(
        &mut self,
        committed_manifest: &VersionManifest,
        target_version: i64,
    ) -> Result<()> {
        if self.version >= target_version {
            return Ok(());
        }
        let wal_store = self.wal_store.as_ref().map(Arc::clone).ok_or_else(|| {
            MptDbError::Other("wal-first recovery requires wal store".to_string())
        })?;
        let base_version = self.version;
        let original_config = self.config.clone();
        self.manifest = Self::truncate_manifest_to_version(committed_manifest, base_version);

        // Use aggressive parallelism thresholds during replay to maximize
        // throughput.  Normal commit uses conservative thresholds to avoid
        // overhead for small blocks, but replay processes many versions
        // sequentially so every bit of per-version parallelism helps.
        let mut replay_config = original_config.clone();
        replay_config.wal_first_commit = false;
        replay_config.wal_shadow_validate = false;
        replay_config.async_blob_threshold = 0;
        replay_config.parallel_storage_tries_min = 4;
        replay_config.parallel_account_frontier_min = 2;
        self.config = replay_config;

        let replay_result = (|| -> Result<()> {
            let mut next_version = base_version + 1;
            while next_version <= target_version {
                // Pre-fetch a batch of WAL entries to amortize lock + I/O.
                let batch_end =
                    (next_version + Self::REPLAY_PREFETCH_BATCH as i64 - 1).min(target_version);
                let entries = {
                    let wal = wal_store.lock();
                    let mut batch = Vec::with_capacity((batch_end - next_version + 1) as usize);
                    for version in next_version..=batch_end {
                        let entry = wal.load_entry(version)?.ok_or_else(|| {
                            MptDbError::Other(format!(
                                "missing wal entry during replay: version {version}"
                            ))
                        })?;
                        batch.push(entry);
                    }
                    batch
                };

                for entry in &entries {
                    self.apply_wal_entry_inner(entry)?;
                    let (replayed_version, replayed_root) = self.commit_inner()?;
                    if replayed_version != entry.version || replayed_root != entry.state_root {
                        return Err(MptDbError::Other(format!(
                            "wal replay divergence at version {}: got ({}, {}), expected ({}, {})",
                            entry.version,
                            replayed_version,
                            replayed_root,
                            entry.version,
                            entry.state_root
                        )));
                    }
                }
                next_version = batch_end + 1;
            }
            Ok(())
        })();

        self.config = original_config;
        if replay_result.is_ok() {
            self.manifest = committed_manifest.clone();
            self.reload_published_view()?;
            self.poisoned = false;
            self.applied_this_block = false;
        }
        replay_result
    }

    pub fn load_version_target(&mut self, target_version: i64) -> Result<()> {
        // Wait for any in-flight persist jobs to complete before reloading from disk.
        self.flush_persist()?;

        let manifest = VersionManifest::load(&self.manifest_path)?;
        let committed_version = manifest.latest_version;
        let target_version = if target_version == 0 { committed_version } else { target_version };
        if target_version < manifest.earliest_version || target_version > committed_version {
            return Err(MptDbError::Other(format!(
                "load_version target {} out of range [{}, {}]",
                target_version, manifest.earliest_version, committed_version
            )));
        }

        let (account_trie, loaded_from_checkpoint, durable_version) =
            self.recover_target_state(&manifest, target_version)?;
        self.restore_version_state(manifest, target_version, account_trie, loaded_from_checkpoint)?;

        self.durable_version.store(durable_version, Ordering::Release);
        Ok(())
    }

    fn validate_shadow_wal_replay(&self, entry: &CommitWalEntry) -> Result<()> {
        let wal_store = self.wal_store.as_ref().map(Arc::clone).ok_or_else(|| {
            MptDbError::Other("wal shadow validation requires wal store".to_string())
        })?;
        let base_version = self.wal_recovery_base_version(self.version);
        let mut committed_manifest = self.manifest.clone();
        committed_manifest.add_version(entry.version, entry.state_root)?;

        let mut shadow = Self::open_replay_materializer_state(
            &self.dir,
            Arc::clone(&self.persisted),
            Arc::clone(&self.published_baseline),
            Some(wal_store),
            committed_manifest.clone(),
            self.config.clone(),
            base_version,
        )?;
        shadow.replay_wal_catchup_to(&committed_manifest, entry.version)
    }

    fn build_published_update_from_wal_entry(
        &self,
        entry: &CommitWalEntry,
    ) -> Result<(Vec<(B256, StorageTrieSegment)>, Vec<B256>)> {
        let deleted_accounts: HashSet<B256> = entry.deleted_accounts.iter().copied().collect();
        let mut deferred_roots = Vec::new();
        let mut published_deletes = entry.deleted_accounts.clone();

        for account in &entry.accounts {
            if deleted_accounts.contains(&account.hashed_address) {
                continue;
            }
            if !account.storage_wiped && account.storage_changes.is_empty() {
                continue;
            }
            let storage_root = self.get_existing_storage_root(&account.hashed_address);
            if storage_root == EMPTY_ROOT_HASH {
                published_deletes.push(account.hashed_address);
            } else {
                deferred_roots.push((account.hashed_address, storage_root));
            }
        }

        published_deletes.sort_unstable();
        published_deletes.dedup();
        let publish_puts =
            Self::build_publish_segments_from_roots(&self.persisted, &deferred_roots)?;
        Ok((publish_puts, published_deletes))
    }

    /// Open an MptCommitStore at the given directory with default configuration.
    ///
    /// `read_only=true` disables writes and does not acquire the exclusive lock.
    pub fn open(dir: &Path, read_only: bool) -> Result<Self> {
        Self::open_with_config(dir, read_only, MptConfig::default())
    }

    pub fn open_at_version(
        dir: &Path,
        read_only: bool,
        target_version: i64,
        overwrite: bool,
    ) -> Result<Self> {
        Self::open_with_config_at_version(
            dir,
            read_only,
            MptConfig::default(),
            target_version,
            overwrite,
        )
    }

    /// Open an MptCommitStore at the given directory with custom configuration.
    ///
    /// `read_only=true` disables writes and does not acquire the exclusive lock.
    pub fn open_with_config(dir: &Path, read_only: bool, config: MptConfig) -> Result<Self> {
        // Ensure directories exist
        fs::create_dir_all(dir)
            .map_err(|e| MptDbError::Other(format!("create dir {}: {e}", dir.display())))?;
        let trie_nodes_dir = dir.join("trie_nodes");
        fs::create_dir_all(&trie_nodes_dir)
            .map_err(|e| MptDbError::Other(format!("create trie_nodes dir: {e}")))?;

        // Lock: exclusive for writer, shared for reader
        let file_lock = {
            let lock_path = dir.join("LOCK");
            let lock_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| MptDbError::Other(format!("open LOCK file: {e}")))?;
            if read_only {
                lock_file.try_lock_shared().map_err(|e| {
                    MptDbError::Other(format!("failed to acquire shared lock: {e}"))
                })?;
            } else {
                lock_file
                    .try_lock_exclusive()
                    .map_err(|e| MptDbError::Other(format!("failed to lock db: {e}")))?;
            }
            Some(lock_file)
        };

        if !read_only {
            Self::cleanup_tmp_artifacts(dir)?;
        }

        let manifest_path = dir.join("manifest.json");
        let mut manifest = VersionManifest::load(&manifest_path)?;
        let wal_store = if config.wal_first_commit {
            Some(Arc::new(Mutex::new(CommitWalStore::open(dir)?)))
        } else {
            None
        };

        // In wal_first mode, the WAL may contain entries beyond the manifest
        // (committed to WAL but the persist worker hadn't saved the manifest
        // before crash). Extend the manifest with those WAL entries so they
        // are included in the replay range, recovering all committed work.
        if config.wal_first_commit {
            if let Some(ref wal_store) = wal_store {
                let wal = wal_store.lock();
                let wal_latest = wal.latest_version();
                if wal_latest > manifest.latest_version {
                    for v in (manifest.latest_version + 1)..=wal_latest {
                        if let Some(entry) = wal.load_entry(v)? {
                            manifest.add_version(v, entry.state_root)?;
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        let persisted = Arc::new(PersistedTrieStore::open_with_capacity(
            &trie_nodes_dir,
            config.persisted_node_cache_capacity,
        )?);

        let published_baseline =
            Arc::new(PublishedBaselineManager::open(&Self::fast_storage_root(dir))?);
        let mut published_meta = None;
        let mut published_store = None;

        if let Some(meta) = published_baseline.load_meta()? {
            if manifest.get_root(meta.version) == Some(meta.root) {
                published_store = published_baseline.open_published_store(&meta)?;
                published_meta = Some(meta);
            }
        }

        let committed_version = manifest.latest_version;
        let durable_on_disk = wal_store
            .as_ref()
            .map(|store| {
                let wal = store.lock();
                if wal.is_empty() {
                    committed_version
                } else {
                    wal.durable_version()
                }
            })
            .unwrap_or(committed_version);
        let version = durable_on_disk.min(committed_version);
        let root = manifest.get_root(version).unwrap_or(EMPTY_ROOT_HASH);
        let (account_trie, account_loaded_from_checkpoint) =
            Self::load_account_trie_snapshot(dir, &persisted, version, root)?;
        #[cfg(not(test))]
        let _ = account_loaded_from_checkpoint;

        let parallelism = ParallelismThresholds {
            storage_tries_min: config.parallel_storage_tries_min,
            account_frontier_min: config.parallel_account_frontier_min,
        };

        let async_error = Arc::new(AtomicBool::new(false));
        let async_error_detail = Arc::new(Mutex::new(None));
        let durable_version = Arc::new(AtomicI64::new(version));
        let published_version =
            Arc::new(AtomicI64::new(published_meta.as_ref().map(|meta| meta.version).unwrap_or(0)));
        let last_wal_replay_micros = Arc::new(AtomicU64::new(0));
        let last_durable_materialize_micros = Arc::new(AtomicU64::new(0));
        let last_published_materialize_micros = Arc::new(AtomicU64::new(0));
        let checkpoint_saved_version =
            Arc::new(AtomicI64::new(if account_loaded_from_checkpoint { version } else { 0 }));
        #[cfg(test)]
        let async_fail_mode = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let checkpoint_account_trie_nodes = if account_loaded_from_checkpoint {
            Some(account_trie.arena_len())
        } else if root == EMPTY_ROOT_HASH {
            Some(0)
        } else {
            None
        };

        let mut store = Self {
            dir: dir.to_path_buf(),
            manifest_path,
            account_trie: AccountTrieHandle::snapshot(version, account_trie),
            storage_trie_handles: HashMap::new(),
            storage_trie_cache: Self::new_storage_trie_cache(config.storage_trie_cache_capacity),
            dirty_accounts: Vec::new(),
            persisted,
            published_baseline,
            published_meta,
            published_store,
            manifest: manifest.clone(),
            wal_store,
            version,
            initial_version: 1,
            applied_this_block: false,
            poisoned: false,
            read_only,
            replay_materializer: false,
            file_lock,
            parallelism,
            config,
            bulk_load: None,
            bulk_segment_writer: None,
            durable_version,
            published_version,
            last_wal_replay_micros,
            last_durable_materialize_micros,
            last_published_materialize_micros,
            persist_tx: None,
            persist_handle: None,
            published_rewrite_tx: None,
            published_rewrite_handle: None,
            checkpoint_save_tx: None,
            checkpoint_save_handle: None,
            checkpoint_saved_version,
            async_error,
            async_error_detail,
            pending_checkpoint: None,
            last_apply_duration: Duration::ZERO,
            last_apply_collect_dirty_accounts: Duration::ZERO,
            last_apply_get_or_load_storage_tries: Duration::ZERO,
            last_apply_account_trie_checkout: Duration::ZERO,
            last_apply_ensure_storage: Duration::ZERO,
            last_apply_storage_root_lookup: Duration::ZERO,
            last_apply_storage_slot_updates: Duration::ZERO,
            last_apply_l3_latest_load: Duration::ZERO,
            last_apply_l3_published_load: Duration::ZERO,
            last_apply_l3_into_tree: Duration::ZERO,
            last_apply_published_refreshes: 0,
            last_apply_l2_hits: 0,
            last_apply_l3_latest_hits: 0,
            last_apply_l3_published_hits: 0,
            last_apply_l3_published_post_flush_hits: 0,
            last_apply_node_fallback_loads: 0,
            last_apply_slot_inserts: 0,
            last_apply_slot_deletes: 0,
            last_apply_leaf_splits: 0,
            last_apply_extension_splits: 0,
            last_apply_branch_collapse_to_empty: 0,
            last_apply_branch_collapse_to_leaf: 0,
            last_apply_branch_collapse_to_extension: 0,
            last_apply_extension_leaf_merges: 0,
            last_apply_extension_extension_merges: 0,
            last_wal_append_lock_wait: Duration::ZERO,
            last_wal_append_write: Duration::ZERO,
            last_commit_profile: CommitProfile::default(),
            checkpoint_account_trie_nodes,
            shutdown_complete: false,

            #[cfg(test)]
            loaded_from_checkpoint: account_loaded_from_checkpoint,
            #[cfg(test)]
            fail_point: None,
            #[cfg(test)]
            async_fail_mode,
        };

        if store.config.wal_first_commit && version < committed_version {
            store.replay_wal_catchup_to(&manifest, committed_version)?;
        }

        if !read_only {
            store.start_persist_worker()?;
        }

        Ok(store)
    }

    pub fn open_with_config_at_version(
        dir: &Path,
        read_only: bool,
        config: MptConfig,
        target_version: i64,
        overwrite: bool,
    ) -> Result<Self> {
        if overwrite && read_only {
            return Err(MptDbError::Other(
                "cannot open with overwrite in read-only mode".to_string(),
            ));
        }
        if overwrite && target_version <= 0 {
            return Err(MptDbError::Other(
                "overwrite requires a positive target version".to_string(),
            ));
        }

        if overwrite {
            // LoadForOverwriting: truncate WAL + manifest + published BEFORE
            // opening, so that open_with_config loads directly at the target
            // version without replaying entries that will be discarded.
            // This mirrors sei-db's atomic LoadForOverwriting pattern:
            //   1. Truncate WAL after target
            //   2. Truncate manifest after target
            //   3. Prune published beyond target
            //   4. Open at the (now truncated) latest version
            Self::truncate_to_version_on_disk(dir, target_version, &config)?;
            Self::open_with_config(dir, read_only, config)
        } else {
            let mut store = Self::open_with_config(dir, read_only, config)?;
            if target_version > 0 {
                store.load_version_target(target_version)?;
            }
            Ok(store)
        }
    }

    /// Pre-open truncation for LoadForOverwriting: truncate WAL, manifest,
    /// and published baseline on disk so that subsequent `open_with_config`
    /// loads directly at the target version.
    ///
    /// Mirrors sei-db's `LoadForOverwriting` which atomically:
    ///   - downgrades current symlink
    ///   - truncates WAL
    ///   - prunes newer snapshots
    fn truncate_to_version_on_disk(
        dir: &Path,
        target_version: i64,
        config: &MptConfig,
    ) -> Result<()> {
        // Truncate manifest.
        let manifest_path = dir.join("manifest.json");
        if manifest_path.exists() {
            let mut manifest = VersionManifest::load(&manifest_path)?;
            if target_version <= manifest.latest_version {
                manifest.truncate_after(target_version);
                manifest.save(&manifest_path)?;
            }
        }

        // Truncate WAL.
        if config.wal_first_commit {
            let mut wal_store = CommitWalStore::open(dir)?;
            wal_store.truncate_after(target_version)?;
        }

        // Activate published baseline at target version (prunes newer generations).
        let published_baseline = PublishedBaselineManager::open(dir)?;
        let manifest = VersionManifest::load(&manifest_path)?;
        let target_root = manifest.get_root(target_version).unwrap_or(EMPTY_ROOT_HASH);
        published_baseline.activate_published_version(target_version, target_root)?;
        let _ = published_baseline.compact_for_manifest(&manifest);

        Ok(())
    }

    /// Try to extract storage_root from an existing account leaf in the trie.
    fn get_existing_storage_root(&self, hashed_address: &B256) -> B256 {
        let key = Nibbles::unpack(hashed_address);
        match self
            .account_trie
            .current_for_read(self.current_working_version())
            .get(&self.persisted, &key)
        {
            Ok(Some(rlp_bytes)) => {
                // Decode TrieAccount RLP to extract storage_root
                match alloy_rlp::Decodable::decode(&mut &rlp_bytes[..]) {
                    Ok(trie_account) => {
                        let ta: alloy_trie::TrieAccount = trie_account;
                        ta.storage_root
                    }
                    Err(_) => EMPTY_ROOT_HASH,
                }
            }
            Ok(None) => EMPTY_ROOT_HASH,
            Err(_) => EMPTY_ROOT_HASH,
        }
    }

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(MptDbError::Other("store is read-only".to_string()));
        }
        Ok(())
    }

    fn check_not_poisoned(&self) -> Result<()> {
        if self.poisoned {
            return Err(MptDbError::Other(
                "store is poisoned, call load_version() to recover".to_string(),
            ));
        }
        self.check_async_error()
    }

    fn check_not_applied(&self) -> Result<()> {
        if self.applied_this_block {
            return Err(MptDbError::Other(
                "cannot perform this operation while a block is being applied".to_string(),
            ));
        }
        Ok(())
    }

    fn check_not_bulk_loading(&self) -> Result<()> {
        if self.bulk_load.is_some() {
            return Err(MptDbError::Other(
                "store is in bulk-load mode, use bulk_ingest_bundle_chunk()/finish_bulk_load()"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn default_commit_mode(&self) -> CommitExecutionMode {
        if self.replay_materializer {
            CommitExecutionMode {
                wal_first: false,
                allow_async: false,
                save_manifest: false,
                publish_baseline: false,
            }
        } else {
            CommitExecutionMode {
                wal_first: self.config.wal_first_commit,
                allow_async: !self.config.wal_first_commit,
                save_manifest: true,
                // Always publish segments so the mmap-backed published store
                // stays current. In wal_first mode segments are built from
                // in-memory tries on the frontend and published by the
                // background worker each block.
                publish_baseline: true,
            }
        }
    }

    /// Wait for all in-flight background persist jobs to complete.
    ///
    /// Sends a barrier job through the channel and waits for it to be
    /// processed. Since the channel is FIFO, all previously sent jobs will
    /// have been completed by the time the barrier finishes. The barrier
    /// itself does not perform any extra RocksDB or manifest writes.
    pub fn flush_persist(&self) -> Result<()> {
        if let Err(err) = self.check_async_error() {
            if self.config.wal_first_commit {
                self.durable_version.store(self.wal_durable_version(), Ordering::Release);
            }
            return Err(err);
        }
        if self.durable_version.load(Ordering::Acquire) < self.version {
            if let Some(ref tx) = self.persist_tx {
                let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<()>>(0);
                let job = PersistJob {
                    barrier_only: true,
                    replay_from_wal: false,
                    blobs: vec![],
                    published_puts: vec![],
                    deferred_published_roots: vec![],
                    published_deletes: vec![],
                    publish_baseline: false,
                    state_root: EMPTY_ROOT_HASH,
                    manifest: self.manifest.clone(),
                    manifest_path: self.manifest_path.clone(),
                    save_manifest: false,
                    version: self.version,
                    done: Some(done_tx),
                    committed_tries: vec![],
                };
                if tx.send(job).is_ok() {
                    match done_rx.recv() {
                        Ok(result) => {
                            if let Err(err) = result {
                                if self.config.wal_first_commit {
                                    self.durable_version
                                        .store(self.wal_durable_version(), Ordering::Release);
                                }
                                return Err(err);
                            }
                        }
                        Err(_) => {
                            if self.config.wal_first_commit {
                                self.durable_version
                                    .store(self.wal_durable_version(), Ordering::Release);
                            }
                            return self.check_async_error();
                        }
                    }
                }
            }
        }
        if let Some(ref tx) = self.published_rewrite_tx {
            let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<()>>(0);
            let job = PublishedRewriteJob {
                barrier_only: true,
                target_version: 0,
                state_root: EMPTY_ROOT_HASH,
                segments: None,
                done: Some(done_tx),
            };
            if tx.send(job).is_ok() {
                match done_rx.recv() {
                    Ok(result) => {
                        if let Err(err) = result {
                            if self.config.wal_first_commit {
                                self.durable_version
                                    .store(self.wal_durable_version(), Ordering::Release);
                            }
                            return Err(err);
                        }
                    }
                    Err(_) => {
                        if self.config.wal_first_commit {
                            self.durable_version
                                .store(self.wal_durable_version(), Ordering::Release);
                        }
                        return self.check_async_error();
                    }
                }
            }
        }
        if self.config.wal_first_commit {
            self.durable_version.store(self.wal_durable_version(), Ordering::Release);
        }
        self.check_async_error()
    }

    fn append_shadow_wal_entry(&mut self, entry: &CommitWalEntry) -> Result<()> {
        let Some(wal_store) = self.wal_store.as_ref() else {
            return Ok(());
        };
        let lock_wait_start = std::time::Instant::now();
        let mut wal_store = wal_store.lock();
        let lock_wait = lock_wait_start.elapsed();
        let write_start = std::time::Instant::now();
        wal_store.append_entry(entry)?;
        let write_elapsed = write_start.elapsed();
        self.last_wal_append_lock_wait = lock_wait;
        self.last_wal_append_write = write_elapsed;

        if self.config.wal_shadow_validate {
            let stored = wal_store.load_entry(entry.version)?;
            if stored.as_ref() != Some(entry) {
                return Err(MptDbError::Other(format!(
                    "wal shadow validation failed at version {}",
                    entry.version
                )));
            }
            drop(wal_store);
            self.validate_shadow_wal_replay(entry).map_err(|err| {
                MptDbError::Other(format!(
                    "wal shadow replay validation failed at version {}: {err}",
                    entry.version
                ))
            })?;
        }

        Ok(())
    }

    fn rollback_shadow_wal_to(&mut self, version: i64) -> Result<()> {
        let Some(wal_store) = self.wal_store.as_ref() else {
            return Ok(());
        };
        wal_store.lock().truncate_after(version)
    }

    fn prune_shadow_wal_before(&mut self, version: i64) -> Result<()> {
        let Some(wal_store) = self.wal_store.as_ref() else {
            return Ok(());
        };
        wal_store.lock().prune_before(version)
    }

    fn wal_durable_version(&self) -> i64 {
        self.wal_store
            .as_ref()
            .map(|store| {
                let wal = store.lock();
                if wal.is_empty() {
                    self.version
                } else {
                    wal.durable_version()
                }
            })
            .unwrap_or(self.version)
    }

    fn wal_recovery_base_version(&self, committed_version: i64) -> i64 {
        self.wal_store
            .as_ref()
            .map(|store| {
                let wal = store.lock();
                if wal.is_empty() {
                    committed_version
                } else {
                    wal.durable_version().min(committed_version)
                }
            })
            .unwrap_or(committed_version)
    }

    fn wal_prune_floor_version(&self) -> Result<i64> {
        let mut floor = self.manifest.earliest_version;
        if let Some(snapshot_floor) = self.published_baseline.earliest_snapshot_version()? {
            floor = floor.min(snapshot_floor);
        }
        Ok(floor)
    }

    /// Check if this is a fresh DB (manifest = {0->EMPTY_ROOT_HASH}, no other versions, empty
    /// store).
    fn is_fresh_db(&self) -> Result<bool> {
        if self.manifest.latest_version != 0 {
            return Ok(false);
        }
        if self.manifest.versions.len() != 1 {
            return Ok(false);
        }
        if self.manifest.get_root(0) != Some(EMPTY_ROOT_HASH) {
            return Ok(false);
        }
        self.persisted.is_empty()
    }
}

#[cfg(test)]
impl MptCommitStore {
    pub(crate) fn set_fail_point(&mut self, fail: Option<CommitFailPoint>) {
        self.fail_point = fail;
    }

    pub(crate) fn set_async_fail_mode(&self, mode: u8) {
        self.async_fail_mode.store(mode, Ordering::Relaxed);
    }

    pub(crate) fn set_parallelism_thresholds(&mut self, thresholds: ParallelismThresholds) {
        self.parallelism = thresholds;
    }

    pub(crate) fn loaded_from_checkpoint(&self) -> bool {
        self.loaded_from_checkpoint
    }

    pub(crate) fn published_version(&self) -> Option<i64> {
        match self.published_version.load(Ordering::Acquire) {
            0 => None,
            version => Some(version),
        }
    }

    pub(crate) fn has_published_store(&self) -> bool {
        self.published_store.is_some()
    }

    pub(crate) fn account_trie_arena_len(&self) -> usize {
        self.account_trie.committed().arena_len()
    }
}

impl MptCommitter for MptCommitStore {
    fn apply_bundle_state(&mut self, bundle: &BundleState) -> Result<()> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_bulk_loading()?;

        if self.applied_this_block {
            return Err(MptDbError::Other(
                "apply_bundle_state already called for this block".to_string(),
            ));
        }

        let start = std::time::Instant::now();
        let apply_result = self.apply_bundle_state_inner(bundle);
        if apply_result.is_err() {
            self.poisoned = true;
            return apply_result;
        }

        self.last_apply_duration = start.elapsed();
        self.applied_this_block = true;
        Ok(())
    }

    fn commit(&mut self) -> Result<(i64, B256)> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_bulk_loading()?;

        if !self.applied_this_block {
            return Err(MptDbError::Other("must call apply_bundle_state before commit".to_string()));
        }

        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn version(&self) -> i64 {
        self.version
    }

    fn load_version(&mut self) -> Result<()> {
        self.load_version_target(0)
    }

    fn rollback(&mut self, target_version: i64) -> Result<()> {
        self.check_writable()?;
        // Wait for any in-flight persist jobs before modifying manifest
        self.flush_persist()?;

        let manifest = VersionManifest::load(&self.manifest_path)?;
        if target_version < manifest.earliest_version || target_version > manifest.latest_version {
            return Err(MptDbError::Other(format!(
                "rollback target {} out of range [{}, {}]",
                target_version, manifest.earliest_version, manifest.latest_version
            )));
        }

        let (account_trie, loaded_from_checkpoint, _durable_version) =
            self.recover_target_state(&manifest, target_version)?;

        let mut manifest_copy = manifest.clone();
        manifest_copy.truncate_after(target_version);
        manifest_copy.save(&self.manifest_path)?;
        self.rollback_shadow_wal_to(target_version)?;
        let target_root = manifest_copy.get_root(target_version).unwrap_or(EMPTY_ROOT_HASH);
        self.published_baseline.activate_published_version(target_version, target_root)?;
        self.restore_version_state(
            manifest_copy,
            target_version,
            account_trie,
            loaded_from_checkpoint,
        )?;
        if self.published_baseline.compact_for_manifest(&self.manifest)? {
            self.reload_published_view()?;
        }
        self.durable_version.store(target_version, Ordering::Release);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.shutdown(false)
    }

    fn prune_before(&mut self, version: i64) -> Result<()> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;
        // Wait for any in-flight persist jobs before modifying manifest
        self.flush_persist()?;

        if version < self.manifest.earliest_version || version > self.manifest.latest_version {
            return Err(MptDbError::Other(format!(
                "prune_before: version {} out of range [{}, {}]",
                version, self.manifest.earliest_version, self.manifest.latest_version
            )));
        }

        // Remove [earliest, version) from manifest, keep version itself
        let mut new_manifest = self.manifest.clone();
        let to_remove: Vec<i64> = new_manifest
            .versions
            .keys()
            .copied()
            .filter(|v| *v >= new_manifest.earliest_version && *v < version)
            .collect();
        for v in to_remove {
            new_manifest.versions.remove(&v);
        }
        new_manifest.earliest_version = version;
        new_manifest.save(&self.manifest_path)?;
        self.manifest = new_manifest;
        self.maybe_compact_segment_store()?;
        let wal_floor = self.wal_prune_floor_version()?;
        self.prune_shadow_wal_before(wal_floor)?;

        Ok(())
    }

    fn gc(&mut self) -> Result<MptGcStats> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;
        // Wait for any in-flight persist jobs before GC scans reachable nodes
        self.flush_persist()?;

        let live_roots: Vec<B256> = self.manifest.versions.values().copied().collect();
        let live = gc::collect_reachable_hashes(&self.persisted, live_roots)?;
        gc::gc_unreachable_nodes(&self.persisted, &live)
    }

    fn account_proof(
        &self,
        version: i64,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof> {
        // Wait for any in-flight persist jobs to ensure latest nodes are on disk
        self.flush_persist()?;
        let root = self.manifest.get_root(version).ok_or_else(|| {
            MptDbError::Other(format!("account_proof: version {version} not in manifest"))
        })?;
        proof::build_account_proof_from_root(&self.persisted, root, address, slots)
    }

    fn exporter(&self, version: i64) -> Result<Box<dyn MptSnapshotExporter>> {
        // Wait for any in-flight persist jobs to ensure all nodes are on disk
        self.flush_persist()?;
        let root = self.manifest.get_root(version).ok_or_else(|| {
            MptDbError::Other(format!("exporter: version {version} not in manifest"))
        })?;
        let exp = SnapshotExporter::new(self.persisted.clone(), root, version)?;
        Ok(Box::new(exp))
    }

    fn frontier(&self) -> CommitFrontier {
        let logical = self.version;
        let durable = self.durable_version.load(Ordering::Acquire);
        let committed_root = self.manifest.get_root(logical).unwrap_or(EMPTY_ROOT_HASH);
        let durable_root = self.manifest.get_root(durable).unwrap_or(EMPTY_ROOT_HASH);
        CommitFrontier {
            logical_version: logical,
            durable_version: durable,
            committed_root,
            durable_root,
        }
    }

    fn importer(
        &mut self,
        version: i64,
        expected_root: B256,
    ) -> Result<Box<dyn MptSnapshotImporter + '_>> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;

        if !self.is_fresh_db()? {
            return Err(MptDbError::Other("importer: DB is not fresh, cannot import".to_string()));
        }

        let imp = SnapshotImporter::new(
            version,
            expected_root,
            self.persisted.clone(),
            self.manifest_path.clone(),
        )?;

        // Wrap in a struct that holds &mut self to bind lifetime
        Ok(Box::new(BoundImporter { inner: imp, store: self }))
    }

    fn set_initial_version(&mut self, initial_version: i64) -> Result<()> {
        if initial_version < 1 {
            return Err(MptDbError::Other(format!(
                "initial_version must be >= 1, got {initial_version}"
            )));
        }
        if self.version != 0 {
            return Err(MptDbError::Other(format!(
                "set_initial_version requires version == 0, current version is {}",
                self.version
            )));
        }
        if !self.is_fresh_db()? {
            return Err(MptDbError::Other("set_initial_version requires a fresh DB".to_string()));
        }
        self.initial_version = initial_version;
        Ok(())
    }
}

/// In-memory trie node statistics, mirroring sei-db's
/// `TotalMemNodeSize` / `TotalNumOfMemNode`.
#[derive(Debug, Clone, Default)]
pub struct MemoryStats {
    /// Total number of arena nodes in the account trie.
    pub account_trie_nodes: usize,
    /// Number of cached storage trie handles.
    pub storage_trie_cached: usize,
    /// Total number of arena nodes across all cached storage tries (base copies).
    pub storage_trie_total_nodes: usize,
}

impl MptCommitStore {
    /// Return a snapshot of in-memory trie node statistics.
    ///
    /// This mirrors sei-db's `TotalMemNodeSize` / `TotalNumOfMemNode` globals.
    /// The numbers here cover resident arena nodes only — lazy/persisted
    /// subtrees are not counted until they are materialized.
    pub fn memory_stats(&self) -> MemoryStats {
        let account_trie_nodes = self.account_trie.committed().arena_len();
        let mut storage_trie_total_nodes = 0usize;
        for handle in self.storage_trie_handles.values() {
            storage_trie_total_nodes += handle.base.arena_len();
        }
        MemoryStats {
            account_trie_nodes,
            storage_trie_cached: self.storage_trie_handles.len(),
            storage_trie_total_nodes,
        }
    }

    pub fn last_commit_profile(&self) -> &CommitProfile {
        &self.last_commit_profile
    }

    pub fn commit_with_profile(&mut self) -> Result<((i64, B256), CommitProfile)> {
        let result = self.commit()?;
        Ok((result, self.last_commit_profile.clone()))
    }

    pub fn begin_bulk_load(&mut self, opts: BulkLoadOptions) -> Result<()> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;
        self.flush_persist()?;

        if self.replay_materializer {
            return Err(MptDbError::Other(
                "bulk load is not available on replay materializers".to_string(),
            ));
        }
        if self.bulk_load.is_some() {
            return Err(MptDbError::Other("bulk load already active".to_string()));
        }
        if !self.is_fresh_db()? {
            return Err(MptDbError::Other(
                "bulk load requires a fresh DB and must run before normal commits".to_string(),
            ));
        }

        self.reset_derived_state_for_new_base()?;
        // Create streaming segment writer — matches sei-db's snapshotWriter
        // that streams directly to snapshot files during import.
        self.bulk_segment_writer =
            Some(BulkSegmentWriter::new(&Self::fast_storage_root(&self.dir))?);
        self.bulk_load = Some(BulkLoadState {
            retain_only_latest: opts.retain_only_latest,
            chunks_committed: 0,
        });
        Ok(())
    }

    pub fn bulk_ingest_bundle_chunk(&mut self, bundle: &BundleState) -> Result<(i64, B256)> {
        self.check_writable()?;
        self.check_not_poisoned()?;

        if self.bulk_load.is_none() {
            return Err(MptDbError::Other(
                "bulk load is not active, call begin_bulk_load() first".to_string(),
            ));
        }
        if self.applied_this_block {
            return Err(MptDbError::Other(
                "cannot bulk-ingest while a block is being applied".to_string(),
            ));
        }

        let start = std::time::Instant::now();
        let dirty_accounts = state::collect_prepop_accounts(bundle)?;
        let collect_elapsed = start.elapsed();
        if let Err(err) = self.apply_dirty_accounts_inner(dirty_accounts) {
            self.poisoned = true;
            return Err(err);
        }
        self.last_apply_collect_dirty_accounts = collect_elapsed;
        self.last_apply_duration = start.elapsed();
        self.applied_this_block = true;

        let result = self.bulk_commit_inner();
        if result.is_err() {
            self.poisoned = true;
            return result;
        }

        if let Some(state) = self.bulk_load.as_mut() {
            state.chunks_committed += 1;
        }
        result
    }

    pub fn finish_bulk_load(&mut self) -> Result<BulkLoadSummary> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;

        let Some(state) = self.bulk_load.take() else {
            return Err(MptDbError::Other("bulk load is not active".to_string()));
        };

        // Finalize the streaming segment writer BEFORE prune — prune's
        // compact_for_manifest may rewrite pages.data, but the delta file
        // must exist first so the compactor knows which pages to keep.
        if let Some(writer) = self.bulk_segment_writer.take() {
            let root = self.manifest.get_root(self.version).unwrap_or(EMPTY_ROOT_HASH);
            if let Some(meta) = writer.finalize(self.version, root)? {
                self.published_meta = Some(meta.clone());
                self.published_store = self.published_baseline.open_published_store(&meta)?;
                self.published_version.store(self.version, Ordering::Release);
            }
        }

        if state.retain_only_latest && self.version > self.manifest.earliest_version {
            self.prune_before(self.version)?;
            // Re-open published store after compaction (pages may have moved).
            if let Some(ref meta) = self.published_meta {
                self.published_store = self.published_baseline.open_published_store(meta)?;
            }
        }

        Ok(BulkLoadSummary {
            chunks_committed: state.chunks_committed,
            final_version: self.version,
            final_root: self.manifest.get_root(self.version).unwrap_or(EMPTY_ROOT_HASH),
        })
    }

    fn apply_dirty_accounts_inner(&mut self, dirty_accounts: Vec<DirtyAccount>) -> Result<()> {
        let published_refreshes = 0u64;
        let l3_into_tree = Duration::ZERO;
        let collect_elapsed = Duration::ZERO;
        let load_start = std::time::Instant::now();
        let working_version = self.current_working_version();
        let acct_checkout_start = std::time::Instant::now();
        self.account_trie.checkout_for_write(working_version);
        // No bulk preload_paths: matching sei-db's model where reads go
        // directly through mmap'd segment (PersistedNode) and writes do
        // on-demand COW (MemNode).  Only the first block after a cold start
        // (when the trie root is Lazy) triggers batch materialization;
        // subsequent blocks reuse the arena and resolve new paths lazily.
        if let Some(working) = self.account_trie.working.as_mut() {
            if working.is_lazy_root() {
                // Cold start: batch-load all paths from segment/persisted
                // (equivalent to sei-db's initial LoadMultiTree).
                let touched_account_keys: Vec<Nibbles> =
                    dirty_accounts.iter().map(|dirty| dirty.account_key.clone()).collect();
                working.preload_paths(&self.persisted, &touched_account_keys)?;
            }
        }
        let acct_checkout_elapsed = acct_checkout_start.elapsed();
        let ensure_start = std::time::Instant::now();
        let load_stats = self.ensure_working_storage_tries(&dirty_accounts)?;
        let ensure_elapsed = ensure_start.elapsed();

        let get_or_load_elapsed = load_start.elapsed();
        let mut slot_updates_elapsed = Duration::ZERO;

        let slot_updates_start = std::time::Instant::now();
        tree_algo::reset_stats();
        let mut dirty_storage_accounts = HashMap::new();
        for dirty in &dirty_accounts {
            if dirty.storage_wiped {
                // Evict from cache: selfdestruct invalidates any cached trie
                self.evict_cached_storage_trie(&dirty.hashed_address);
                // Wiped: start from empty storage trie, apply new changes on top.
                self.activate_empty_trie(dirty.hashed_address);
            }
            if dirty.storage_wiped || !dirty.storage_changes.is_empty() {
                dirty_storage_accounts.insert(dirty.hashed_address, dirty);
            }
        }

        let dirty_addresses: Vec<B256> = dirty_storage_accounts.keys().copied().collect();
        let dirty_handles = self.take_working_handles(dirty_addresses.clone());
        // When wal_first, merge slot updates + root hash into a single rayon
        // pass so trie data stays cache-hot.  This eliminates the second cache
        // load that the separate storage_roots phase would incur.
        let merge_hash = self.config.wal_first_commit;
        let persisted_ref = &self.persisted;
        // Use parallel iteration only when there are enough handles to amortize
        // rayon dispatch overhead + allocator contention.  For fewer handles
        // (e.g., 300 contracts in B4.7), sequential is 41x faster due to
        // zero cache thrashing and zero malloc lock contention.
        let apply_one = |(hashed_address, mut handle): (B256, StorageTrieHandle)| -> Result<(B256, StorageTrieHandle)> {
                let trie = handle.take_working_or_base_for_version(working_version);
                let trie = match dirty_storage_accounts.get(&hashed_address) {
                    Some(dirty) => {
                        Self::apply_storage_changes_to_working(trie, persisted_ref, dirty)?
                    }
                    None => trie,
                };
                if merge_hash {
                    // Hash immediately while data is cache-hot.
                    let (root, mut cow) = trie.root_hash_only(persisted_ref).map_err(|err| {
                        MptDbError::Other(format!(
                            "merged storage trie root hash for {hashed_address}: {err}"
                        ))
                    })?;
                    cow.clear_dirty();
                    handle.pre_computed = Some((root, cow));
                } else {
                    handle.restore_working(working_version, trie);
                }
            Ok((hashed_address, handle))
        };
        // Always use rayon parallel iteration regardless of handle count.
        // Parallel utilizes all cores' memory bandwidth simultaneously — even
        // 300 tasks on 12 cores is faster than serial because scattered 60KB
        // tries cause cache misses that serial would serialize into a bottleneck.
        // See: .claude/problems/b4_7_performance_gap.md (Fix 2: rejected)
        let updated_handles: Vec<(B256, StorageTrieHandle)> =
            dirty_handles.into_par_iter().map(apply_one).collect::<Result<_>>()?;
        self.reinsert_handles(updated_handles);

        for hashed_address in &dirty_addresses {
            if !self.contains_working_trie(hashed_address) {
                return Err(MptDbError::Other(format!(
                    "missing working storage trie for {}",
                    hashed_address
                )));
            }
        }
        slot_updates_elapsed += slot_updates_start.elapsed();
        let slot_stats = tree_algo::snapshot_stats();

        self.dirty_accounts = dirty_accounts;
        self.last_apply_collect_dirty_accounts = collect_elapsed;
        self.last_apply_get_or_load_storage_tries = get_or_load_elapsed;
        self.last_apply_account_trie_checkout = acct_checkout_elapsed;
        self.last_apply_ensure_storage = ensure_elapsed;
        self.last_apply_storage_root_lookup = load_stats.storage_root_lookup;
        self.last_apply_storage_slot_updates = slot_updates_elapsed;
        self.last_apply_l3_latest_load = load_stats.l3_latest_load;
        self.last_apply_l3_published_load = load_stats.l3_published_load;
        self.last_apply_l3_into_tree = l3_into_tree;
        self.last_apply_published_refreshes = published_refreshes;
        self.last_apply_l2_hits = load_stats.l2_hits;
        self.last_apply_l3_latest_hits = load_stats.l3_latest_hits;
        self.last_apply_l3_published_hits = load_stats.l3_published_hits;
        self.last_apply_l3_published_post_flush_hits = load_stats.l3_published_post_flush_hits;
        self.last_apply_node_fallback_loads = load_stats.node_fallback_loads;
        self.last_apply_slot_inserts = slot_stats.slot_inserts;
        self.last_apply_slot_deletes = slot_stats.slot_deletes;
        self.last_apply_leaf_splits = slot_stats.leaf_splits;
        self.last_apply_extension_splits = slot_stats.extension_splits;
        self.last_apply_branch_collapse_to_empty = slot_stats.branch_collapse_to_empty;
        self.last_apply_branch_collapse_to_leaf = slot_stats.branch_collapse_to_leaf;
        self.last_apply_branch_collapse_to_extension = slot_stats.branch_collapse_to_extension;
        self.last_apply_extension_leaf_merges = slot_stats.extension_leaf_merges;
        self.last_apply_extension_extension_merges = slot_stats.extension_extension_merges;
        Ok(())
    }

    fn apply_bundle_state_inner(&mut self, bundle: &BundleState) -> Result<()> {
        let collect_start = std::time::Instant::now();
        let dirty_accounts = state::collect_dirty_accounts(bundle)?;
        let collect_elapsed = collect_start.elapsed();
        self.apply_dirty_accounts_inner(dirty_accounts)?;
        self.last_apply_collect_dirty_accounts = collect_elapsed;
        Ok(())
    }

    fn apply_wal_entry_inner(&mut self, entry: &CommitWalEntry) -> Result<()> {
        let start = std::time::Instant::now();
        // Apply schema upgrades before changeset (mirrors sei-db replay ordering).
        if !entry.upgrades.is_empty() {
            self.apply_wal_upgrades(&entry.upgrades)?;
        }
        let dirty_accounts = entry.to_dirty_accounts();
        self.apply_dirty_accounts_inner(dirty_accounts)?;
        self.last_apply_duration = start.elapsed();
        self.applied_this_block = true;
        Ok(())
    }

    fn apply_wal_upgrades(&mut self, upgrades: &[super::wal::CommitWalUpgrade]) -> Result<()> {
        for upgrade in upgrades {
            tracing::info!(
                key = %upgrade.key,
                value = %upgrade.value,
                "applying WAL upgrade"
            );
            // No upgrade types are defined yet.  Future upgrades (e.g.,
            // format_version changes, key encoding migrations) will match
            // on upgrade.key here.
        }
        Ok(())
    }

    fn bulk_commit_inner(&mut self) -> Result<(i64, B256)> {
        self.commit_inner_with_mode(CommitExecutionMode {
            wal_first: false,
            allow_async: false,
            save_manifest: true,
            // Build segments from in-memory tries — the BulkSegmentWriter
            // streams them directly to pages.data (matching sei-db's
            // snapshotWriter).  publish_generation is skipped during
            // bulk_load; one delta file is written at finish.
            publish_baseline: true,
        })
    }

    /// Core commit logic: compute roots, persist nodes, update manifest.
    ///
    /// ## Persistence Protocol
    ///
    /// Synchronous commits durably persist dirty trie nodes before atomically
    /// saving the manifest. Asynchronous commits publish the new logical
    /// version immediately and rely on the background worker to make that
    /// version durable; callers that need durability must call `flush_persist`.
    fn commit_inner(&mut self) -> Result<(i64, B256)> {
        self.commit_inner_with_mode(self.default_commit_mode())
    }

    fn commit_inner_with_mode(&mut self, mode: CommitExecutionMode) -> Result<(i64, B256)> {
        // Phase 1: compute storage roots for all dirty accounts.
        //
        // Collect DELETE/REUSE roots serially (cheap lookups), then compute
        // RECOMPUTE roots from dirty handle working copies.
        let profile_start = std::time::Instant::now();
        let mut storage_roots: HashMap<B256, B256> =
            HashMap::with_capacity(self.dirty_accounts.len());
        let storage_start = std::time::Instant::now();
        let mut storage_roots_fast_path_elapsed = Duration::ZERO;
        let mut storage_roots_fast_path_extract_elapsed = Duration::ZERO;
        let mut storage_roots_fast_path_release_elapsed = Duration::ZERO;
        let mut storage_roots_fast_path_drop_elapsed = Duration::ZERO;
        let mut storage_roots_fallback_elapsed = Duration::ZERO;
        let storage_roots_merge_elapsed;
        let mut storage_roots_precomputed_handles = 0u64;
        let mut storage_roots_rehashed_handles = 0u64;

        // Pre-fill DELETE and REUSE cases (no trie computation needed)
        let storage_prefill_start = std::time::Instant::now();
        for dirty in &self.dirty_accounts {
            if dirty.info.is_none() && dirty.storage_wiped {
                // DELETE case
                storage_roots.insert(dirty.hashed_address, EMPTY_ROOT_HASH);
            } else if !self.contains_working_trie(&dirty.hashed_address) {
                // REUSE case: get from existing account leaf
                let root = self.get_existing_storage_root(&dirty.hashed_address);
                storage_roots.insert(dirty.hashed_address, root);
            }
            // RECOMPUTE case handled below via parallel/serial path
        }
        let storage_roots_prefill_elapsed = storage_prefill_start.elapsed();

        let working_version = self.current_working_version();
        let take_handles_start = std::time::Instant::now();
        let dirty_working_addresses: Vec<B256> = self
            .dirty_accounts
            .iter()
            .filter(|dirty| self.contains_working_trie(&dirty.hashed_address))
            .map(|dirty| dirty.hashed_address)
            .collect();
        let dirty_handles = self.take_working_handles(dirty_working_addresses.clone());
        let storage_roots_take_handles_elapsed = take_handles_start.elapsed();
        let storage_roots_working_handles = dirty_handles.len() as u64;
        let should_parallel =
            self.parallelism.should_parallelize_storage_tries(dirty_handles.len());
        let mut storage_root_hash_elapsed = Duration::ZERO;
        let mut storage_segment_build_elapsed = Duration::ZERO;
        let persisted_for_hash = Arc::clone(&self.persisted);

        // In wal_first mode, use hash-only computation (no blob collection)
        // matching sei-db's model: commit is pure in-memory, serialization
        // is deferred to background segment publish.
        let hash_only = mode.wal_first;

        let compute_storage_artifact = |addr: B256,
                                        mut handle: StorageTrieHandle,
                                        persisted: &PersistedTrieStore,
                                        hash_only: bool|
         -> Result<StorageTrieCommitArtifacts> {
            let trie = handle.take_working_or_base_for_version(working_version);
            let hash_start = std::time::Instant::now();
            let (root, blobs, mut cow) = if hash_only {
                let (root, cow) = trie.root_hash_only(persisted).map_err(|err| {
                    MptDbError::Other(format!("storage trie root hash for {addr}: {err}"))
                })?;
                (root, Vec::new(), cow)
            } else {
                trie.root_hash_and_dirty_blobs(persisted).map_err(|err| {
                    MptDbError::Other(format!("storage trie root hash for {addr}: {err}"))
                })?
            };
            let hash_elapsed = hash_start.elapsed();
            cow.clear_dirty();
            Ok(StorageTrieCommitArtifacts {
                hashed_address: addr,
                storage_root: root,
                node_blobs: blobs,
                publish_view: StorageTriePublishView::DeferredRoot(root),
                hash_elapsed,
                segment_elapsed: Duration::ZERO,
                trie: cow,
            })
        };

        // Check if any handle carries a pre-computed result from the merged
        // apply+hash phase. If so, collect directly — no re-hash needed.
        let any_pre_computed = dirty_handles.iter().any(|(_, h)| h.pre_computed.is_some());

        let storage_artifacts: Vec<StorageTrieCommitArtifacts> = if any_pre_computed {
            // Fast path: use pre-computed roots from the merged apply+hash
            // rayon pass.  Data was hashed while still cache-hot.
            let fast_path_start = std::time::Instant::now();
            let mut artifacts = Vec::with_capacity(dirty_handles.len());
            for (addr, mut handle) in dirty_handles {
                if let Some((root, cow)) = handle.pre_computed.take() {
                    storage_roots_precomputed_handles += 1;
                    let extract_start = std::time::Instant::now();
                    artifacts.push(StorageTrieCommitArtifacts {
                        hashed_address: addr,
                        storage_root: root,
                        node_blobs: Vec::new(),
                        publish_view: StorageTriePublishView::DeferredRoot(root),
                        hash_elapsed: Duration::ZERO,
                        segment_elapsed: Duration::ZERO,
                        trie: cow,
                    });
                    storage_roots_fast_path_extract_elapsed += extract_start.elapsed();
                    // Drop the old base immediately so Arc::make_mut in the
                    // parallel snapshot pass below sees refcount=1 and can
                    // freeze in-place rather than copying all trie nodes.
                    let release_start = std::time::Instant::now();
                    drop(handle);
                    storage_roots_fast_path_release_elapsed += release_start.elapsed();
                } else {
                    storage_roots_rehashed_handles += 1;
                    artifacts.push(compute_storage_artifact(
                        addr,
                        handle,
                        &persisted_for_hash,
                        hash_only,
                    )?);
                }
            }
            storage_roots_fast_path_elapsed = fast_path_start.elapsed();

            // Parallel snapshot pass: all old bases are now dropped, so
            // Arc::make_mut fires in-place (refcount=1) for every trie.
            // After this, each trie's frozen.nodes is an independent Arc —
            // the subsequent trie.clone() in save_storage_version is O(1).
            let snap_start = std::time::Instant::now();
            if should_parallel {
                artifacts.par_iter_mut().for_each(|a| a.trie.snapshot());
            } else {
                artifacts.iter_mut().for_each(|a| a.trie.snapshot());
            }
            storage_roots_fast_path_drop_elapsed = snap_start.elapsed();

            artifacts
        } else if should_parallel {
            let fallback_start = std::time::Instant::now();
            storage_roots_rehashed_handles = storage_roots_working_handles;
            let artifacts = dirty_handles
                .into_par_iter()
                .map(|(addr, handle)| {
                    compute_storage_artifact(addr, handle, &persisted_for_hash, hash_only)
                })
                .collect::<Result<Vec<_>>>()?;
            storage_roots_fallback_elapsed = fallback_start.elapsed();
            artifacts
        } else {
            let fallback_start = std::time::Instant::now();
            storage_roots_rehashed_handles = storage_roots_working_handles;
            let artifacts = dirty_handles
                .into_iter()
                .map(|(addr, handle)| {
                    compute_storage_artifact(addr, handle, &persisted_for_hash, hash_only)
                })
                .collect::<Result<Vec<_>>>()?;
            storage_roots_fallback_elapsed = fallback_start.elapsed();
            artifacts
        };

        // Merge RECOMPUTE roots into storage_roots map
        let merge_start = std::time::Instant::now();
        for artifact in &storage_artifacts {
            storage_roots.insert(artifact.hashed_address, artifact.storage_root);
            storage_root_hash_elapsed += artifact.hash_elapsed;
            storage_segment_build_elapsed += artifact.segment_elapsed;
        }
        storage_roots_merge_elapsed = merge_start.elapsed();

        let storage_roots_elapsed = storage_start.elapsed();

        // Phase 2: precompute account writes in parallel, then apply to the
        // single shared account trie serially.
        let account_updates_start = std::time::Instant::now();
        let encode_account = |dirty: &DirtyAccount| {
            let storage_root = storage_roots[&dirty.hashed_address];
            match &dirty.info {
                None => None,
                Some(info) => {
                    let is_empty = info.is_empty() && storage_root == EMPTY_ROOT_HASH;
                    if is_empty {
                        None
                    } else {
                        let trie_account = alloy_trie::TrieAccount {
                            nonce: info.nonce,
                            balance: info.balance,
                            storage_root,
                            code_hash: info.code_hash,
                        };
                        let mut rlp_buf = Vec::new();
                        trie_account.encode(&mut rlp_buf);
                        Some(rlp_buf)
                    }
                }
            }
        };

        // Overlap WAL changeset construction with account encode + apply.
        // WAL changeset building (sorting 300K storage changes) is CPU-intensive
        // but does NOT depend on state_root.  Run it on a rayon worker while
        // the main thread does account trie updates.
        let wal_changeset_holder: Option<
            Arc<parking_lot::Mutex<Option<Vec<CommitWalAccountChange>>>>,
        > = if mode.wal_first && self.dirty_accounts.len() >= 256 {
            let holder = Arc::new(parking_lot::Mutex::new(None));
            let holder_clone = Arc::clone(&holder);
            // Collect the data needed for WAL changeset building.
            // This avoids holding a borrow on self across the rayon spawn boundary.
            let wal_dirty: Vec<_> = self
                .dirty_accounts
                .iter()
                .map(|d| {
                    (
                        d.address,
                        d.hashed_address,
                        d.info.clone(),
                        d.storage_wiped,
                        d.storage_known_empty,
                        d.storage_changes.clone(),
                    )
                })
                .collect();
            rayon::spawn(move || {
                let accounts: Vec<CommitWalAccountChange> = wal_dirty
                    .into_iter()
                    .map(
                        |(
                            address,
                            hashed_address,
                            info,
                            storage_wiped,
                            storage_known_empty,
                            storage_changes,
                        )| {
                            use super::wal::{CommitWalAccountInfo, CommitWalStorageChange};
                            let mut sc: Vec<CommitWalStorageChange> = storage_changes
                                .iter()
                                .map(|c| CommitWalStorageChange {
                                    hashed_slot: c.hashed_slot,
                                    value: c.value,
                                })
                                .collect();
                            sc.sort_by(|a, b| a.hashed_slot.cmp(&b.hashed_slot));
                            CommitWalAccountChange {
                                address,
                                hashed_address,
                                info: info.as_ref().map(|i| CommitWalAccountInfo {
                                    nonce: i.nonce,
                                    balance: i.balance,
                                    code_hash: i.code_hash,
                                }),
                                storage_wiped,
                                storage_known_empty,
                                storage_changes: sc,
                            }
                        },
                    )
                    .collect();
                *holder_clone.lock() = Some(accounts);
            });
            Some(holder)
        } else {
            None
        };

        let account_writes: Vec<Option<Vec<u8>>> = if self.dirty_accounts.len() >= 1_024 {
            self.dirty_accounts.par_iter().map(encode_account).collect()
        } else {
            self.dirty_accounts.iter().map(encode_account).collect()
        };

        let working_version = self.current_working_version();
        self.account_trie.checkout_for_write(working_version);
        let mut account_trie = self.account_trie.take_working_or_base_for_version(working_version);
        // Use the fast materialized path when the account trie has no lazy
        // root (true after bulk_load and all subsequent blocks).
        let materialized = !account_trie.is_lazy_root();
        for (dirty, encoded) in self.dirty_accounts.iter().zip(account_writes.into_iter()) {
            let key = &dirty.account_key;
            if materialized {
                account_trie.apply_change_materialized(key, encoded);
            } else if let Some(rlp_buf) = encoded {
                account_trie.apply_change(&self.persisted, key, Some(rlp_buf)).map_err(|err| {
                    MptDbError::Other(format!(
                        "account trie apply_change for {}: {err}",
                        dirty.hashed_address
                    ))
                })?;
            } else {
                account_trie.apply_change(&self.persisted, key, None).map_err(|err| {
                    MptDbError::Other(format!(
                        "account trie apply_change delete for {}: {err}",
                        dirty.hashed_address
                    ))
                })?;
            }
        }
        let account_updates_elapsed = account_updates_start.elapsed();

        // Phase 2b: compute state root.
        // wal_first: hash-only (no blob collection) — matching sei-db.
        // sync: hash + collect blobs for RocksDB persist.
        let account_root_start = std::time::Instant::now();
        let (state_root, account_blobs, account_cow) = if hash_only {
            let (root, cow) = account_trie
                .root_hash_only_parallel(&self.persisted)
                .map_err(|err| MptDbError::Other(format!("account trie root hash: {err}")))?;
            (root, Vec::new(), cow)
        } else {
            account_trie
                .root_hash_and_dirty_blobs_parallel(&self.persisted)
                .map_err(|err| MptDbError::Other(format!("account trie root hash: {err}")))?
        };
        let account_root_elapsed = account_root_start.elapsed();

        // Separate node blobs from tries so we can cache tries after persist
        let mut storage_cache_candidates: Vec<(B256, StorageTrieCow)> =
            Vec::with_capacity(storage_artifacts.len());
        let mut deferred_published_roots: Vec<(B256, B256)> =
            Vec::with_capacity(storage_artifacts.len());
        let extra_blob_capacity: usize =
            storage_artifacts.iter().map(|artifact| artifact.node_blobs.len()).sum();
        let mut all_blobs = Vec::with_capacity(account_blobs.len() + extra_blob_capacity);
        all_blobs.extend(account_blobs);
        for artifact in storage_artifacts {
            all_blobs.extend(artifact.node_blobs);
            let StorageTriePublishView::DeferredRoot(root) = artifact.publish_view;
            deferred_published_roots.push((artifact.hashed_address, root));
            storage_cache_candidates.push((artifact.hashed_address, artifact.trie));
        }

        // Check test failpoint: BeforePersist
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::BeforePersist) {
            return Err(MptDbError::Other("failpoint: BeforePersist".to_string()));
        }

        // Check test failpoint: AfterPersistBeforeManifest
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::AfterPersistBeforeManifest) {
            return Err(MptDbError::Other("failpoint: AfterPersistBeforeManifest".to_string()));
        }

        let cache_publish_start = std::time::Instant::now();
        let prepared = self.prepare_storage_version(
            state_root,
            &storage_roots,
            &storage_cache_candidates,
            all_blobs.len(),
            mode.allow_async,
        )?;
        let wal_entry = if let Some(ref holder) = wal_changeset_holder {
            // Collect pre-built changeset from rayon task (should be done by now).
            let accounts = holder.lock().take().unwrap_or_default();
            Some(CommitWalEntry::from_prebuilt_changes(
                prepared.new_version,
                state_root,
                state_root,
                accounts,
                &self.dirty_accounts,
            ))
        } else if mode.wal_first {
            // Small changeset (< 256 accounts): build WAL entry synchronously.
            Some(CommitWalEntry::from_dirty_accounts(
                prepared.new_version,
                state_root,
                state_root,
                &self.dirty_accounts,
            ))
        } else {
            None
        };
        let cache_publish_elapsed = cache_publish_start.elapsed();
        let mut wal_append_elapsed = Duration::ZERO;
        self.last_wal_append_lock_wait = Duration::ZERO;
        self.last_wal_append_write = Duration::ZERO;

        // Check test failpoint: ManifestSave
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::ManifestSave) {
            return Err(MptDbError::Other("failpoint: ManifestSave".to_string()));
        }

        if let Some(ref wal_entry) = wal_entry {
            let wal_append_start = std::time::Instant::now();
            if let Err(err) = self.append_shadow_wal_entry(wal_entry) {
                let _ = self.rollback_shadow_wal_to(self.version);
                return Err(err);
            }
            wal_append_elapsed = wal_append_start.elapsed();
        }

        let mut saved = match self.save_storage_version(
            prepared,
            all_blobs,
            &storage_roots,
            &mut storage_cache_candidates,
            deferred_published_roots,
            mode,
            &mut storage_segment_build_elapsed,
        ) {
            Ok(saved) => saved,
            Err(err) => {
                if wal_entry.is_some() {
                    self.rollback_shadow_wal_to(self.version)?;
                }
                return Err(err);
            }
        };
        saved.wal_append_elapsed = wal_append_elapsed;

        // Commit succeeded: update internal state
        self.manifest = saved.manifest;
        self.version = saved.new_version;
        let checkpoint_account_trie_nodes = Some(account_cow.arena_len());
        let committed_account_trie = if state_root == EMPTY_ROOT_HASH {
            StorageTrieCow::empty()
        } else {
            // Always keep the account trie in memory across blocks,
            // matching sei-db's model where trees are always resident.
            // After bulk_load the trie is fully materialized; subsequent
            // commits only COW the modified paths.  Since apply_change
            // never introduces new lazy children on a materialized base,
            // the trie stays fully in-arena with no pending_lazy_children.
            account_cow
        };
        let acct_set_base_start = std::time::Instant::now();
        self.account_trie.set_committed_base(saved.new_version, committed_account_trie);
        let acct_set_base_elapsed = acct_set_base_start.elapsed();
        self.checkpoint_account_trie_nodes = checkpoint_account_trie_nodes;
        self.schedule_checkpoint_save()?;
        self.dirty_accounts.clear();
        self.applied_this_block = false;

        if !saved.use_async && mode.publish_baseline && self.bulk_segment_writer.is_none() {
            self.maybe_compact_segment_store()?;
        }

        let cache_storage_prep_start = std::time::Instant::now();
        // Fold committed working tries back into their long-lived handle bases.
        // Also write L3 fast store images (best-effort, non-fatal).
        let published_segment_map: HashMap<B256, &StorageTrieSegment> =
            saved.published_puts.iter().map(|(addr, segment)| (*addr, segment)).collect();
        let cached_storage_tries = if storage_cache_candidates.len() >= 256 {
            storage_cache_candidates
                .into_par_iter()
                .filter_map(|(addr, trie)| {
                    if saved.deleted_accounts.contains(&addr) {
                        return None;
                    }
                    let storage_root = storage_roots.get(&addr).copied().unwrap_or(EMPTY_ROOT_HASH);
                    let published_segment = published_segment_map.get(&addr).copied();
                    Some(
                        self.prepare_cached_storage_trie(
                            trie,
                            storage_root,
                            published_segment,
                            saved.use_async,
                        )
                        .map(|cached_trie| (addr, cached_trie)),
                    )
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            storage_cache_candidates
                .into_iter()
                .filter_map(|(addr, trie)| {
                    if saved.deleted_accounts.contains(&addr) {
                        return None;
                    }
                    let storage_root = storage_roots.get(&addr).copied().unwrap_or(EMPTY_ROOT_HASH);
                    let published_segment = published_segment_map.get(&addr).copied();
                    Some(
                        self.prepare_cached_storage_trie(
                            trie,
                            storage_root,
                            published_segment,
                            saved.use_async,
                        )
                        .map(|cached_trie| (addr, cached_trie)),
                    )
                })
                .collect::<Result<Vec<_>>>()?
        };
        for (addr, cached_trie) in cached_storage_tries {
            self.cache_storage_trie(addr, cached_trie);
        }
        let cache_storage_prep_elapsed = cache_storage_prep_start.elapsed();

        self.last_commit_profile = CommitProfile {
            apply_bundle_state: self.last_apply_duration,
            apply_collect_dirty_accounts: self.last_apply_collect_dirty_accounts,
            apply_get_or_load_storage_tries: self.last_apply_get_or_load_storage_tries,
            apply_storage_slot_updates: self.last_apply_storage_slot_updates,
            apply_l3_latest_load: self.last_apply_l3_latest_load,
            apply_l3_published_load: self.last_apply_l3_published_load,
            apply_l3_into_tree: self.last_apply_l3_into_tree,
            apply_published_refreshes: self.last_apply_published_refreshes,
            apply_l2_hits: self.last_apply_l2_hits,
            apply_l3_latest_hits: self.last_apply_l3_latest_hits,
            apply_l3_published_hits: self.last_apply_l3_published_hits,
            apply_l3_published_post_flush_hits: self.last_apply_l3_published_post_flush_hits,
            apply_node_fallback_loads: self.last_apply_node_fallback_loads,
            apply_slot_inserts: self.last_apply_slot_inserts,
            apply_slot_deletes: self.last_apply_slot_deletes,
            apply_leaf_splits: self.last_apply_leaf_splits,
            apply_extension_splits: self.last_apply_extension_splits,
            apply_branch_collapse_to_empty: self.last_apply_branch_collapse_to_empty,
            apply_branch_collapse_to_leaf: self.last_apply_branch_collapse_to_leaf,
            apply_branch_collapse_to_extension: self.last_apply_branch_collapse_to_extension,
            apply_extension_leaf_merges: self.last_apply_extension_leaf_merges,
            apply_extension_extension_merges: self.last_apply_extension_extension_merges,
            storage_roots: storage_roots_elapsed,
            storage_roots_prefill: storage_roots_prefill_elapsed,
            storage_roots_take_handles: storage_roots_take_handles_elapsed,
            storage_roots_fast_path_collect: storage_roots_fast_path_elapsed,
            storage_roots_fast_path_extract: storage_roots_fast_path_extract_elapsed,
            storage_roots_fast_path_release: storage_roots_fast_path_release_elapsed,
            storage_roots_fast_path_drop: storage_roots_fast_path_drop_elapsed,
            storage_roots_fallback_collect: storage_roots_fallback_elapsed,
            storage_roots_merge: storage_roots_merge_elapsed,
            storage_roots_working_handles,
            storage_roots_precomputed_handles,
            storage_roots_rehashed_handles,
            storage_root_hashing: storage_root_hash_elapsed,
            storage_segment_build: storage_segment_build_elapsed,
            account_updates: account_updates_elapsed,
            account_root_and_blobs: account_root_elapsed,
            wal_append: saved.wal_append_elapsed,
            wal_append_lock_wait: self.last_wal_append_lock_wait,
            wal_append_write: self.last_wal_append_write,
            wal_replay: Duration::from_micros(self.last_wal_replay_micros.load(Ordering::Acquire)),
            durable_materialize: Duration::from_micros(
                self.last_durable_materialize_micros.load(Ordering::Acquire),
            ),
            published_materialize: Duration::from_micros(
                self.last_published_materialize_micros.load(Ordering::Acquire),
            ),
            durable_version_lag: self.version - self.durable_version.load(Ordering::Acquire),
            published_version_lag: self.version - self.published_version.load(Ordering::Acquire),
            persist_and_manifest: saved.persist_elapsed,
            persist_batch: saved.persist_batch_elapsed,
            manifest_save: saved.manifest_save_elapsed,
            publish_generation: saved.publish_generation_elapsed,
            open_published_store: saved.open_published_store_elapsed,
            cache_publish: cache_publish_elapsed,
            total_commit: profile_start.elapsed(),
            apply_account_trie_checkout: self.last_apply_account_trie_checkout,
            apply_ensure_storage: self.last_apply_ensure_storage,
            apply_storage_root_lookup: self.last_apply_storage_root_lookup,
            commit_account_set_base: acct_set_base_elapsed,
            commit_cache_storage_prep: cache_storage_prep_elapsed,
        };

        // In wal_first mode, periodically save the account trie checkpoint
        // so cold starts can load directly from the checkpoint file instead
        // of materializing the entire trie from RocksDB.
        if self.config.wal_first_commit &&
            self.bulk_load.is_none() &&
            saved.new_version > 0 &&
            (saved.new_version as usize) % self.config.published_snapshot_interval == 0
        {
            let _ = self.save_checkpoint();
        }

        Ok((saved.new_version, state_root))
    }
}

/// Wrapper that binds an importer's lifetime to the MptCommitStore.
/// On close, it updates the store's internal state to reflect the import.
struct BoundImporter<'a> {
    inner: SnapshotImporter,
    store: &'a mut MptCommitStore,
}

impl<'a> super::r#trait::MptSnapshotImporter for BoundImporter<'a> {
    fn add_node(&mut self, node: &super::r#trait::MptSnapshotNode) -> Result<()> {
        self.inner.add_node(node)
    }

    fn close(&mut self) -> Result<()> {
        // Drop the persist channel so the background thread finishes and
        // releases its Arc<PersistedTrieStore>. Then close the old store
        // to release the RocksDB lock before the atomic rename in inner.close().
        self.store.persist_tx.take();
        self.store.published_rewrite_tx.take();
        if let Some(handle) = self.store.persist_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.store.published_rewrite_handle.take() {
            let _ = handle.join();
        }
        // Now self.store.persisted should be the only Arc reference
        if let Some(old_store) = Arc::get_mut(&mut self.store.persisted) {
            old_store.close()?;
        }

        self.inner.close()?;

        // After successful import with atomic install, the live trie_nodes dir
        // has been replaced. Reopen the PersistedTrieStore at the new directory.
        let trie_nodes_dir = self.store.dir.join("trie_nodes");
        self.store.persisted = Arc::new(PersistedTrieStore::open_with_capacity(
            &trie_nodes_dir,
            self.store.config.persisted_node_cache_capacity,
        )?);

        self.store.reset_derived_state_for_new_base()?;
        let manifest = VersionManifest::load(&self.store.manifest_path)?;
        let version = manifest.latest_version;
        let root = manifest.get_root(version).unwrap_or(EMPTY_ROOT_HASH);
        let (account_trie, loaded_from_checkpoint) = MptCommitStore::load_account_trie_snapshot(
            &self.store.dir,
            &self.store.persisted,
            version,
            root,
        )?;
        self.store.durable_version.store(version, Ordering::Release);
        self.store.restore_version_state(
            manifest,
            version,
            account_trie,
            loaded_from_checkpoint,
        )?;
        self.store.start_persist_worker()?;

        Ok(())
    }
}

impl Drop for MptCommitStore {
    fn drop(&mut self) {
        let _ = self.shutdown(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, Address};
    use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
    use revm_database::{states::StorageSlot, BundleAccount};
    use revm_state::AccountInfo;
    use std::time::Duration;
    use tempfile::TempDir;

    fn make_bundle(
        accounts: Vec<(
            Address,
            Option<AccountInfo>,
            revm_database::AccountStatus,
            Vec<(U256, U256, U256)>,
        )>,
    ) -> BundleState {
        let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
            alloy_primitives::map::HashMap::default();
        for (address, info, status, storage) in accounts {
            let storage_map: revm_database::StorageWithOriginalValues = storage
                .into_iter()
                .map(|(key, orig, present)| (key, StorageSlot::new_changed(orig, present)))
                .collect();
            let bundle_account = BundleAccount::new(None, info, storage_map, status);
            state.insert(address, bundle_account);
        }
        BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    fn default_info(nonce: u64, balance: u64) -> AccountInfo {
        AccountInfo {
            nonce,
            balance: U256::from(balance),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        }
    }

    fn wait_for_published_generation(
        store: &MptCommitStore,
        version: i64,
    ) -> PublishedBaselineMeta {
        let root = store.manifest.get_root(version).unwrap_or(EMPTY_ROOT_HASH);
        for _ in 0..200 {
            if let Some(meta) = store.published_baseline.meta_for_version(version, root).unwrap() {
                return meta;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("published generation {version} did not appear");
    }

    #[test]
    fn bulk_load_requires_fresh_db() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        let err = store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap_err();
        assert!(err.to_string().contains("fresh DB"));
    }

    #[test]
    fn bulk_load_matches_normal_roots_and_prunes_to_latest() {
        let chunk1 = make_bundle(vec![
            (
                Address::repeat_byte(0x11),
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(10))],
            ),
            (
                Address::repeat_byte(0x22),
                Some(default_info(2, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(20))],
            ),
        ]);
        let chunk2 = make_bundle(vec![(
            Address::repeat_byte(0x33),
            Some(default_info(3, 300)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(30))],
        )]);

        let normal_dir = TempDir::new().unwrap();
        let mut normal = MptCommitStore::open(normal_dir.path(), false).unwrap();
        normal.apply_bundle_state(&chunk1).unwrap();
        normal.commit().unwrap();
        normal.apply_bundle_state(&chunk2).unwrap();
        let (expected_version, expected_root) = normal.commit().unwrap();

        let bulk_dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut bulk = MptCommitStore::open_with_config(bulk_dir.path(), false, config).unwrap();
        bulk.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();
        bulk.bulk_ingest_bundle_chunk(&chunk1).unwrap();
        bulk.bulk_ingest_bundle_chunk(&chunk2).unwrap();
        let summary = bulk.finish_bulk_load().unwrap();

        assert_eq!(summary.chunks_committed, 2);
        assert_eq!(summary.final_version, expected_version);
        assert_eq!(summary.final_root, expected_root);
        assert_eq!(bulk.manifest.earliest_version, expected_version);
        assert_eq!(bulk.manifest.latest_version, expected_version);
        assert_eq!(bulk.published_version(), None);
        assert!(bulk.wal_store.as_ref().unwrap().lock().is_empty());
    }

    #[test]
    fn bulk_load_can_continue_with_normal_commits_and_reopen() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store =
            MptCommitStore::open_with_config(dir.path(), false, config.clone()).unwrap();

        let chunk = make_bundle(vec![(
            Address::repeat_byte(0x44),
            Some(default_info(1, 111)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(11))],
        )]);
        let followup = make_bundle(vec![(
            Address::repeat_byte(0x55),
            Some(default_info(2, 222)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(22))],
        )]);

        store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();
        store.bulk_ingest_bundle_chunk(&chunk).unwrap();
        let summary = store.finish_bulk_load().unwrap();
        assert_eq!(summary.final_version, 1);

        store.apply_bundle_state(&followup).unwrap();
        let (version, root) = store.commit().unwrap();
        assert_eq!(version, 2);
        store.flush_persist().unwrap();
        store.close().unwrap();

        let reopened = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
        assert_eq!(reopened.version(), 2);
        assert_eq!(reopened.manifest.get_root(2), Some(root));
    }

    #[test]
    fn bulk_load_resets_stale_derived_state() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let checkpoint =
            AccountTrieCheckpoint { version: 0, root: EMPTY_ROOT_HASH, trie: MptTree::default() };
        let checkpoint_bytes = bincode::serialize(&checkpoint).unwrap();
        let checkpoint_path = MptCommitStore::checkpoint_path(dir.path());
        fs::write(&checkpoint_path, checkpoint_bytes).unwrap();

        let wal_entry = CommitWalEntry {
            format_version: CommitWalEntry::FORMAT_VERSION,
            version: 1,
            state_root: B256::repeat_byte(0x77),
            account_root: B256::repeat_byte(0x77),
            deleted_accounts: Vec::new(),
            accounts: Vec::new(),
            upgrades: Vec::new(),
        };
        store.wal_store.as_ref().unwrap().lock().append_entry(&wal_entry).unwrap();

        store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();

        assert!(!checkpoint_path.exists());
        assert!(store.wal_store.as_ref().unwrap().lock().is_empty());
        assert_eq!(store.published_version(), None);
        assert!(!store.has_published_store());
    }

    #[test]
    fn dual_run_legacy_and_wal_first_match_roots_across_blocks() {
        let blocks = vec![
            make_bundle(vec![
                (
                    Address::repeat_byte(0x61),
                    Some(default_info(1, 100)),
                    revm_database::AccountStatus::Changed,
                    vec![(U256::from(1), U256::ZERO, U256::from(11))],
                ),
                (
                    Address::repeat_byte(0x62),
                    Some(default_info(2, 200)),
                    revm_database::AccountStatus::Changed,
                    vec![(U256::from(2), U256::ZERO, U256::from(22))],
                ),
            ]),
            make_bundle(vec![
                (
                    Address::repeat_byte(0x61),
                    Some(default_info(3, 300)),
                    revm_database::AccountStatus::Changed,
                    vec![(U256::from(3), U256::ZERO, U256::from(33))],
                ),
                (
                    Address::repeat_byte(0x63),
                    Some(default_info(4, 400)),
                    revm_database::AccountStatus::Changed,
                    vec![(U256::from(4), U256::ZERO, U256::from(44))],
                ),
            ]),
            make_bundle(vec![(
                Address::repeat_byte(0x62),
                Some(default_info(5, 500)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(5), U256::ZERO, U256::from(55))],
            )]),
        ];

        let legacy_dir = TempDir::new().unwrap();
        let mut legacy = MptCommitStore::open(legacy_dir.path(), false).unwrap();
        let wal_dir = TempDir::new().unwrap();
        let mut wal_config = MptConfig::default();
        wal_config.wal_first_commit = true;
        wal_config.wal_shadow_validate = true;
        wal_config.checkpoint_max_account_trie_nodes = 0;
        let mut wal_first =
            MptCommitStore::open_with_config(wal_dir.path(), false, wal_config).unwrap();

        for (idx, block) in blocks.iter().enumerate() {
            legacy.apply_bundle_state(block).unwrap();
            wal_first.apply_bundle_state(block).unwrap();

            let (legacy_version, legacy_root) = legacy.commit().unwrap();
            let (wal_version, wal_root) = wal_first.commit().unwrap();

            assert_eq!(legacy_version, wal_version, "block {}", idx + 1);
            assert_eq!(legacy_root, wal_root, "block {}", idx + 1);
        }
    }

    /// T5.1: open fresh dir -> version=0
    #[test]
    fn t5_1_open_fresh() {
        let dir = TempDir::new().unwrap();
        let store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), 0);
    }

    /// T5.2: read_only -> apply/commit/rollback all Err
    #[test]
    fn t5_2_read_only() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), true).unwrap();
        assert!(store.apply_bundle_state(&BundleState::default()).is_err());
        assert!(store.commit().is_err());
        assert!(store.rollback(0).is_err());
    }

    /// T5.3: writer double-open fails
    #[test]
    fn t5_3_writer_double_open() {
        let dir = TempDir::new().unwrap();
        let _store1 = MptCommitStore::open(dir.path(), false).unwrap();
        let result = MptCommitStore::open(dir.path(), false);
        assert!(result.is_err());
    }

    /// T5.4: empty bundle apply + commit -> version+1, root unchanged
    #[test]
    fn t5_4_empty_apply_commit() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root) = store.commit().unwrap();
        assert_eq!(ver, 1);
        assert_eq!(root, EMPTY_ROOT_HASH);
    }

    #[test]
    fn shadow_wal_commit_writes_version_entry() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.wal_shadow_validate = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0x11);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (version, root) = store.commit().unwrap();

        let wal = CommitWalStore::open(dir.path()).unwrap();
        let entry = wal.load_entry(version).unwrap().unwrap();
        assert_eq!(wal.latest_version(), version);
        assert_eq!(entry.version, version);
        assert_eq!(entry.state_root, root);
        assert_eq!(entry.accounts.len(), 1);
        assert_eq!(entry.accounts[0].address, addr);
    }

    #[test]
    fn shadow_wal_validate_allows_multi_block_commits() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.wal_shadow_validate = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr1 = Address::repeat_byte(0x17);
        let addr2 = Address::repeat_byte(0x18);

        let bundle1 = make_bundle(vec![(
            addr1,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(7))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (version1, _) = store.commit().unwrap();
        assert_eq!(version1, 1);

        let bundle2 = make_bundle(vec![
            (
                addr1,
                Some(default_info(2, 150)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::from(7), U256::from(9))],
            ),
            (
                addr2,
                Some(default_info(1, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(11))],
            ),
        ]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (version2, _) = store.commit().unwrap();
        assert_eq!(version2, 2);

        let wal = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(wal.latest_version(), 2);
        assert!(wal.load_entry(1).unwrap().is_some());
        assert!(wal.load_entry(2).unwrap().is_some());
    }

    #[test]
    fn shadow_wal_rollback_truncates_newer_entries() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0x12);
        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (version1, _) = store.commit().unwrap();

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (version2, _) = store.commit().unwrap();
        assert_eq!(version2, version1 + 1);

        store.rollback(version1).unwrap();

        let wal = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(wal.latest_version(), version1);
        assert!(wal.load_entry(version2).unwrap().is_none());
        assert!(wal.load_entry(version1).unwrap().is_some());
    }

    #[test]
    fn shadow_wal_prune_before_advances_floor() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.published_snapshot_interval = 1;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0x13);
        for version in 1..=3 {
            let bundle = make_bundle(vec![(
                addr,
                Some(default_info(version as u64, 100 + version as u64)),
                revm_database::AccountStatus::Changed,
                vec![],
            )]);
            store.apply_bundle_state(&bundle).unwrap();
            let (committed, _) = store.commit().unwrap();
            assert_eq!(committed, version);
        }

        store.prune_before(2).unwrap();

        let wal = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(wal.earliest_version(), 2);
        assert_eq!(wal.latest_version(), 3);
        assert!(wal.load_entry(1).unwrap().is_none());
        assert!(wal.load_entry(2).unwrap().is_some());
        assert!(wal.load_entry(3).unwrap().is_some());
    }

    #[test]
    fn wal_prune_respects_retained_published_snapshot_floor() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.published_snapshot_interval = 1;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
        let addr = Address::repeat_byte(0x52);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(11))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_v1, root1) = store.commit().unwrap();
        store.flush_persist().unwrap();
        // Create a published generation manually (overwriting the
        // incremental publish from the persist worker) to set up a
        // known baseline for prune-floor testing.
        let published1 =
            store.published_baseline.publish_generation(None, 1, root1, &[], &[]).unwrap();
        store.published_version.store(1, Ordering::Release);
        store.reload_published_view().unwrap();
        store.published_meta = Some(published1.meta.clone());
        let held_reader =
            store.published_baseline.open_published_store(&published1.meta).unwrap().unwrap();

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(22))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_v2, root2) = store.commit().unwrap();
        store.flush_persist().unwrap();
        // Also publish generation 2 so that compact_for_manifest can find
        // all generations referenced by the manifest.
        let published2 = store
            .published_baseline
            .publish_generation(Some(&published1.meta), 2, root2, &[], &[])
            .unwrap();
        store.published_version.store(2, Ordering::Release);
        store.reload_published_view().unwrap();
        store.published_meta = Some(published2.meta.clone());

        store.prune_before(2).unwrap();
        {
            let wal = store.wal_store.as_ref().unwrap().lock();
            assert_eq!(wal.earliest_version(), 1);
            assert_eq!(wal.latest_version(), 2);
        }

        drop(held_reader);
        store.prune_before(2).unwrap();
        {
            let wal = store.wal_store.as_ref().unwrap().lock();
            assert_eq!(wal.earliest_version(), 2);
            assert_eq!(wal.latest_version(), 2);
        }
    }

    #[test]
    fn wal_replay_reproduces_committed_roots() {
        let src_dir = TempDir::new().unwrap();
        let mut src_config = MptConfig::default();
        src_config.wal_first_commit = true;
        src_config.wal_shadow_validate = true;
        let mut src = MptCommitStore::open_with_config(src_dir.path(), false, src_config).unwrap();

        let addr1 = Address::repeat_byte(0x21);
        let addr2 = Address::repeat_byte(0x22);

        let bundle1 = make_bundle(vec![(
            addr1,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(7))],
        )]);
        src.apply_bundle_state(&bundle1).unwrap();
        let (version1, root1) = src.commit().unwrap();

        let bundle2 = make_bundle(vec![
            (
                addr1,
                Some(default_info(2, 150)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::from(7), U256::from(9))],
            ),
            (
                addr2,
                Some(default_info(1, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(11))],
            ),
        ]);
        src.apply_bundle_state(&bundle2).unwrap();
        let (version2, root2) = src.commit().unwrap();

        let wal = CommitWalStore::open(src_dir.path()).unwrap();

        let replay_dir = TempDir::new().unwrap();
        let replay = MptCommitStore::open(replay_dir.path(), false).unwrap();
        drop(replay);
        let mut dst = MptCommitStore::open(replay_dir.path(), false).unwrap();

        let entry1 = wal.load_entry(version1).unwrap().unwrap();
        dst.apply_wal_entry_inner(&entry1).unwrap();
        let (replayed_v1, replayed_root1) = dst.commit().unwrap();
        assert_eq!(replayed_v1, version1);
        assert_eq!(replayed_root1, root1);

        let entry2 = wal.load_entry(version2).unwrap().unwrap();
        dst.apply_wal_entry_inner(&entry2).unwrap();
        let (replayed_v2, replayed_root2) = dst.commit().unwrap();
        assert_eq!(replayed_v2, version2);
        assert_eq!(replayed_root2, root2);
    }

    #[test]
    fn wal_first_commit_can_lead_durable_frontier() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
        store.set_async_fail_mode(1);

        let addr = Address::repeat_byte(0x24);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 101)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(5))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (version, _) = store.commit().unwrap();
        assert_eq!(version, 1);

        let frontier = store.frontier();
        assert_eq!(frontier.logical_version, 1);
        assert_eq!(frontier.durable_version, 0);
    }

    #[test]
    fn wal_first_reopen_replays_latest_committed_version() {
        let dir = TempDir::new().unwrap();
        let expected_root;
        {
            let mut config = MptConfig::default();
            config.wal_first_commit = true;
            let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
            store.set_async_fail_mode(1);

            let addr = Address::repeat_byte(0x25);
            let bundle = make_bundle(vec![(
                addr,
                Some(default_info(1, 202)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(8))],
            )]);
            store.apply_bundle_state(&bundle).unwrap();
            let (_, root) = store.commit().unwrap();
            expected_root = root;
        }

        let mut reopen_config = MptConfig::default();
        reopen_config.wal_first_commit = true;
        let reopened = MptCommitStore::open_with_config(dir.path(), false, reopen_config).unwrap();
        assert_eq!(reopened.version(), 1);
        assert_eq!(reopened.frontier().durable_version, 1);
        assert_eq!(reopened.manifest.get_root(1), Some(expected_root));
    }

    #[test]
    fn wal_first_load_version_target_replays_from_durable_base() {
        let dir = TempDir::new().unwrap();
        let root_v1;
        let root_v2;
        {
            let mut config = MptConfig::default();
            config.wal_first_commit = true;
            let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

            let addr = Address::repeat_byte(0x27);
            let bundle1 = make_bundle(vec![(
                addr,
                Some(default_info(1, 111)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(7))],
            )]);
            store.apply_bundle_state(&bundle1).unwrap();
            root_v1 = store.commit().unwrap().1;

            let bundle2 = make_bundle(vec![(
                addr,
                Some(default_info(2, 222)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(14))],
            )]);
            store.apply_bundle_state(&bundle2).unwrap();
            root_v2 = store.commit().unwrap().1;
            store.close().unwrap();
        }

        let wal_meta_path = dir.path().join("changelog").join("meta.json");
        let mut wal_meta: serde_json::Value =
            serde_json::from_slice(&fs::read(&wal_meta_path).unwrap()).unwrap();
        wal_meta["durable_version"] = serde_json::Value::from(1);
        fs::write(&wal_meta_path, serde_json::to_vec_pretty(&wal_meta).unwrap()).unwrap();

        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut reopened = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        reopened.load_version_target(1).unwrap();
        assert_eq!(reopened.version(), 1);
        assert_eq!(reopened.frontier().committed_root, root_v1);
        assert_eq!(reopened.frontier().durable_version, 1);

        reopened.load_version_target(2).unwrap();
        assert_eq!(reopened.version(), 2);
        assert_eq!(reopened.frontier().committed_root, root_v2);
        assert_eq!(reopened.frontier().durable_version, 1);
    }

    #[test]
    fn wal_first_committed_account_trie_materializes_before_flush() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0x28);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 333)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(9))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let materialized =
            store.account_trie.committed().clone().into_materialized_tree(&store.persisted);
        if let Err(err) = materialized {
            panic!("{err}");
        }
    }

    #[test]
    fn bulk_then_wal_first_committed_account_trie_materializes_before_flush() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let prepop = make_bundle(vec![
            (
                Address::repeat_byte(0x29),
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(1))],
            ),
            (
                Address::repeat_byte(0x2a),
                Some(default_info(2, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(2))],
            ),
        ]);
        store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();
        store.bulk_ingest_bundle_chunk(&prepop).unwrap();
        store.finish_bulk_load().unwrap();

        let bundle = make_bundle(vec![(
            Address::repeat_byte(0x29),
            Some(default_info(3, 300)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(3))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let materialized =
            store.account_trie.committed().clone().into_materialized_tree(&store.persisted);
        if let Err(err) = materialized {
            panic!("{err}");
        }
    }

    #[test]
    fn bulk_then_wal_first_flush_persist_succeeds() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.published_snapshot_interval = 1;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let prepop = make_bundle(vec![
            (
                Address::repeat_byte(0x6a),
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(1))],
            ),
            (
                Address::repeat_byte(0x6b),
                Some(default_info(2, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(2))],
            ),
        ]);
        store.begin_bulk_load(BulkLoadOptions { retain_only_latest: true }).unwrap();
        store.bulk_ingest_bundle_chunk(&prepop).unwrap();
        store.finish_bulk_load().unwrap();

        let bundle = make_bundle(vec![(
            Address::repeat_byte(0x6a),
            Some(default_info(3, 300)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(3))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();
    }

    #[test]
    fn wal_first_committed_account_trie_stays_materializable_across_lagged_commits() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.published_snapshot_interval = 1;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addrs = [
            Address::repeat_byte(0x60),
            Address::repeat_byte(0x61),
            Address::repeat_byte(0x62),
            Address::repeat_byte(0x63),
        ];

        for step in 0..8u64 {
            let addr = addrs[(step as usize) % addrs.len()];
            let bundle = make_bundle(vec![(
                addr,
                Some(default_info(step + 1, 1_000 + step)),
                revm_database::AccountStatus::Changed,
                vec![
                    (U256::from(1), U256::ZERO, U256::from(step + 1)),
                    (U256::from(2), U256::ZERO, U256::from((step + 1) * 2)),
                ],
            )]);
            store.apply_bundle_state(&bundle).unwrap();
            store.commit().unwrap();

            let materialized =
                store.account_trie.committed().clone().into_materialized_tree(&store.persisted);
            if let Err(err) = materialized {
                panic!("step {step} committed trie lost materializability: {err}");
            }
        }

        let followup = make_bundle(vec![(
            addrs[0],
            Some(default_info(99, 9_999)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(999))],
        )]);
        store.apply_bundle_state(&followup).unwrap();
        store.commit().unwrap();
    }

    #[test]
    fn wal_first_publish_failure_is_nonfatal() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
        store.set_async_fail_mode(3);

        let addr = Address::repeat_byte(0x26);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 303)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(13))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (version, _) = store.commit().unwrap();
        assert_eq!(version, 1);

        // Published baseline failure is non-fatal: persist + WAL still work.
        // flush_persist should succeed because the persist itself completed.
        store.flush_persist().unwrap();
        assert_eq!(store.frontier().durable_version, 1);
        // Published may or may not have advanced depending on timing.
    }

    #[test]
    fn wal_first_commit_triggers_background_rewrite_publish() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.published_snapshot_interval = 64;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let bundle = make_bundle(vec![(
            Address::repeat_byte(0x27),
            Some(default_info(1, 404)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(4), U256::ZERO, U256::from(14))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (version, _root) = store.commit().unwrap();
        assert_eq!(version, 1);

        store.flush_persist().unwrap();
        assert_eq!(store.frontier().durable_version, 1);
        assert_eq!(store.published_version(), Some(1));
        let root = store.manifest.get_root(1).unwrap_or(EMPTY_ROOT_HASH);
        let published = wait_for_published_generation(&store, 1);
        assert_eq!(published.version, 1);
        assert_eq!(published.root, root);
        store.maybe_refresh_published_view().unwrap();
        assert!(store.has_published_store());
    }

    #[test]
    fn wal_first_rewrite_does_not_rollback_newer_published_meta() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        config.published_snapshot_interval = 64;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let bundle1 = make_bundle(vec![(
            Address::repeat_byte(0x31),
            Some(default_info(1, 101)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(11))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_v1, root1) = store.commit().unwrap();
        store.flush_persist().unwrap();

        let bundle2 = make_bundle(vec![(
            Address::repeat_byte(0x32),
            Some(default_info(2, 202)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(22))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_v2, root2) = store.commit().unwrap();
        store.flush_persist().unwrap();

        let published =
            store.published_baseline.publish_generation(None, 2, root2, &[], &[]).unwrap();
        store.published_version.store(2, Ordering::Release);
        store.reload_published_view().unwrap();
        store.published_meta = Some(published.meta.clone());

        assert_eq!(store.published_version(), Some(2));
        assert_eq!(store.published_baseline.load_meta().unwrap().unwrap().version, 2);

        let tx = store.published_rewrite_tx.as_ref().unwrap().clone();
        let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<()>>(0);
        tx.send(PublishedRewriteJob {
            barrier_only: false,
            target_version: 1,
            state_root: root1,
            segments: None,
            done: Some(done_tx),
        })
        .unwrap();
        done_rx.recv().unwrap().unwrap();

        store.reload_published_view().unwrap();
        assert_eq!(store.published_version(), Some(2));
        assert_eq!(store.published_baseline.load_meta().unwrap().unwrap().version, 2);
        assert_eq!(store.published_meta.as_ref().map(|meta| meta.version), Some(2));
    }

    /// T5.5: single account nonce/balance update -> state_root matches reference
    #[test]
    fn t5_5_single_account_update() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 1000);
        let bundle = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();

        // Compute reference root
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();

        assert_eq!(root, expected);
    }

    /// T5.6: single account storage update -> state_root matches reference
    #[test]
    fn t5_6_single_account_storage() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x02);
        let info = default_info(1, 500);
        let slot_key = U256::from(1);
        let slot_val = U256::from(42);
        let bundle = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot_key, U256::ZERO, slot_val)],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();

        // Compute reference storage root
        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        slot_val.encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot), &encoded_val);
        let storage_root = storage_hb.root();

        // Compute reference state root
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();

        assert_eq!(root, expected);
    }

    /// T5.7: only change account fields, no storage -> reuse old storage_root
    #[test]
    fn t5_7_reuse_storage_root() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x03);
        let info1 = default_info(1, 100);
        let slot_key = U256::from(5);
        let slot_val = U256::from(99);

        // Block 1: create account with storage
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info1),
            revm_database::AccountStatus::Changed,
            vec![(slot_key, U256::ZERO, slot_val)],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        // Block 2: only update balance (no storage changes)
        let info2 = default_info(2, 200);
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info2.clone()),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Root should change (balance changed) but storage_root should be same
        assert_ne!(root1, root2);

        // Verify: compute expected root2 with the storage_root from block 1
        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        slot_val.encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot), &encoded_val);
        let storage_root = storage_hb.root();

        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info2.nonce,
            balance: info2.balance,
            storage_root,
            code_hash: info2.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();
        assert_eq!(root2, expected);
    }

    /// T5.8: storage_wiped=true -> old storage cleared, new slots applied
    #[test]
    fn t5_8_storage_wiped() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x04);
        let info = default_info(1, 100);
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        // Block 1: account with slot1
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot1, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: destroy+recreate with only slot2
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::DestroyedChanged,
            vec![(slot2, U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: only slot2 in storage (slot1 wiped)
        let hashed_slot2 = keccak256(slot2.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        U256::from(20).encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot2), &encoded_val);
        let storage_root = storage_hb.root();

        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        assert_eq!(root2, hb.root());
    }

    /// T5.9: ZERO slot -> leaf deleted
    #[test]
    fn t5_9_zero_slot_deletes() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x05);
        let info = default_info(1, 100);
        let slot = U256::from(1);

        // Block 1: set slot to nonzero
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(77))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: set slot to zero (delete)
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(77), U256::ZERO)],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: no storage slots -> EMPTY_ROOT_HASH for storage
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        assert_eq!(root2, hb.root());
    }

    /// T5.10: selfdestruct without rebuild -> account leaf deleted
    #[test]
    fn t5_10_selfdestruct_no_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x06);
        let info = default_info(1, 100);

        // Block 1: create account
        let bundle1 =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: destroy
        let bundle2 =
            make_bundle(vec![(addr, None, revm_database::AccountStatus::Destroyed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: empty trie
        assert_eq!(root2, EMPTY_ROOT_HASH);
    }

    /// T5.11: selfdestruct then rebuild -> account leaf kept, storage_wiped
    #[test]
    fn t5_11_selfdestruct_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x07);
        let info1 = default_info(1, 100);

        // Block 1: create account with storage
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info1),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: destroy + recreate with new nonce, no storage
        let info2 = default_info(0, 50);
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info2.clone()),
            revm_database::AccountStatus::DestroyedChanged,
            vec![],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: account exists with empty storage
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info2.nonce,
            balance: info2.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: info2.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();

        // But EIP-161: empty account with EMPTY_ROOT_HASH storage => deleted
        // info2: nonce=0, balance=50, code_hash=KECCAK_EMPTY -> not empty (balance > 0)
        assert_eq!(root2, expected);
    }

    /// T5.12: double apply -> Err
    #[test]
    fn t5_12_double_apply() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let result = store.apply_bundle_state(&BundleState::default());
        assert!(result.is_err());
    }

    /// T5.13: commit without apply -> Err
    #[test]
    fn t5_13_commit_without_apply() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let result = store.commit();
        assert!(result.is_err());
    }

    /// T5.14: after successful commit, working state cleared
    #[test]
    fn t5_14_working_state_cleared() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        assert!(!store.applied_this_block);
        assert!(store.dirty_accounts.is_empty());
        assert!(store.working_storage_tries_empty());
    }

    /// T5.15: commit failure -> poisoned
    #[test]
    fn t5_15_commit_failure_poisoned() {
        let dir = TempDir::new().unwrap();

        for fp in [
            CommitFailPoint::BeforePersist,
            CommitFailPoint::AfterPersistBeforeManifest,
            CommitFailPoint::ManifestSave,
        ] {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_fail_point(Some(fp));
            store.apply_bundle_state(&BundleState::default()).unwrap();
            let result = store.commit();
            assert!(result.is_err(), "expected error for failpoint {fp:?}");
            assert!(store.poisoned);
            assert!(store.commit().is_err());
            assert!(store.apply_bundle_state(&BundleState::default()).is_err());
            // Close before reopening
            store.close().unwrap();
        }
    }

    /// T5.16: load_version clears poisoned state
    #[test]
    fn t5_16_load_version_clears_poisoned() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit block 1 successfully
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();
        assert_eq!(store.version(), 1);

        // Set failpoint for block 2
        store.set_fail_point(Some(CommitFailPoint::BeforePersist));
        store.apply_bundle_state(&BundleState::default()).unwrap();
        assert!(store.commit().is_err());
        assert!(store.poisoned);

        // Recover
        store.set_fail_point(None);
        store.load_version().unwrap();
        assert!(!store.poisoned);
        assert_eq!(store.version(), 1);
    }

    /// T5.17: rollback truncates future versions
    #[test]
    fn t5_17_rollback() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit 3 blocks
        for _ in 0..3 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 3);

        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        assert!(store.manifest.get_root(2).is_none());
        assert!(store.manifest.get_root(3).is_none());
    }

    /// T5.18: rollback then continue committing
    #[test]
    fn t5_18_rollback_then_commit() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        for _ in 0..3 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }

        store.rollback(1).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, _) = store.commit().unwrap();
        assert_eq!(ver, 2);
    }

    /// T5.19: close releases writer lock, reopen succeeds
    #[test]
    fn t5_19_close_reopen() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.close().unwrap();
        // Reopen should succeed
        let _store2 = MptCommitStore::open(dir.path(), false).unwrap();
    }

    #[test]
    fn t5_20_checkpoint_written_and_used_on_reopen() {
        let dir = TempDir::new().unwrap();
        let checkpoint_path = MptCommitStore::checkpoint_path(dir.path());

        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x44);
        let info = default_info(1, 123);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(9))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
        store.close().unwrap();

        assert!(checkpoint_path.exists(), "checkpoint should be written on close");

        let store2 = MptCommitStore::open(dir.path(), false).unwrap();
        assert!(store2.loaded_from_checkpoint(), "reopen should load account trie checkpoint");
    }

    #[test]
    fn t5_20b_checkpoint_skipped_above_node_threshold() {
        let dir = TempDir::new().unwrap();
        let checkpoint_path = MptCommitStore::checkpoint_path(dir.path());

        let mut config = MptConfig::default();
        config.checkpoint_max_account_trie_nodes = 1;
        let mut store =
            MptCommitStore::open_with_config(dir.path(), false, config.clone()).unwrap();
        let bundle = make_bundle(vec![
            (
                Address::repeat_byte(0x45),
                Some(default_info(1, 123)),
                revm_database::AccountStatus::Changed,
                vec![],
            ),
            (
                Address::repeat_byte(0x46),
                Some(default_info(2, 456)),
                revm_database::AccountStatus::Changed,
                vec![],
            ),
        ]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
        store.close().unwrap();

        assert!(!checkpoint_path.exists(), "checkpoint should be skipped above threshold");

        let reopened = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
        assert!(!reopened.loaded_from_checkpoint(), "reopen should fall back without checkpoint");
    }

    #[test]
    fn t5_20c_close_is_idempotent_after_explicit_close() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let bundle = make_bundle(vec![(
            Address::repeat_byte(0x47),
            Some(default_info(1, 123)),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        store.close().unwrap();
        store.close().unwrap();
    }

    // ── Phase 4 parallel tests ──

    /// Helper: create a bundle with many accounts each having storage slots.
    fn make_storage_heavy_bundle(num_accounts: usize, slots_per_account: usize) -> BundleState {
        let mut accounts = Vec::new();
        for i in 0..num_accounts {
            let addr = Address::from_word(B256::from(U256::from(i + 1)));
            let info = default_info(1, 1000);
            let storage: Vec<(U256, U256, U256)> = (0..slots_per_account)
                .map(|s| (U256::from(s), U256::ZERO, U256::from(s + 1)))
                .collect();
            accounts.push((addr, Some(info), revm_database::AccountStatus::Changed, storage));
        }
        make_bundle(accounts)
    }

    /// T1.6: set_parallelism_thresholds() can override default values in tests.
    #[test]
    fn t1_6_set_parallelism_thresholds() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.parallelism, ParallelismThresholds::default());

        let custom = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        store.set_parallelism_thresholds(custom);
        assert_eq!(store.parallelism, custom);
    }

    /// T3.1: open() initializes parallelism with Default values.
    #[test]
    fn t3_1_default_parallelism() {
        let dir = TempDir::new().unwrap();
        let store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.parallelism, ParallelismThresholds::default());
    }

    /// T3.2: force storage_tries parallel path (threshold=1) -> root matches serial.
    #[test]
    fn t3_2_forced_parallel_storage_root_matches_serial() {
        let dir_serial = TempDir::new().unwrap();
        let dir_parallel = TempDir::new().unwrap();

        let bundle = make_storage_heavy_bundle(10, 5);

        // Serial path (high threshold)
        let mut store_s = MptCommitStore::open(dir_serial.path(), false).unwrap();
        store_s.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });
        store_s.apply_bundle_state(&bundle).unwrap();
        let (_, root_serial) = store_s.commit().unwrap();

        // Parallel path (threshold=1)
        let mut store_p = MptCommitStore::open(dir_parallel.path(), false).unwrap();
        store_p.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 1,
            account_frontier_min: 99999,
        });
        store_p.apply_bundle_state(&bundle).unwrap();
        let (_, root_parallel) = store_p.commit().unwrap();

        assert_eq!(root_serial, root_parallel);
    }

    /// T3.3: force storage_tries serial path (high threshold) -> root correct.
    #[test]
    fn t3_3_forced_serial_storage_root_correct() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(5, 3);

        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, root) = store.commit().unwrap();
        assert_eq!(ver, 1);
        assert_ne!(root, EMPTY_ROOT_HASH);
    }

    /// T3.4: force account_trie parallel hash path -> state_root matches serial.
    #[test]
    fn t3_4_forced_parallel_account_hash_matches_serial() {
        let dir_serial = TempDir::new().unwrap();
        let dir_parallel = TempDir::new().unwrap();

        // Many accounts to get a wide frontier
        let bundle = make_storage_heavy_bundle(50, 2);

        let mut store_s = MptCommitStore::open(dir_serial.path(), false).unwrap();
        store_s.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });
        store_s.apply_bundle_state(&bundle).unwrap();
        let (_, root_serial) = store_s.commit().unwrap();

        let mut store_p = MptCommitStore::open(dir_parallel.path(), false).unwrap();
        store_p.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 1,
        });
        store_p.apply_bundle_state(&bundle).unwrap();
        let (_, root_parallel) = store_p.commit().unwrap();

        assert_eq!(root_serial, root_parallel);
    }

    /// T3.5: same blocks, forced serial vs forced parallel -> identical results.
    #[test]
    fn t3_5_serial_vs_parallel_identical() {
        let dir_serial = TempDir::new().unwrap();
        let dir_parallel = TempDir::new().unwrap();

        let bundles: Vec<BundleState> =
            (0..3).map(|i| make_storage_heavy_bundle(10 + i * 5, 3)).collect();

        let mut store_s = MptCommitStore::open(dir_serial.path(), false).unwrap();
        store_s.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });

        let mut store_p = MptCommitStore::open(dir_parallel.path(), false).unwrap();
        store_p.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 1,
            account_frontier_min: 1,
        });

        for bundle in &bundles {
            store_s.apply_bundle_state(bundle).unwrap();
            let (vs, rs) = store_s.commit().unwrap();

            store_p.apply_bundle_state(bundle).unwrap();
            let (vp, rp) = store_p.commit().unwrap();

            assert_eq!(vs, vp);
            assert_eq!(rs, rp);
        }
    }

    /// T3.6: multi-account parallel commit + reopen/load_version -> consistent.
    #[test]
    fn t3_6_parallel_reopen_consistent() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(20, 4);

        let (version, root);
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.apply_bundle_state(&bundle).unwrap();
            let result = store.commit().unwrap();
            version = result.0;
            root = result.1;
            store.close().unwrap();
        }

        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            assert_eq!(store.version(), version);

            // Commit empty block: root should not change
            store.apply_bundle_state(&BundleState::default()).unwrap();
            let (_, root_after) = store.commit().unwrap();
            assert_eq!(root_after, root);
        }
    }

    /// T3.7: parallel commit artifacts are fully persisted; reopen/load_version root matches.
    #[test]
    fn t3_7_parallel_artifacts_persisted() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(15, 5);

        let root1;
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.apply_bundle_state(&bundle).unwrap();
            let (_, r) = store.commit().unwrap();
            root1 = r;
            store.close().unwrap();
        }

        {
            let store = MptCommitStore::open(dir.path(), false).unwrap();
            assert_eq!(store.version(), 1);
            // Verify the root from manifest matches what we committed
            let stored_root = store.manifest.get_root(1).unwrap();
            assert_eq!(stored_root, root1);
        }
    }

    /// T3.8: parallel commit with failpoints -> same behavior as serial.
    #[test]
    fn t3_8_parallel_failpoints() {
        for fp in [
            CommitFailPoint::BeforePersist,
            CommitFailPoint::AfterPersistBeforeManifest,
            CommitFailPoint::ManifestSave,
        ] {
            let dir = TempDir::new().unwrap();
            let bundle = make_storage_heavy_bundle(5, 2);

            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.set_fail_point(Some(fp));
            store.apply_bundle_state(&bundle).unwrap();
            let result = store.commit();
            assert!(result.is_err(), "expected error for failpoint {fp:?}");
            assert!(store.poisoned);
            store.close().unwrap();
        }
    }

    /// T3.9: after parallel commit success, dirty state is cleared.
    #[test]
    fn t3_9_parallel_clears_dirty_state() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(10, 3);

        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 1,
            account_frontier_min: 1,
        });
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        assert!(!store.applied_this_block);
        assert!(store.dirty_accounts.is_empty());
        assert!(store.working_storage_tries_empty());
    }

    // ── Phase 5 tests ──

    /// T6.1: read_only open uses shared lock; verified by: reader close then reopen succeeds
    /// Note: Two concurrent readers at the same RocksDB path is blocked by RocksDB's internal
    /// directory lock, so we verify the file-level shared lock semantics via close+reopen.
    /// Full reader+reader coexistence requires sub-process testing (see integration tests).
    #[test]
    fn t6_1_shared_lock_readers() {
        let dir = TempDir::new().unwrap();
        // Open reader, verify it acquires shared lock (file_lock is Some)
        let mut reader1 = MptCommitStore::open(dir.path(), true).unwrap();
        assert!(reader1.file_lock.is_some());
        reader1.close().unwrap();
        // Can reopen as reader after close
        let _reader2 = MptCommitStore::open(dir.path(), true).unwrap();
    }

    /// T6.2: writer open then reader open fails
    #[test]
    fn t6_2_writer_blocks_reader() {
        let dir = TempDir::new().unwrap();
        let _writer = MptCommitStore::open(dir.path(), false).unwrap();
        let result = MptCommitStore::open(dir.path(), true);
        assert!(result.is_err());
    }

    /// T6.3: reader open then writer open fails
    #[test]
    fn t6_3_reader_blocks_writer() {
        let dir = TempDir::new().unwrap();
        let _reader = MptCommitStore::open(dir.path(), true).unwrap();
        let result = MptCommitStore::open(dir.path(), false);
        assert!(result.is_err());
    }

    /// T6.4: close releases shared/exclusive lock
    #[test]
    fn t6_4_close_releases_lock() {
        let dir = TempDir::new().unwrap();
        let mut reader = MptCommitStore::open(dir.path(), true).unwrap();
        reader.close().unwrap();
        // Writer should succeed now
        let _writer = MptCommitStore::open(dir.path(), false).unwrap();
    }

    /// T6.4b: implicit drop also releases the writer lock and background resources.
    #[test]
    fn t6_4b_drop_releases_lock() {
        let dir = TempDir::new().unwrap();
        {
            let mut writer = MptCommitStore::open(dir.path(), false).unwrap();
            writer.apply_bundle_state(&BundleState::default()).unwrap();
            writer.commit().unwrap();
        }
        let _writer2 = MptCommitStore::open(dir.path(), false).unwrap();
    }

    /// T6.5: prune_before correctly updates manifest earliest_version
    #[test]
    fn t6_5_prune_before() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        for _ in 0..5 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 5);

        store.prune_before(3).unwrap();
        assert_eq!(store.manifest.earliest_version, 3);
        assert!(store.manifest.get_root(1).is_none());
        assert!(store.manifest.get_root(2).is_none());
        assert!(store.manifest.get_root(3).is_some());
        assert!(store.manifest.get_root(5).is_some());
    }

    /// T6.6: prune_before(latest) is legal, keeps only latest
    #[test]
    fn t6_6_prune_before_latest() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        for _ in 0..3 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }

        store.prune_before(3).unwrap();
        assert_eq!(store.manifest.earliest_version, 3);
        assert!(store.manifest.get_root(3).is_some());
        // Only version 3 should remain (version 0 was pruned)
        assert!(store.manifest.get_root(0).is_none());
    }

    /// T6.7: prune_before(out of range) -> Err
    #[test]
    fn t6_7_prune_before_out_of_range() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        assert!(store.prune_before(-1).is_err());
        assert!(store.prune_before(999).is_err());
    }

    /// T6.8: gc returns stats and doesn't change manifest/latest version
    #[test]
    fn t6_8_gc_returns_stats() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 1000);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let ver_before = store.version();
        let stats = store.gc().unwrap();
        assert!(stats.scanned_nodes > 0);
        assert_eq!(stats.deleted_nodes, 0); // no orphans yet
        assert_eq!(store.version(), ver_before);
    }

    /// T6.9: read_only prune/gc/importer all Err
    #[test]
    fn t6_9_read_only_maintenance_errors() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), true).unwrap();
        assert!(store.prune_before(0).is_err());
        assert!(store.gc().is_err());
        assert!(store.importer(1, B256::ZERO).is_err());
    }

    /// T6.10: applied_this_block=true blocks prune/gc
    #[test]
    fn t6_10_applied_blocks_maintenance() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        assert!(store.prune_before(0).is_err());
        assert!(store.gc().is_err());
    }

    /// T6.11: exporter(valid version) creates and streams
    #[test]
    fn t6_11_exporter_valid_version() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 100);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let mut exp = store.exporter(1).unwrap();
        let meta = exp.meta();
        assert_eq!(meta.version, 1);
        // Should produce at least one node
        assert!(exp.next_node().unwrap().is_some());
        exp.close().unwrap();
    }

    /// T6.12: exporter(missing version) -> Err
    #[test]
    fn t6_12_exporter_missing_version() {
        let dir = TempDir::new().unwrap();
        let store = MptCommitStore::open(dir.path(), false).unwrap();
        assert!(store.exporter(999).is_err());
    }

    /// T6.13: importer(fresh DB) succeeds and load_version works after
    #[test]
    fn t6_13_importer_fresh_db() {
        // Build source
        let src_dir = TempDir::new().unwrap();
        let mut src_store = MptCommitStore::open(src_dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 100);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        src_store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = src_store.commit().unwrap();

        // Export
        let mut exp = src_store.exporter(1).unwrap();
        let mut nodes = Vec::new();
        while let Some(n) = exp.next_node().unwrap() {
            nodes.push(n);
        }
        exp.close().unwrap();
        src_store.close().unwrap();

        // Import into fresh DB
        let dst_dir = TempDir::new().unwrap();
        let mut dst_store = MptCommitStore::open(dst_dir.path(), false).unwrap();

        {
            let mut imp = dst_store.importer(1, root).unwrap();
            for n in &nodes {
                imp.add_node(n).unwrap();
            }
            imp.close().unwrap();
        }

        // After import, store should reflect new state
        assert_eq!(dst_store.version(), 1);
        assert_eq!(dst_store.manifest.get_root(1), Some(root));
    }

    /// T6.14: importer(non-fresh DB) -> Err
    #[test]
    fn t6_14_importer_non_fresh() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit something first
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        let result = store.importer(2, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn t6_14b_wal_first_import_can_continue_committing_and_reopen() {
        let src_dir = TempDir::new().unwrap();
        let mut src_store = MptCommitStore::open(src_dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x41);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 123)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(9))],
        )]);
        src_store.apply_bundle_state(&bundle).unwrap();
        let (_, root_v1) = src_store.commit().unwrap();

        let mut exp = src_store.exporter(1).unwrap();
        let mut nodes = Vec::new();
        while let Some(node) = exp.next_node().unwrap() {
            nodes.push(node);
        }
        exp.close().unwrap();
        src_store.close().unwrap();

        let dst_dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut dst_store =
            MptCommitStore::open_with_config(dst_dir.path(), false, config.clone()).unwrap();

        {
            let mut imp = dst_store.importer(1, root_v1).unwrap();
            for node in &nodes {
                imp.add_node(node).unwrap();
            }
            imp.close().unwrap();
        }

        assert_eq!(dst_store.version(), 1);
        assert_eq!(dst_store.frontier().durable_version, 1);

        dst_store.apply_bundle_state(&BundleState::default()).unwrap();
        let (version2, root_v2) = dst_store.commit().unwrap();
        assert_eq!(version2, 2);
        assert_eq!(root_v2, root_v1);
        dst_store.close().unwrap();

        let reopened = MptCommitStore::open_with_config(dst_dir.path(), false, config).unwrap();
        assert_eq!(reopened.version(), 2);
        assert_eq!(reopened.manifest.get_root(1), Some(root_v1));
        assert_eq!(reopened.manifest.get_root(2), Some(root_v2));
        assert_eq!(reopened.frontier().durable_version, 2);
    }

    #[test]
    fn t6_14c_wal_first_import_resets_derived_state() {
        let src_dir = TempDir::new().unwrap();
        let mut src_store = MptCommitStore::open(src_dir.path(), false).unwrap();
        let bundle = make_bundle(vec![(
            Address::repeat_byte(0x42),
            Some(default_info(1, 321)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(7))],
        )]);
        src_store.apply_bundle_state(&bundle).unwrap();
        let (_, root_v1) = src_store.commit().unwrap();

        let mut exp = src_store.exporter(1).unwrap();
        let mut nodes = Vec::new();
        while let Some(node) = exp.next_node().unwrap() {
            nodes.push(node);
        }
        exp.close().unwrap();
        src_store.close().unwrap();

        let dst_dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let mut dst_store =
            MptCommitStore::open_with_config(dst_dir.path(), false, config).unwrap();

        let checkpoint =
            AccountTrieCheckpoint { version: 0, root: EMPTY_ROOT_HASH, trie: MptTree::default() };
        let checkpoint_path = MptCommitStore::checkpoint_path(dst_dir.path());
        fs::write(&checkpoint_path, bincode::serialize(&checkpoint).unwrap()).unwrap();
        let wal_entry = CommitWalEntry {
            format_version: CommitWalEntry::FORMAT_VERSION,
            version: 1,
            state_root: B256::repeat_byte(0x88),
            account_root: B256::repeat_byte(0x88),
            deleted_accounts: Vec::new(),
            accounts: Vec::new(),
            upgrades: Vec::new(),
        };
        dst_store.wal_store.as_ref().unwrap().lock().append_entry(&wal_entry).unwrap();

        {
            let mut imp = dst_store.importer(1, root_v1).unwrap();
            for node in &nodes {
                imp.add_node(node).unwrap();
            }
            imp.close().unwrap();
        }

        assert!(!checkpoint_path.exists());
        assert!(dst_store.wal_store.as_ref().unwrap().lock().is_empty());
        assert_eq!(dst_store.version(), 1);
        assert_eq!(dst_store.frontier().durable_version, 1);
        assert_eq!(dst_store.published_version(), None);
        assert!(!dst_store.has_published_store());
    }

    /// T6.15: account_proof(version) for specified committed root is correct
    #[test]
    fn t6_15_account_proof_version() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 1000);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();

        let proof = store.account_proof(1, addr, &[]).unwrap();
        assert!(proof.info.is_some());
        proof.verify(root).unwrap();
    }

    /// T6.16: rollback then exporter/proof only sees new latest/kept versions
    #[test]
    fn t6_16_rollback_then_proof() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info1 = default_info(1, 100);
        let bundle1 =
            make_bundle(vec![(addr, Some(info1), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        let info2 = default_info(2, 200);
        let bundle2 =
            make_bundle(vec![(addr, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();

        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);

        // Version 2 should be gone
        assert!(store.exporter(2).is_err());
        assert!(store.account_proof(2, addr, &[]).is_err());

        // Version 1 should work
        let proof = store.account_proof(1, addr, &[]).unwrap();
        proof.verify(root1).unwrap();
    }

    /// T6.17: historical version proof before prune works, after prune+gc -> Err
    #[test]
    fn t6_17_historical_proof_prune_gc() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);

        // v1: create account with large storage to ensure hash children
        let info1 = default_info(1, 100);
        let slots1: Vec<(U256, U256, U256)> =
            (0..10).map(|i| (U256::from(i), U256::ZERO, U256::from(i + 100))).collect();
        let bundle1 =
            make_bundle(vec![(addr, Some(info1), revm_database::AccountStatus::Changed, slots1)]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        // v2: completely different state
        let addr2 = Address::repeat_byte(0x02);
        let info2 = default_info(1, 999);
        let bundle2 =
            make_bundle(vec![(addr2, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // v1 proof should work before prune
        let proof1 = store.account_proof(1, addr, &[]).unwrap();
        proof1.verify(root1).unwrap();

        // Prune v1 + gc
        store.prune_before(2).unwrap();
        store.gc().unwrap();

        // v1 should no longer be in manifest
        assert!(store.account_proof(1, addr, &[]).is_err());

        // v2 should still work
        let proof2 = store.account_proof(2, addr2, &[]).unwrap();
        proof2.verify(root2).unwrap();
    }

    // ── Storage trie cache tests ──

    /// Cross-block cache hit: account A modified in block 1 and block 2.
    /// Block 2 should use the cached trie instead of reloading from RocksDB.
    #[test]
    fn storage_trie_cache_cross_block_hit() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0xAA);
        let info = default_info(1, 1000);
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        // Block 1: create account with slot1
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot1, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        // After commit, storage trie should be in cache
        let hashed_addr = keccak256(addr);
        assert!(
            store.storage_trie_cache_contains(&hashed_addr),
            "storage trie should be in cache after commit"
        );
        let cached = match store.clone_cached_storage_trie(&hashed_addr) {
            Some(entry) => entry,
            None => panic!("missing cached trie"),
        };
        store.cache_storage_trie(hashed_addr, cached);

        // Block 2: add slot2 to same account
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot2, U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();

        // The cached snapshot should remain, and the handle should expose a working copy.
        assert!(
            store.storage_trie_cache_contains(&hashed_addr),
            "cached snapshot should remain after loading a working copy"
        );
        assert!(store.contains_working_trie(&hashed_addr), "trie should be in working state");

        let (_, root2) = store.commit().unwrap();
        assert_ne!(root1, root2, "root should change after adding slot2");

        // After block 2 commit, trie should be back in cache
        assert!(store.storage_trie_cache_contains(&hashed_addr));
    }

    #[test]
    fn storage_trie_cache_prefers_snapshot_cows_after_sync_commit() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.async_blob_threshold = 0;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0xAB);
        let info = default_info(1, 1000);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        let cached = store
            .clone_cached_storage_trie(&hashed_addr)
            .expect("expected cached entry after sync commit");
        let trie = cached;
        assert!(matches!(trie.root_ref(), CowRootRef::Lazy(CowLazyNodeRef::Segment(_))));
        assert!(trie.arena_nodes().is_empty());
        store.cache_storage_trie(hashed_addr, trie);
    }

    #[test]
    fn storage_trie_cache_prefers_snapshot_cows_after_async_commit() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.async_blob_threshold = usize::MAX;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0xAC);
        let info = default_info(1, 1000);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        let cached = store
            .clone_cached_storage_trie(&hashed_addr)
            .expect("expected cached entry after async commit");
        let trie = cached;
        assert!(matches!(trie.root_ref(), CowRootRef::Lazy(CowLazyNodeRef::Segment(_))));
        assert!(trie.arena_nodes().is_empty());
        store.cache_storage_trie(hashed_addr, trie);
        store.flush_persist().unwrap();
    }

    #[test]
    fn storage_trie_cache_reloads_l2_hits_as_snapshot_cows() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.async_blob_threshold = 0;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0xAD);
        let info = default_info(1, 1000);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        let bundle2 = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        let cached = store
            .clone_cached_storage_trie(&hashed_addr)
            .expect("expected cached entry after hot reuse");
        let trie = cached;
        assert!(matches!(trie.root_ref(), CowRootRef::Lazy(CowLazyNodeRef::Segment(_))));
        store.cache_storage_trie(hashed_addr, trie);
    }

    #[test]
    fn storage_trie_handle_versions_track_base_and_working_state() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.async_blob_threshold = 0;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0xAE);
        let hashed_addr = keccak256(addr);
        let info = default_info(1, 1000);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        assert_eq!(store.storage_trie_handle_versions(&hashed_addr), Some((0, Some(1))));
        store.commit().unwrap();
        assert_eq!(store.storage_trie_handle_versions(&hashed_addr), Some((1, None)));

        let bundle2 = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        assert_eq!(store.storage_trie_handle_versions(&hashed_addr), Some((1, Some(2))));
        store.commit().unwrap();
        assert_eq!(store.storage_trie_handle_versions(&hashed_addr), Some((2, None)));
    }

    #[test]
    fn account_trie_handle_versions_track_commit_load_and_rollback() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.async_blob_threshold = 0;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let addr = Address::repeat_byte(0xAF);
        let info = default_info(1, 1000);
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        assert_eq!(store.account_trie_handle_versions(), (0, Some(1)));
        store.commit().unwrap();
        assert_eq!(store.account_trie_handle_versions(), (1, None));

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 2000)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        assert_eq!(store.account_trie_handle_versions(), (1, Some(2)));
        store.commit().unwrap();
        assert_eq!(store.account_trie_handle_versions(), (2, None));

        store.rollback(1).unwrap();
        assert_eq!(store.account_trie_handle_versions(), (1, None));

        store.load_version().unwrap();
        assert_eq!(store.account_trie_handle_versions(), (1, None));
    }

    #[test]
    fn account_trie_load_version_restores_lazy_snapshot() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.async_blob_threshold = 0;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let bundle = make_bundle(
            (0..64_u8)
                .map(|byte| {
                    (
                        Address::repeat_byte(byte.wrapping_add(1)),
                        Some(default_info(1, 1000 + byte as u64)),
                        revm_database::AccountStatus::Changed,
                        vec![(U256::from(byte as u64 + 1), U256::ZERO, U256::from(10))],
                    )
                })
                .collect(),
        );
        store.apply_bundle_state(&bundle).unwrap();
        let (_version, root) = store.commit().unwrap();

        let full = persisted::load_tree_from_root(&store.persisted, root).unwrap();
        let full_nodes = full.arena_len();

        store.load_version().unwrap();
        assert!(
            store.account_trie_arena_len() < full_nodes,
            "load_version should restore a lazy account trie snapshot",
        );

        let followup = make_bundle(vec![(
            Address::repeat_byte(1),
            Some(default_info(2, 2000)),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&followup).unwrap();
        store.commit().unwrap();
    }

    #[test]
    fn storage_trie_cache_bulk_block_caches_all_accounts_within_capacity() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.storage_trie_cache_capacity = 2;
        config.async_blob_threshold = 0;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let accounts = [0xD1_u8, 0xD2_u8, 0xD3_u8, 0xD4_u8];
        let bundle = make_bundle(
            accounts
                .into_iter()
                .enumerate()
                .map(|(idx, byte)| {
                    (
                        Address::repeat_byte(byte),
                        Some(default_info(1, 3000 + idx as u64)),
                        revm_database::AccountStatus::Changed,
                        vec![(U256::from(idx as u64 + 1), U256::ZERO, U256::from(idx as u64 + 10))],
                    )
                })
                .collect(),
        );

        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        for byte in accounts {
            assert!(
                store.storage_trie_cache_contains(&keccak256(Address::repeat_byte(byte))),
                "bulk account should remain cached when it fits in the LRU capacity",
            );
        }
    }

    /// Selfdestruct (storage_wiped) should evict cached trie and not reuse it.
    #[test]
    fn storage_trie_cache_selfdestruct_evicts() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0xBB);
        let info = default_info(1, 1000);
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        // Block 1: create account with slot1
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot1, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        assert!(store.storage_trie_cache_contains(&hashed_addr));

        // Block 2: selfdestruct + recreate with slot2 only
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::DestroyedChanged,
            vec![(slot2, U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();

        // Cache should be evicted after apply with storage_wiped
        assert!(!store.storage_trie_cache_contains(&hashed_addr));

        let (_, root2) = store.commit().unwrap();

        // Verify: only slot2 in storage (slot1 was wiped)
        let hashed_slot2 = keccak256(slot2.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        U256::from(20).encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot2), &encoded_val);
        let expected_storage_root = storage_hb.root();

        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root: expected_storage_root,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        assert_eq!(root2, hb.root());
    }

    #[test]
    fn t6_5a_load_version_rebinds_published_baseline() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x31);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (version, _) = store.commit().unwrap();
        assert_eq!(version, 1);

        store.load_version().unwrap();
        assert_eq!(store.published_version(), Some(1));
        assert!(store.has_published_store());
    }

    #[test]
    fn t6_5b_reopen_uses_published_baseline() {
        let dir = TempDir::new().unwrap();
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            let addr = Address::repeat_byte(0x32);
            let bundle = make_bundle(vec![(
                addr,
                Some(default_info(1, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(20))],
            )]);
            store.apply_bundle_state(&bundle).unwrap();
            store.commit().unwrap();
            store.close().unwrap();
        }

        let reopened = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(reopened.version(), 1);
        assert_eq!(reopened.published_version(), Some(1));
        assert!(reopened.has_published_store());
    }

    #[test]
    fn t6_5c_rollback_rebinds_published_pointer() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x33);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(1))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();
        store.load_version().unwrap();
        assert_eq!(store.published_version(), Some(1));

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(2))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();
        store.load_version().unwrap();
        assert_eq!(store.published_version(), Some(2));

        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        assert_eq!(store.published_version(), Some(1));
        assert!(store.has_published_store());
    }

    #[test]
    fn t6_5c2_load_version_target_rebinds_historical_published_generation() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x3a);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(1))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();
        store.load_version().unwrap();
        assert_eq!(store.published_version(), Some(1));

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(2))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();
        store.load_version().unwrap();
        assert_eq!(store.published_version(), Some(2));

        store.load_version_target(1).unwrap();
        assert_eq!(store.version(), 1);
        assert_eq!(store.published_version(), Some(1));
        assert!(store.has_published_store());
    }

    #[test]
    fn t6_5c3_rollback_compacts_future_published_generations() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x3b);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(1))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();
        store.load_version().unwrap();

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(2))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();
        store.load_version().unwrap();
        assert!(store.published_baseline.meta_for_version(2, root2).unwrap().is_some());

        store.rollback(1).unwrap();

        assert_eq!(store.version(), 1);
        assert_eq!(store.published_version(), Some(1));
        assert!(store.has_published_store());
        assert!(store.published_baseline.meta_for_version(1, root1).unwrap().is_some());
        assert!(store.published_baseline.meta_for_version(2, root2).unwrap().is_none());
    }

    #[test]
    fn t6_5c4_open_cleans_tmp_artifacts() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        let tmp_paths = [
            base.join("manifest.tmp"),
            base.join("account_trie_checkpoint.bin.tmp"),
            base.join("changelog").join("meta.tmp"),
            MptCommitStore::fast_storage_root(base).join("published").join("gen-9.delta.tmp"),
            MptCommitStore::fast_storage_root(base).join("meta").join("published.tmp"),
        ];
        for path in &tmp_paths {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, b"tmp").unwrap();
        }

        let tmp_dir = MptCommitStore::fast_storage_root(base).join("published").join("stale.tmp");
        fs::create_dir_all(&tmp_dir).unwrap();
        fs::write(tmp_dir.join("leftover"), b"tmp").unwrap();

        let store = MptCommitStore::open(base, false).unwrap();
        drop(store);

        for path in &tmp_paths {
            assert!(!path.exists(), "tmp artifact should be removed: {}", path.display());
        }
        assert!(!tmp_dir.exists(), "tmp directory should be removed");
    }

    #[test]
    fn t6_5c5_open_at_version_loads_historical_state() {
        let dir = TempDir::new().unwrap();
        let addr = Address::repeat_byte(0x3c);

        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            let bundle1 = make_bundle(vec![(
                addr,
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(1))],
            )]);
            store.apply_bundle_state(&bundle1).unwrap();
            store.commit().unwrap();

            let bundle2 = make_bundle(vec![(
                addr,
                Some(default_info(2, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(2))],
            )]);
            store.apply_bundle_state(&bundle2).unwrap();
            store.commit().unwrap();
            store.close().unwrap();
        }

        let store = MptCommitStore::open_at_version(dir.path(), false, 1, false).unwrap();
        assert_eq!(store.version(), 1);
        assert_eq!(store.manifest.latest_version, 2);
        assert_eq!(store.published_version(), Some(1));
        assert!(store.has_published_store());
    }

    #[test]
    fn t6_5c6_open_at_version_overwrite_truncates_future_history() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_commit = true;
        let addr = Address::repeat_byte(0x3d);

        {
            let mut store =
                MptCommitStore::open_with_config(dir.path(), false, config.clone()).unwrap();
            let bundle1 = make_bundle(vec![(
                addr,
                Some(default_info(1, 100)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(1))],
            )]);
            store.apply_bundle_state(&bundle1).unwrap();
            store.commit().unwrap();
            store.flush_persist().unwrap();

            let bundle2 = make_bundle(vec![(
                addr,
                Some(default_info(2, 200)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(2))],
            )]);
            store.apply_bundle_state(&bundle2).unwrap();
            store.commit().unwrap();
            store.flush_persist().unwrap();
            store.close().unwrap();
        }

        let store =
            MptCommitStore::open_with_config_at_version(dir.path(), false, config.clone(), 1, true)
                .unwrap();
        assert_eq!(store.version(), 1);
        assert_eq!(store.manifest.latest_version, 1);
        // After truncation to version 1, published segments from the
        // incremental publish are preserved — published_version is 1.
        assert_eq!(store.published_version(), Some(1));
        {
            let wal = store.wal_store.as_ref().unwrap().lock();
            assert_eq!(wal.latest_version(), 1);
        }
        drop(store);

        let reopened = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();
        assert_eq!(reopened.version(), 1);
        assert_eq!(reopened.manifest.latest_version, 1);
    }

    #[test]
    fn t6_5d_prefers_current_published_view_for_current_version() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x34);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(1))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Rebind to the current published view and clear the cross-block L2 cache so the
        // next block must reload from the current published generation.
        store.load_version().unwrap();
        assert_eq!(store.published_version(), Some(1));
        assert!(store.has_published_store());

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(2))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let ((_version, _root), profile) = store.commit_with_profile().unwrap();

        assert_eq!(profile.apply_l2_hits, 0, "profile: {profile:?}");
        assert_eq!(profile.apply_l3_latest_hits, 0, "profile: {profile:?}");
        assert_eq!(profile.apply_l3_published_hits, 1, "profile: {profile:?}");
        assert_eq!(profile.apply_l3_published_post_flush_hits, 0, "profile: {profile:?}");
        assert_eq!(profile.apply_node_fallback_loads, 0, "profile: {profile:?}");
    }

    #[test]
    fn t6_5e_publish_failure_is_nonfatal() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_async_fail_mode(3);

        let addr = Address::repeat_byte(0x34);
        let bundle = make_bundle(vec![(
            addr,
            Some(default_info(1, 123)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(3))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        // Published failure is non-fatal: flush + load_version succeed.
        store.flush_persist().unwrap();
        store.load_version().unwrap();
    }

    #[test]
    fn t6_5f_deferred_root_rebuilds_current_published_view() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x35);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(1))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();
        store.load_version().unwrap();

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(2))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let ((_version2, _root2), profile2) = store.commit_with_profile().unwrap();
        assert_eq!(profile2.apply_l2_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_l3_latest_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_l3_published_hits, 1, "profile2: {profile2:?}");

        // Clear the L2 cache and require the next block to reload from the current published
        // generation produced by the deferred-root rebuild path for version 2.
        store.load_version().unwrap();

        let bundle3 = make_bundle(vec![(
            addr,
            Some(default_info(3, 300)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(3))],
        )]);
        store.apply_bundle_state(&bundle3).unwrap();
        let ((_version3, _root3), profile3) = store.commit_with_profile().unwrap();

        assert_eq!(profile3.apply_l2_hits, 0, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_node_fallback_loads, 0, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_l3_latest_hits, 0, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_l3_published_hits, 1, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_l3_published_post_flush_hits, 0, "profile3: {profile3:?}");
    }

    #[test]
    fn t6_5g_overlay_commit_also_rebuilds_current_published_view() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x36);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(1))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2 should come from the L2 cache and commit through the overlay path.
        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(2))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let ((_version2, _root2), profile2) = store.commit_with_profile().unwrap();
        assert_eq!(profile2.apply_l2_hits, 1, "profile2: {profile2:?}");

        // After clearing L2 via load_version, the next block must be able to reload from
        // the current published generation rebuilt from the deferred root of block 2.
        store.load_version().unwrap();

        let bundle3 = make_bundle(vec![(
            addr,
            Some(default_info(3, 300)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(3), U256::ZERO, U256::from(3))],
        )]);
        store.apply_bundle_state(&bundle3).unwrap();
        let ((_version3, _root3), profile3) = store.commit_with_profile().unwrap();

        assert_eq!(profile3.apply_l2_hits, 0, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_node_fallback_loads, 0, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_l3_latest_hits, 0, "profile3: {profile3:?}");
        assert_eq!(profile3.apply_l3_published_hits, 1, "profile3: {profile3:?}");
    }

    #[test]
    fn t6_5h_refreshes_published_view_after_flush_before_falling_back() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x37);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(11))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();

        store.clear_storage_trie_state();
        store.published_meta = None;
        store.published_store = None;
        store.published_version.store(0, Ordering::Release);

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(22))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let ((_version2, _root2), profile2) = store.commit_with_profile().unwrap();

        assert_eq!(profile2.apply_l2_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_l3_published_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_l3_published_post_flush_hits, 1, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_node_fallback_loads, 0, "profile2: {profile2:?}");
    }

    #[test]
    fn t6_5i_falls_back_to_persisted_root_when_no_published_view_exists() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x38);

        let bundle1 = make_bundle(vec![(
            addr,
            Some(default_info(1, 100)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(13))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();

        store.clear_storage_trie_state();
        store.published_baseline.clear_meta().unwrap();
        store.published_meta = None;
        store.published_store = None;
        store.published_version.store(0, Ordering::Release);

        let bundle2 = make_bundle(vec![(
            addr,
            Some(default_info(2, 200)),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(2), U256::ZERO, U256::from(26))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let ((_version2, _root2), profile2) = store.commit_with_profile().unwrap();

        assert_eq!(profile2.apply_l2_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_l3_published_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_l3_published_post_flush_hits, 0, "profile2: {profile2:?}");
        assert_eq!(profile2.apply_node_fallback_loads, 1, "profile2: {profile2:?}");
        assert_eq!(store.published_version(), None);
        assert!(!store.has_published_store());
    }

    /// load_version and rollback clear all resident storage trie state.
    #[test]
    fn storage_trie_cache_cleared_on_load_version_and_rollback() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0xCC);
        let info = default_info(1, 1000);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(42))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        assert!(store.storage_trie_cache_contains(&hashed_addr));

        // load_version clears cache
        store.load_version().unwrap();
        assert!(store.storage_trie_cache_is_empty());

        // Re-populate cache
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
        assert!(store.storage_trie_cache_contains(&hashed_addr));

        // rollback clears cache
        store.rollback(1).unwrap();
        assert!(store.storage_trie_cache_is_empty());
    }

    /// Cache correctness across 3 blocks: account touched in block 1 and 3 (not 2)
    /// should still produce correct roots.
    #[test]
    fn storage_trie_cache_correctness_across_blocks() {
        let dir_cached = TempDir::new().unwrap();
        let dir_nocache = TempDir::new().unwrap();

        let addr = Address::repeat_byte(0xDD);
        let info = default_info(1, 1000);
        let slot = U256::from(1);

        // Block 1: set slot=10
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(10))],
        )]);
        // Block 2: only change balance (no storage)
        let info2 = default_info(2, 2000);
        let bundle2 =
            make_bundle(vec![(addr, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        // Block 3: set slot=30
        let info3 = default_info(3, 3000);
        let bundle3 = make_bundle(vec![(
            addr,
            Some(info3),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(10), U256::from(30))],
        )]);

        // Run with cache (normal)
        let mut store_c = MptCommitStore::open(dir_cached.path(), false).unwrap();
        store_c.apply_bundle_state(&bundle1).unwrap();
        let (_, r1c) = store_c.commit().unwrap();
        store_c.apply_bundle_state(&bundle2).unwrap();
        let (_, r2c) = store_c.commit().unwrap();
        store_c.apply_bundle_state(&bundle3).unwrap();
        let (_, r3c) = store_c.commit().unwrap();

        // Run without resident storage trie state between blocks.
        let mut store_n = MptCommitStore::open(dir_nocache.path(), false).unwrap();
        store_n.apply_bundle_state(&bundle1).unwrap();
        let (_, r1n) = store_n.commit().unwrap();
        store_n.clear_storage_trie_state();
        store_n.apply_bundle_state(&bundle2).unwrap();
        let (_, r2n) = store_n.commit().unwrap();
        store_n.clear_storage_trie_state();
        store_n.apply_bundle_state(&bundle3).unwrap();
        let (_, r3n) = store_n.commit().unwrap();

        assert_eq!(r1c, r1n, "block 1 roots must match");
        assert_eq!(r2c, r2n, "block 2 roots must match");
        assert_eq!(r3c, r3n, "block 3 roots must match");
    }

    /// T3.10: read_only / poisoned / rollback semantics unchanged by parallel path.
    #[test]
    fn t3_10_parallel_semantics_unchanged() {
        let dir = TempDir::new().unwrap();

        // read_only still rejects writes
        {
            let mut store = MptCommitStore::open(dir.path(), true).unwrap();
            assert!(store.apply_bundle_state(&BundleState::default()).is_err());
        }

        // poisoned still blocks operations
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.set_fail_point(Some(CommitFailPoint::BeforePersist));
            store.apply_bundle_state(&BundleState::default()).unwrap();
            assert!(store.commit().is_err());
            assert!(store.poisoned);
            assert!(store.commit().is_err());
            assert!(store.apply_bundle_state(&BundleState::default()).is_err());

            // load_version recovers
            store.set_fail_point(None);
            store.load_version().unwrap();
            assert!(!store.poisoned);
            store.close().unwrap();
        }

        // rollback works after parallel commits
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            let bundle = make_storage_heavy_bundle(5, 2);
            store.apply_bundle_state(&bundle).unwrap();
            store.commit().unwrap();
            store.apply_bundle_state(&bundle).unwrap();
            store.commit().unwrap();
            assert_eq!(store.version(), 2);
            store.rollback(1).unwrap();
            assert_eq!(store.version(), 1);
            store.close().unwrap();
        }
    }

    #[test]
    fn t_async_flush_reports_background_persist_failure() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_async_fail_mode(1);

        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        let err = store.flush_persist().unwrap_err();
        assert!(err.to_string().contains("forced async persist failure"));
        assert!(store.apply_bundle_state(&BundleState::default()).is_err());
        assert!(store.close().is_err());
    }

    #[test]
    fn t_async_flush_is_true_barrier() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();

        let manifest = VersionManifest::load(&dir.path().join("manifest.json")).unwrap();
        assert_eq!(manifest.latest_version, 1);
        store.close().unwrap();
    }
}
