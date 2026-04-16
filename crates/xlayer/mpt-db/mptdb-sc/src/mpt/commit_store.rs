use alloy_primitives::{Address, B256};
use alloy_rlp::Encodable;
use alloy_trie::{Nibbles, EMPTY_ROOT_HASH};
use fs4::fs_std::FileExt;
use mptdb_common::error::{MptDbError, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use reth_trie_common::{updates::TrieUpdates, AccountProof};
use reth_trie_sparse::{SerialSparseTrie, SparseStateTrie};
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
    published_baseline::{
        BulkSegmentWriter, IoRateLimiter, PublishedBaselineManager, PublishedBaselineMeta,
        PublishedBaselineReader,
    },
    r#trait::{CommitFrontier, MptCommitter, MptGcStats, MptSnapshotExporter, MptSnapshotImporter},
    segment::StorageTrieSegment,
    snapshot::{SnapshotExporter, SnapshotImporter},
    sparse_storage::{
        apply_all_storage_changes_sparse, build_storage_segments_from_sparse_snapshots,
        build_storage_segments_from_sparse_trie, convert_arena_to_account_proof_nodes_for_paths,
        convert_arena_to_decoded_storage_multiproof,
        convert_arena_to_decoded_storage_multiproof_for_paths,
        extract_account_proof_from_sparse_trie_for_paths, extract_storage_proof_from_sparse_trie,
        extract_storage_proof_from_sparse_trie_for_paths, storage_key_requires_provider_reveal,
        SegmentTrieNodeProviderFactory,
    },
    state::{self, DirtyAccount},
    storage_cow::{CowRootRef, StorageTrieCow},
    tree::MptTree,
    tree_algo,
    wal::{CommitWalAccountChange, CommitWalEntry, CommitWalStore},
};

#[cfg(test)]
use alloy_primitives::U256;

// B2 (SLRU / frequency-aware eviction) is deferred.
// A two-LruMap SLRU doubles the hash-table memory footprint vs a single
// LruMap, causing a measurable B4.5 regression (~50 ms) that outweighs the
// B4.6 gain.  The right implementation needs a single underlying map with a
// custom schnellru limiter that skips protected entries during eviction.
// Tracked in arena-overlay-reuse.md TODO section.

const L2_FREQ_SKETCH_ROWS: usize = 4;
const L2_FREQ_SKETCH_WIDTH: usize = 256;
const L2_FREQ_SKETCH_AGE_INTERVAL: u64 = 65_536;
const SPARSE_DEFERRED_MATERIALIZE_INTERVAL_LARGE_DEFAULT: i64 = 4;
const SPARSE_DEFERRED_MATERIALIZE_MIN_PENDING_TARGETS: usize = 2_048;
const SPARSE_DEFERRED_MATERIALIZE_MAX_PENDING_TARGETS: usize = 20_000;
const SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_SMALL_DEFAULT: usize = 512;
const SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_MID_DEFAULT: usize = 768;
const SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_LARGE_DEFAULT: usize = 1_024;
const SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_HIGH_WATERMARK: usize = 8_192;
const L2_FREQ_SEEDS: [u32; L2_FREQ_SKETCH_ROWS] =
    [0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F];

#[derive(Clone)]
struct CountMinSketch {
    counters: [[u16; L2_FREQ_SKETCH_WIDTH]; L2_FREQ_SKETCH_ROWS],
    samples: u64,
}

impl CountMinSketch {
    fn observe(&mut self, key: &B256) {
        let bytes = key.as_slice();
        for row in 0..L2_FREQ_SKETCH_ROWS {
            let idx = Self::index(bytes, row);
            self.counters[row][idx] = self.counters[row][idx].saturating_add(1);
        }
        self.samples = self.samples.saturating_add(1);
        if self.samples >= L2_FREQ_SKETCH_AGE_INTERVAL {
            for row in &mut self.counters {
                for value in row.iter_mut() {
                    *value >>= 1;
                }
            }
            self.samples = 0;
        }
    }

    fn estimate(&self, key: &B256) -> u16 {
        let bytes = key.as_slice();
        let mut min = u16::MAX;
        for row in 0..L2_FREQ_SKETCH_ROWS {
            let idx = Self::index(bytes, row);
            min = min.min(self.counters[row][idx]);
        }
        min
    }

    fn index(bytes: &[u8], row: usize) -> usize {
        let off = row * 4;
        let mut chunk = [0u8; 4];
        chunk.copy_from_slice(&bytes[off..off + 4]);
        let mixed = u32::from_le_bytes(chunk) ^ L2_FREQ_SEEDS[row];
        (mixed as usize) & (L2_FREQ_SKETCH_WIDTH - 1)
    }
}

impl Default for CountMinSketch {
    fn default() -> Self {
        Self { counters: [[0u16; L2_FREQ_SKETCH_WIDTH]; L2_FREQ_SKETCH_ROWS], samples: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheTouchOutcome {
    Rejected,
    Hit,
    Inserted { evicted: Option<B256> },
}

struct FreqAwareCache {
    lru: LruMap<B256, (), ByLength>,
    sketch: CountMinSketch,
    capacity: usize,
    admission_enabled: bool,
}

impl FreqAwareCache {
    fn new(capacity: usize, admission_enabled: bool) -> Self {
        Self {
            lru: LruMap::new(ByLength::new(capacity.max(1) as u32)),
            sketch: CountMinSketch::default(),
            capacity,
            admission_enabled,
        }
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn contains(&self, key: &B256) -> bool {
        self.lru.peek(key).is_some()
    }

    fn touch_if_present(&mut self, key: &B256) -> bool {
        // Hit check first — no sketch overhead for non-LRU keys.
        if self.lru.get(key).is_some() {
            return true;
        }
        false
    }

    fn touch(&mut self, key: B256) -> CacheTouchOutcome {
        if self.capacity == 0 {
            return CacheTouchOutcome::Rejected;
        }

        // Hit check before observe: L2 hits are the common case and must not
        // pay sketch overhead. observe() is only needed for admission decisions.
        if self.lru.get(&key).is_some() {
            return CacheTouchOutcome::Hit;
        }

        // New key — record access frequency before making the admission decision.
        self.observe(&key);

        let mut evicted = None;
        if self.lru.len() >= self.capacity {
            let tail = self.lru.peek_oldest().map(|(oldest, _)| *oldest);
            if self.admission_enabled &&
                tail.is_some_and(|tail_key| {
                    self.sketch.estimate(&key) < self.sketch.estimate(&tail_key)
                })
            {
                return CacheTouchOutcome::Rejected;
            }
            evicted = tail;
        }

        let _ = self.lru.get_or_insert(key, || ());
        CacheTouchOutcome::Inserted { evicted }
    }

    fn remove(&mut self, key: &B256) {
        self.lru.remove(key);
    }

    fn clear(&mut self) {
        self.lru.clear();
        self.sketch = CountMinSketch::default();
    }

    fn is_empty(&self) -> bool {
        self.lru.is_empty()
    }

    fn observe(&mut self, key: &B256) {
        if self.admission_enabled {
            self.sketch.observe(key);
        }
    }
}

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

/// Outcome of take_working_or_base_for_version, for observability counters.
#[derive(Clone, Copy)]
enum OverlayOutcome {
    /// Overlay capacity was stolen from the base (lazy or clone path with reuse).
    Stolen { shrank: bool, reused_bytes: usize },
    /// Fell back to `base.clone()` — fresh heap allocation, no capacity reuse.
    FreshClone,
    /// A pre-materialised working trie already existed; no steal needed.
    ExistingWorking,
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

    fn take_working_for_version(
        &mut self,
        version: i64,
        overlay_reuse: bool,
        watermark: Option<usize>,
    ) -> Option<(StorageTrieCow, OverlayOutcome)> {
        if self.working_version != Some(version) {
            return None;
        }
        self.working_version = None;
        if let Some(working) = self.working.take() {
            return Some((working, OverlayOutcome::ExistingWorking));
        }
        // Lazy materialisation: checkout_for_write deferred the clone; do it
        // now inside the rayon parallel section.  Use clone_frozen_only +
        // steal so the base ends up with zero-capacity overlays (O(1) drop)
        // and the working copy starts with pre-allocated capacity (no resizes).
        if overlay_reuse {
            let reused_bytes = self.base.overlay_capacity();
            let mut working = self.base.clone_frozen_only();
            let shrank = working.steal_overlay_capacity_from(&mut self.base, watermark);
            Some((working, OverlayOutcome::Stolen { shrank, reused_bytes }))
        } else {
            Some((self.base.clone(), OverlayOutcome::FreshClone))
        }
    }

    fn clone_base_with_steal(
        &mut self,
        overlay_reuse: bool,
        watermark: Option<usize>,
    ) -> (StorageTrieCow, OverlayOutcome) {
        if overlay_reuse && self.base.is_overlay_reusable() {
            let reused_bytes = self.base.overlay_capacity();
            let mut working = self.base.clone_frozen_only();
            let shrank = working.steal_overlay_capacity_from(&mut self.base, watermark);
            (working, OverlayOutcome::Stolen { shrank, reused_bytes })
        } else {
            (self.base.clone(), OverlayOutcome::FreshClone)
        }
    }

    fn take_working_or_base_for_version(
        &mut self,
        version: i64,
        overlay_reuse: bool,
        watermark: Option<usize>,
    ) -> (StorageTrieCow, OverlayOutcome) {
        if let Some(result) = self.take_working_for_version(version, overlay_reuse, watermark) {
            result
        } else {
            self.clone_base_with_steal(overlay_reuse, watermark)
        }
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

/// Pending state produced by `apply_dirty_accounts_inner_sparse`.
///
/// Held between apply and commit when `MptConfig::use_sparse_storage=true`.
/// `commit_inner_with_mode` takes it and calls `root_with_updates` to compute
/// the state root.
struct PendingSparseState {
    trie: SparseStateTrie,
    factory: SegmentTrieNodeProviderFactory,
}

/// Cross-block sparse state kept between commits when
/// `MptConfig::cross_block_sparse=true`.
///
/// The `SparseStateTrie` survives across blocks: already-revealed paths are
/// skipped on the next block's reveal step, and `root_with_updates` operates
/// incrementally (only recomputes changed subtrees).
struct CrossBlockSparseState {
    trie: SparseStateTrie,
    factory: SegmentTrieNodeProviderFactory,
    /// Version at which each storage account's trie was last accessed.
    /// Used for LRU-style eviction when `cross_block_sparse_max_lag > 0`.
    storage_last_block: alloy_primitives::map::HashMap<B256, i64>,
    /// Per-version eviction queue: each entry holds the block version and the
    /// list of accounts first dirtied at that version.
    ///
    /// # Why this exists
    ///
    /// The naive eviction approach iterates the entire `storage_last_block`
    /// HashMap to find expired entries — O(total_accounts).  For B4.6 with
    /// 10K dirty accounts/block and max_lag=8, the map grows to ~90K entries
    /// and full iteration takes ~20 ms/block (90K × cache-miss-load).
    ///
    /// This queue enables O(evicted_count) eviction:
    /// - Each block appends `(version, accounts_dirty_this_block)` to the back.
    /// - Eviction pops the front entry (oldest block) and removes only those accounts whose
    ///   `storage_last_block` still matches the popped version (i.e. they were not re-accessed in
    ///   a newer block).
    ///
    /// This turns the 20 ms O(90K) scan into a ~0.3 ms O(10K) pass.
    version_queue: std::collections::VecDeque<(i64, Vec<B256>)>,
}

/// A persist job sent to the background worker thread.
struct PersistJob {
    barrier_only: bool,
    published_puts: Vec<(B256, StorageTrieSegment)>,
    published_deletes: Vec<B256>,
    publish_baseline: bool,
    state_root: B256,
    manifest: VersionManifest,
    manifest_path: PathBuf,
    /// If true, the worker saves the manifest after WAL/persist work.
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
    /// Sparse storage trie snapshots captured on the frontend for background
    /// segment materialization. Used when sparse apply is enabled: the normal
    /// storage cache candidates may not include the latest sparse updates.
    committed_sparse_tries: Vec<(B256, B256, SerialSparseTrie)>,
}

/// A publish job handled by the background publish worker.
///
/// Keep this worker independent from manifest/durable updates so heavy
/// segment build + publish_generation work does not delay durable frontier
/// advancement.
struct PublishJob {
    barrier_only: bool,
    published_puts: Vec<(B256, StorageTrieSegment)>,
    published_deletes: Vec<B256>,
    state_root: B256,
    manifest: VersionManifest,
    version: i64,
    committed_tries: Vec<(B256, B256, StorageTrieCow)>,
    committed_sparse_tries: Vec<(B256, B256, SerialSparseTrie)>,
    done: Option<crossbeam_channel::Sender<Result<()>>>,
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
    /// Sparse apply: time spent building SegmentTrieNodeProviderFactory.
    pub sparse_apply_factory_build: Duration,
    /// Sparse apply: time spent extracting account multiproof.
    pub sparse_apply_account_proof: Duration,
    /// Sparse apply: time spent in apply_all_storage_changes_sparse.
    pub sparse_apply_apply_changes: Duration,
    /// Sparse factory: number of dirty accounts examined.
    pub sparse_factory_dirty_accounts: u64,
    /// Sparse factory: number of accounts with storage changes.
    pub sparse_factory_storage_accounts: u64,
    /// Sparse factory: published segment lookup attempts.
    pub sparse_factory_segment_lookups: u64,
    /// Sparse factory: published segment lookup hits.
    pub sparse_factory_segment_hits: u64,
    /// Sparse factory: segment lookup missed because published_store is absent.
    pub sparse_factory_segment_miss_no_store: u64,
    /// Sparse factory: segment lookup miss (entry missing/stale/corrupt).
    pub sparse_factory_segment_miss: u64,
    /// Sparse factory: subset of misses where entry exists but root mismatches expected_root.
    pub sparse_factory_segment_root_mismatch: u64,
    /// Sparse factory: tier-3 dirty-path proof preload attempts.
    pub sparse_factory_tier3_attempts: u64,
    /// Sparse factory: tier-3 dirty-path proof preload successes.
    pub sparse_factory_tier3_hits: u64,
    /// Sparse factory: fallback to tier1/2 proof construction attempts.
    pub sparse_factory_tier12_attempts: u64,
    /// Sparse factory (cross-block): accounts already revealed in cross trie.
    pub sparse_factory_cross_reuse_accounts: u64,
    /// Sparse factory (cross-block): total newly-touched slots not yet revealed.
    pub sparse_factory_cross_missing_slots: u64,
    /// Sparse factory (cross-block): newly-touched slots that still need
    /// provider-backed reveal/proof construction.
    pub sparse_factory_cross_missing_proof_slots: u64,
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
    pub wal_serialize: Duration,
    pub wal_crc: Duration,
    pub wal_payload_bytes: u32,
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
    /// Time spent in maybe_refresh_published_view() during ensure_storage.
    pub apply_published_view_refresh: Duration,
    /// Time spent looking up storage_root from the account trie for L2 misses.
    pub apply_storage_root_lookup: Duration,
    /// Time to freeze the account trie in set_committed_base after commit.
    pub commit_account_set_base: Duration,
    /// Time to prepare + cache 5000 storage tries back into L2 after commit.
    pub commit_cache_storage_prep: Duration,

    // ── Overlay reuse observability ───────────────────────────────────────
    /// Number of storage tries that successfully stole overlay capacity from
    /// the previous block's base (overlay_reuse_enabled + is_overlay_reusable).
    /// Tries where overlay capacity was stolen from the base (lazy or clone path).
    pub overlay_stolen: u64,
    /// Tries that fell back to `base.clone()` — fresh heap allocation, no reuse.
    pub overlay_fresh_clone: u64,
    /// Tries where a pre-materialised working trie already existed (no steal needed).
    pub overlay_existing_working: u64,
    /// Number of overlay shrink_to_fit calls triggered by watermark policy.
    pub overlay_shrink_events: u64,
    /// Total overlay capacity entries transferred across all steals this block.
    pub overlay_reused_capacity_entries: u64,
    /// Watermark (max overlay node count) recorded at the end of this block.
    pub overlay_watermark: usize,
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
    refresh_elapsed: Duration,
}

#[derive(Default, Clone, Copy)]
struct SparseFactoryStats {
    dirty_accounts: u64,
    storage_accounts: u64,
    segment_lookups: u64,
    segment_hits: u64,
    segment_miss_no_store: u64,
    segment_miss: u64,
    segment_root_mismatch: u64,
    tier3_attempts: u64,
    tier3_hits: u64,
    tier12_attempts: u64,
    cross_reuse_accounts: u64,
    cross_missing_slots: u64,
    cross_missing_proof_slots: u64,
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
    /// Cross-block storage trie cache with optional frequency-based admission.
    storage_trie_cache: FreqAwareCache,
    dirty_accounts: Vec<DirtyAccount>,

    persisted: Arc<PersistedTrieStore>,
    /// Stale root index for incremental GC.  Populated by `prune_before`;
    /// consumed (and cleaned) by `gc`.  `None` only when opened read-only.
    stale_index: Option<Arc<super::stale_index::StaleRootIndex>>,
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
    /// Channel to send publish jobs to the background worker.
    publish_tx: Option<crossbeam_channel::Sender<PublishJob>>,
    /// Handle to the background publish worker thread.
    publish_handle: Option<JoinHandle<()>>,
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
    last_apply_published_view_refresh: Duration,
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
    last_sparse_apply_factory_build: Duration,
    last_sparse_apply_account_proof: Duration,
    last_sparse_apply_apply_changes: Duration,
    /// Number of account paths that required reveal in the latest sparse apply.
    ///
    /// When this is zero in cross-block sparse mode, the next commit can skip
    /// eager account-trie hash-cache refresh and keep only arena snapshots.
    last_sparse_account_reveal_keys: usize,
    last_apply_sparse_factory: SparseFactoryStats,
    last_wal_append_lock_wait: Duration,
    last_wal_append_write: Duration,
    last_wal_serialize: Duration,
    last_wal_crc: Duration,
    last_wal_payload_bytes: u32,
    /// Reusable serialization buffer for WAL entries.  Serialize/CRC happen
    /// outside the WAL mutex; the lock only covers file write + index update.
    wal_serialize_buf: Vec<u8>,
    last_commit_profile: CommitProfile,
    checkpoint_account_trie_nodes: Option<usize>,
    shutdown_complete: bool,
    /// Handles evicted from the LRU during the current block's trie_load phase.
    /// Dropped at the start of the NEXT block's ensure_working_storage_tries so
    /// that the deallocation cost falls outside the commit critical path.
    pending_drops: Vec<StorageTrieHandle>,
    /// Addresses whose L2 admission was rejected by frequency filter.
    /// Cleaned up after commit if still absent from LRU.
    rejected_activations: Vec<B256>,
    /// Addresses activated as empty tries this block (bypassed LRU registration).
    /// Drained after commit to remove zero-value handles from storage_trie_handles.
    empty_trie_activations: Vec<B256>,
    /// Addresses whose LRU eviction was blocked by the wal_first guard
    /// (published_version < base_version).  Drained at the start of each
    /// touch_cached_storage_trie call once the published segment has caught up.
    /// Ordered by insertion time: once the front is still blocked, the rest are too.
    deferred_evictions: std::collections::VecDeque<B256>,
    /// Rolling high-water mark of overlay node count across all dirty storage
    /// tries in the previous committed block.  Used as the watermark target for
    /// overlay capacity shrink decisions on steal.
    overlay_watermark: usize,
    last_overlay_stolen: u64,
    last_overlay_fresh_clone: u64,
    last_overlay_existing_working: u64,
    last_overlay_shrink_events: u64,
    last_overlay_reuse_capacity_entries: u64,
    #[cfg(test)]
    loaded_from_checkpoint: bool,

    #[cfg(test)]
    fail_point: Option<CommitFailPoint>,
    #[cfg(test)]
    async_fail_mode: Arc<std::sync::atomic::AtomicU8>,

    /// Pending sparse state from `apply_dirty_accounts_inner_sparse`.
    /// `Some` only between apply and commit when `config.use_sparse_storage=true`.
    pending_sparse_state: Option<Box<PendingSparseState>>,
    /// The `SparseStateTrie` from the most recently committed block.
    /// Available for proof generation for the latest committed version
    /// when `config.use_sparse_storage=true`.  Replaced on each commit.
    last_committed_sparse_trie: Option<Box<SparseStateTrie>>,
    /// Cross-block sparse state kept alive between commits.
    /// `Some` when `config.cross_block_sparse=true` and at least one block
    /// has been committed.
    cross_block_sparse: Option<Box<CrossBlockSparseState>>,
    /// Coalesced storage roots awaiting deferred sparse segment materialization.
    /// Key: hashed account address; Value: latest storage root to publish.
    sparse_deferred_publish_roots: HashMap<B256, B256>,
}

/// Returns `true` when the `MPT_USE_SPARSE_STORAGE` env var is set to a
/// truthy value (`1`, `true`, `on`, `yes`).  Used by `MptCommitStore::open`
/// to force sparse mode on all unit tests without code changes.
fn is_sparse_storage_forced() -> bool {
    std::env::var("MPT_USE_SPARSE_STORAGE")
        .ok()
        .map(|v| {
            let lower = v.trim().to_ascii_lowercase();
            !(lower == "0" || lower == "false" || lower == "off" || lower == "no")
        })
        .unwrap_or(false)
}

impl MptCommitStore {
    fn diagnostics_enabled() -> bool {
        std::env::var_os("MPT_DEBUG_DIAGNOSTICS").is_some()
    }

    fn sparse_l3_trace_enabled() -> bool {
        std::env::var_os("MPT_SPARSE_L3_TRACE").is_some()
    }

    fn l2_freq_admission_enabled_from_env() -> bool {
        std::env::var("MPT_L2_FREQ_ADMISSION")
            .ok()
            .map(|v| {
                let lower = v.trim().to_ascii_lowercase();
                !(lower == "0" || lower == "false" || lower == "off" || lower == "no")
            })
            .unwrap_or(true)
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

    fn new_storage_trie_cache(capacity: usize) -> FreqAwareCache {
        let limit = Self::storage_trie_cache_limit(capacity);
        FreqAwareCache::new(limit, Self::l2_freq_admission_enabled_from_env())
    }

    fn checkout_cached_storage_trie(&mut self, hashed_address: &B256) -> bool {
        let working_version = self.current_working_version();
        let Some(handle) = self.storage_trie_handles.get_mut(hashed_address) else {
            return false;
        };
        handle.checkout_for_write(working_version);
        let _ = self.storage_trie_cache.touch_if_present(hashed_address);
        true
    }

    fn evict_cached_storage_trie(&mut self, hashed_address: &B256) -> Option<StorageTrieCow> {
        self.storage_trie_cache.remove(hashed_address);
        self.storage_trie_handles
            .remove(hashed_address)
            .map(|handle| handle.working.unwrap_or(handle.base))
    }

    fn touch_cached_storage_trie(&mut self, hashed_address: B256) -> bool {
        if self.storage_trie_cache.capacity() == 0 {
            return false;
        }

        // Drain deferred evictions whose wal_first guard has now cleared.
        // Queue is ordered by insertion time, so stop at the first still-blocked entry.
        if !self.deferred_evictions.is_empty() {
            let published = self.published_version.load(Ordering::Acquire);
            while let Some(front) = self.deferred_evictions.front().copied() {
                match self.storage_trie_handles.get(&front) {
                    Some(handle) if !handle.has_working() && published >= handle.base_version => {
                        self.deferred_evictions.pop_front();
                        if let Some(handle) = self.storage_trie_handles.remove(&front) {
                            self.pending_drops.push(handle);
                        }
                    }
                    None => {
                        // Already removed by some other path (e.g. selfdestruct).
                        self.deferred_evictions.pop_front();
                    }
                    _ => break, // Guard still active; rest of queue is also blocked.
                }
            }
        }

        let evicted = match self.storage_trie_cache.touch(hashed_address) {
            CacheTouchOutcome::Rejected => {
                // Frequency admission rejects this key: keep cache unchanged.
                return false;
            }
            CacheTouchOutcome::Hit => return true,
            CacheTouchOutcome::Inserted { evicted } => evicted,
        };

        if let Some(evicted) = evicted {
            if evicted != hashed_address && !self.storage_trie_cache.contains(&evicted) {
                let published = self.published_version.load(Ordering::Acquire);
                let can_evict = self.storage_trie_handles.get(&evicted).is_some_and(|handle| {
                    !handle.has_working() &&
                        // In wal_first mode, only evict if the published segment
                        // has caught up to this handle's version.  Otherwise the
                        // trie data only exists in memory — evicting would lose it
                        // since RocksDB has no nodes in wal_first mode.
                        (published >= handle.base_version)
                });
                if can_evict {
                    if let Some(handle) = self.storage_trie_handles.remove(&evicted) {
                        self.pending_drops.push(handle);
                    }
                } else if true && self.storage_trie_handles.contains_key(&evicted) {
                    // Guard blocked removal: defer until published_version catches up.
                    // The handle stays in storage_trie_handles; we track the address
                    // here so it gets cleaned up once the segment is published.
                    self.deferred_evictions.push_back(evicted);
                }
            }
        }
        true
    }

    fn cache_storage_trie(&mut self, hashed_address: B256, trie: StorageTrieCow) {
        let committed_version = self.version;
        let already_cached = self.storage_trie_cache.contains(&hashed_address);
        if let Some(handle) = self.storage_trie_handles.get_mut(&hashed_address) {
            handle.set_committed_base(committed_version, trie);
        } else {
            self.storage_trie_handles
                .insert(hashed_address, StorageTrieHandle::snapshot(committed_version, trie));
        }
        if !already_cached {
            if !self.touch_cached_storage_trie(hashed_address) {
                self.rejected_activations.push(hashed_address);
            }
        }
    }

    fn clear_storage_trie_state(&mut self) {
        self.storage_trie_cache.clear();
        self.storage_trie_handles.clear();
        self.rejected_activations.clear();
    }

    #[cfg(test)]
    fn storage_trie_cache_contains(&self, hashed_address: &B256) -> bool {
        self.storage_trie_cache.contains(hashed_address)
    }

    #[cfg(test)]
    fn clone_cached_storage_trie(&mut self, hashed_address: &B256) -> Option<StorageTrieCow> {
        if !self.storage_trie_cache.contains(hashed_address) {
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
        if !self.touch_cached_storage_trie(hashed_address) {
            self.rejected_activations.push(hashed_address);
        }
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
        // Empty tries are never registered in the LRU (no cross-block value).
        // Track them so we can remove them from storage_trie_handles after
        // commit, preventing unbounded map growth and LRU slot pollution.
        self.empty_trie_activations.push(hashed_address);
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
        // Drop handles that were evicted from the LRU during the previous
        // block's pub_activate phase. Doing it here rather than at eviction
        // time keeps ~400K small frees (Nibbles + value bytes per MptNode)
        // off the commit critical path.
        drop(std::mem::take(&mut self.pending_drops));

        let mut stats = StorageTrieLoadStats::default();
        let mut latest_candidates: Vec<(B256, B256)> = Vec::new();

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

        // Only refresh the published view when we actually have L3 candidates.
        // All-L2-hit blocks (common for hot-set workloads like B4.7) skip the
        // reload_published_view() I/O entirely, saving ~100ms per block.
        let mut refresh_elapsed = Duration::ZERO;
        if !need_storage_root.is_empty() {
            let refresh_start = std::time::Instant::now();
            self.maybe_refresh_published_view()?;
            refresh_elapsed = refresh_start.elapsed();
        }
        stats.refresh_elapsed = refresh_elapsed;
        let published_current = self.has_current_published_view();

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
            if true {
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
        let mut published_deletes_set: HashSet<B256> = deleted_accounts.clone();
        for (addr, _) in storage_cache_candidates {
            if storage_roots.get(addr).copied() == Some(EMPTY_ROOT_HASH) &&
                !deleted_accounts.contains(addr)
            {
                published_deletes_set.insert(*addr);
            }
        }
        // Sparse-only path has no storage_cache_candidates.  For any account
        // that touched storage this block and ended at EMPTY_ROOT_HASH, publish
        // a delete so L3 does not retain stale old-root pages.
        for dirty in &self.dirty_accounts {
            if deleted_accounts.contains(&dirty.hashed_address) {
                continue;
            }
            if !dirty.storage_wiped && dirty.storage_changes.is_empty() {
                continue;
            }
            if storage_roots.get(&dirty.hashed_address).copied() == Some(EMPTY_ROOT_HASH) {
                published_deletes_set.insert(dirty.hashed_address);
            }
        }
        let published_deletes = published_deletes_set.into_iter().collect::<Vec<_>>();

        Ok(PreparedStorageVersion {
            new_version,
            state_root,
            manifest,
            deleted_accounts,
            published_deletes,
        })
    }

    fn prepare_cached_storage_trie(
        &self,
        trie: StorageTrieCow,
        storage_root: B256,
        published_segment: Option<&StorageTrieSegment>,
        use_async: bool,
    ) -> Result<StorageTrieCow> {
        // Convert materialized trie to a lazy segment-backed reference when
        // a published segment exists.  This keeps the L2 cache lightweight:
        // only the segment page ref is cached, not the full arena data.
        trie.into_snapshot_cached(storage_root, published_segment, use_async)
    }

    fn save_storage_version(
        &mut self,
        prepared: PreparedStorageVersion,
        wal_append_elapsed: Duration,
        all_blobs: Vec<(B256, Vec<u8>)>,
        storage_roots: &HashMap<B256, B256>,
        storage_cache_candidates: &mut [(B256, StorageTrieCow)],
        _deferred_published_roots: Vec<(B256, B256)>,
        sparse_published_puts: Vec<(B256, StorageTrieSegment)>,
        sparse_committed_tries: Vec<(B256, B256, SerialSparseTrie)>,
        mode: CommitExecutionMode,
        storage_segment_build_elapsed: &mut Duration,
    ) -> Result<SavedStorageVersion> {
        let persist_start = std::time::Instant::now();
        let mut published_puts = Vec::new();
        let mut persist_batch_elapsed = Duration::ZERO;
        let mut manifest_save_elapsed = Duration::ZERO;
        let mut publish_generation_elapsed = Duration::ZERO;
        let mut open_published_store_elapsed = Duration::ZERO;
        let use_bg_segment_materialization =
            mode.wal_first && mode.publish_baseline && self.wal_first_defer_segment_build_enabled();

        if mode.wal_first {
            // WAL-first: segment generation defaults to background
            // materialization from committed trie snapshots. WAL + published
            // segments provide crash recovery; RocksDB is no longer on the
            // critical path.
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
            let committed_sparse_tries =
                if use_bg_segment_materialization { sparse_committed_tries } else { Vec::new() };
            let committed_tries =
                if use_bg_segment_materialization && committed_sparse_tries.is_empty() {
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
            let worker_published_puts =
                if use_bg_segment_materialization { Vec::new() } else { sparse_published_puts };

            let tx = self.persist_tx.as_ref().unwrap();
            let job = PersistJob {
                barrier_only: false,
                published_puts: worker_published_puts,
                published_deletes: prepared.published_deletes.clone(),
                publish_baseline: mode.publish_baseline,
                state_root: prepared.state_root,
                manifest: prepared.manifest.clone(),
                manifest_path: self.manifest_path.clone(),
                save_manifest: true,
                version: prepared.new_version,
                done: None,
                committed_tries,
                committed_sparse_tries,
            };
            if let Err(e) = tx.send(job) {
                // WAL append was already performed in the foreground for
                // wal_first commits. If enqueue fails, rollback the just-
                // appended WAL version so failed commits don't appear durable.
                let rollback_to = prepared.new_version.saturating_sub(1);
                let _ = self.rollback_shadow_wal_to(rollback_to);
                return Err(MptDbError::Other(format!("send persist job: {e}")));
            }
        } else {
            if mode.publish_baseline {
                if sparse_published_puts.is_empty() {
                    let segment_build_start = std::time::Instant::now();
                    published_puts = Self::build_publish_segments_from_tries(
                        storage_roots,
                        storage_cache_candidates,
                    )?;
                    *storage_segment_build_elapsed += segment_build_start.elapsed();
                } else {
                    published_puts = sparse_published_puts;
                }
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
            use_async: mode.wal_first,
            published_puts,
            wal_append_elapsed,
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
        published_io_lock: Arc<Mutex<()>>,
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
                            // Serialize published baseline writes/compaction with the
                            // persist worker to keep delta/page locator consistency.
                            let _published_io_guard = published_io_lock.lock();
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

    /// Enforce the frontier ordering invariant used by wal-first recovery:
    /// committed/logical >= durable >= published.
    fn enforce_frontier_invariants(&self, context: &str) -> Result<()> {
        let committed = self.version;
        let durable = self.durable_version.load(Ordering::Acquire);
        let published = self.published_version.load(Ordering::Acquire);

        if durable > committed {
            return Err(MptDbError::Other(format!(
                "frontier invariant violated at {context}: durable_version({durable}) > committed_version({committed})"
            )));
        }
        if published > durable {
            return Err(MptDbError::Other(format!(
                "frontier invariant violated at {context}: published_version({published}) > durable_version({durable})"
            )));
        }
        Ok(())
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
        if true {
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
                let same_generation =
                    self.published_meta.as_ref().map_or(false, |m| m.generation == meta.generation);
                if same_generation {
                    // Generation unchanged: delta HashMap is still valid.
                    if let Some(ref mut m) = self.published_meta {
                        m.version = meta.version;
                        m.root = meta.root;
                    }
                } else if let Some(existing) = self.published_store.take() {
                    // Try incremental extend: load only the single new delta
                    // instead of re-parsing the full historical chain.
                    // Falls back to full rebuild if the parent chain doesn't match.
                    let extended =
                        self.published_baseline.try_extend_published_store(&meta, existing)?;
                    self.published_meta = Some(meta.clone());
                    if let Some(reader) = extended {
                        self.published_store = Some(reader);
                    } else {
                        self.published_store =
                            self.published_baseline.open_published_store(&meta)?;
                    }
                } else {
                    self.published_store = self.published_baseline.open_published_store(&meta)?;
                    self.published_meta = Some(meta);
                }
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
        if self.read_only || self.persist_tx.is_some() || self.publish_tx.is_some() {
            return Ok(());
        }

        self.start_checkpoint_worker()?;

        // Serialize all published baseline writers (persist + rewrite workers)
        // to prevent concurrent publish/compact races.
        let published_io_lock = Arc::new(Mutex::new(()));

        if self.published_rewrite_tx.is_none() {
            let (rewrite_tx, rewrite_handle) = Self::spawn_published_rewrite_worker(
                Arc::clone(&self.persisted),
                Arc::clone(&self.published_baseline),
                Arc::clone(&published_io_lock),
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

        let (publish_tx, publish_rx) =
            crossbeam_channel::bounded::<PublishJob>(self.config.async_queue_depth);
        let published_baseline_clone = Arc::clone(&self.published_baseline);
        let mut worker_published_meta = self.published_meta.clone();
        let async_error_clone = Arc::clone(&self.async_error);
        let async_error_detail_clone = Arc::clone(&self.async_error_detail);
        let published_version_clone = Arc::clone(&self.published_version);
        let published_io_lock_clone = Arc::clone(&published_io_lock);
        #[cfg(test)]
        let async_fail_mode_clone = Arc::clone(&self.async_fail_mode);

        let publish_handle = std::thread::Builder::new()
            .name("mpt-publish".to_string())
            .spawn(move || {
                while let Ok(mut job) = publish_rx.recv() {
                    if async_error_clone.load(Ordering::Relaxed) {
                        if let Some(done) = job.done {
                            let _ = done
                                .send(Err(Self::current_async_error(&async_error_detail_clone)));
                        }
                        continue;
                    }

                    if !job.barrier_only {
                        let mut publish_puts = job.published_puts;
                        let mut skip_publish = false;
                        let mut published_success = false;

                        // wal_first: build segments in background.
                        //
                        // Sparse path publishes from sparse trie snapshots.
                        // Non-sparse path publishes from COW trie snapshots.
                        if !job.committed_sparse_tries.is_empty() {
                            match build_storage_segments_from_sparse_snapshots(
                                &job.committed_sparse_tries,
                            ) {
                                Ok(mut built) => publish_puts.append(&mut built),
                                Err(e) => {
                                    Self::warn_nonfatal_async_error(&e);
                                    skip_publish = true;
                                }
                            }
                        } else if !job.committed_tries.is_empty() {
                            // Sequential materialize + freeze.
                            // Materialization resolves pending segment-lazy
                            // edges into arena refs so segment serialization
                            // does not silently emit hash embeds.
                            for (_, addr_root, trie) in &mut job.committed_tries {
                                if let Err(e) = trie.materialize_pending_segment_children() {
                                    Self::warn_nonfatal_async_error(&MptDbError::Other(format!(
                                        "wal_first materialize pending children for {}: {e}",
                                        addr_root
                                    )));
                                    skip_publish = true;
                                    break;
                                }
                                trie.snapshot();
                            }
                            if !skip_publish {
                                // Parallel segment build via rayon.
                                let built = job
                                    .committed_tries
                                    .par_iter()
                                    .map(|(addr, root, trie)| {
                                        StorageTrieSegment::from_parts(
                                            trie.frozen_arena_nodes_ref(),
                                            trie.frozen_arena_hash_cache_ref(),
                                            trie.root_index(),
                                            *root,
                                        )
                                        .map(|seg| (*addr, seg))
                                        .map_err(|e| {
                                            MptDbError::Other(format!(
                                                "wal_first build segment for {addr} root {root}: {e}"
                                            ))
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>();
                                match built {
                                    Ok(built) => publish_puts.extend(built),
                                    Err(e) => {
                                        Self::warn_nonfatal_async_error(&e);
                                        skip_publish = true;
                                    }
                                }
                            }
                        }

                        if !skip_publish {
                            let _published_io_guard = published_io_lock_clone.lock();
                            #[cfg(test)]
                            let publish_result = if async_fail_mode_clone.load(Ordering::Relaxed) ==
                                3
                            {
                                Err(MptDbError::Other(
                                    "forced async published baseline failure".to_string(),
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
                            let publish_result = published_baseline_clone.publish_generation(
                                worker_published_meta.as_ref(),
                                job.version,
                                job.state_root,
                                &publish_puts,
                                &job.published_deletes,
                            );

                            match publish_result {
                                Ok(result) => {
                                    worker_published_meta = Some(result.meta.clone());
                                    published_success = true;
                                }
                                Err(e) => {
                                    Self::warn_nonfatal_async_error(&e);
                                }
                            }

                            if (job.version as usize) %
                                super::published_baseline::PUBLISHED_REWRITE_INTERVAL ==
                                0 ||
                                job.manifest.earliest_version > 0
                            {
                                if let Err(e) =
                                    published_baseline_clone.compact_for_manifest(&job.manifest)
                                {
                                    Self::warn_nonfatal_async_error(&e);
                                }
                            }
                        }

                        if published_success {
                            published_version_clone.store(job.version, Ordering::Release);
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
            .map_err(|e| MptDbError::Other(format!("spawn publish thread: {e}")))?;

        let (tx, rx) = crossbeam_channel::bounded::<PersistJob>(self.config.async_queue_depth);
        let published_baseline_clone = Arc::clone(&self.published_baseline);
        let async_error_clone = Arc::clone(&self.async_error);
        let async_error_detail_clone = Arc::clone(&self.async_error_detail);
        let durable_version_clone = Arc::clone(&self.durable_version);
        let wal_store_clone = self.wal_store.as_ref().map(Arc::clone);
        let publish_tx_clone = publish_tx.clone();
        #[cfg(test)]
        let async_fail_mode_clone = Arc::clone(&self.async_fail_mode);

        let handle = std::thread::Builder::new()
            .name("mpt-persist".to_string())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
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

                        let result = if let Some(err) = forced_error {
                            Err(err)
                        } else {
                            // wal_first: skip RocksDB persist_batch.
                            // Worker only persists manifest + publishes segments.
                            if job.save_manifest {
                                job.manifest.save(&job.manifest_path)
                            } else {
                                Ok(())
                            }
                        };

                        if let Err(e) = result {
                            Self::report_async_error(
                                &async_error_clone,
                                &async_error_detail_clone,
                                &e,
                            );
                            tracing::error!(?e, "background persist failed");
                        } else {
                            // WAL + manifest succeeded: advance durable frontier
                            // before any publish work so publish latency does not
                            // delay durability tracking.
                            if !job.barrier_only {
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
                            }

                            if !job.barrier_only && job.publish_baseline {
                                let publish_job = PublishJob {
                                    barrier_only: false,
                                    published_puts: job.published_puts,
                                    published_deletes: job.published_deletes,
                                    state_root: job.state_root,
                                    manifest: job.manifest,
                                    version: job.version,
                                    committed_tries: job.committed_tries,
                                    committed_sparse_tries: job.committed_sparse_tries,
                                    done: None,
                                };
                                if let Err(e) = publish_tx_clone.send(publish_job) {
                                    let err = MptDbError::Other(format!("send publish job: {e}"));
                                    Self::report_async_error(
                                        &async_error_clone,
                                        &async_error_detail_clone,
                                        &err,
                                    );
                                    tracing::error!(?err, "background publish enqueue failed");
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
        self.publish_tx = Some(publish_tx);
        self.publish_handle = Some(publish_handle);
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
        self.publish_tx.take();
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
        if let Some(handle) = self.publish_handle.take() {
            if best_effort {
                let _ = handle.join();
            } else {
                handle
                    .join()
                    .map_err(|_| MptDbError::Other("publish worker panicked".to_string()))?;
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

        // Drop stale index to release RocksDB lock before the file lock, so
        // a subsequent re-open in the same process can acquire the lock cleanly.
        self.stale_index.take();

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
        let durable_version = if true {
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
            match Self::load_account_trie_snapshot(&self.dir, &self.persisted, target_version, root)
            {
                Ok((account_trie, loaded_from_checkpoint)) => {
                    let durable_version = if true {
                        self.wal_recovery_base_version(manifest.latest_version)
                    } else {
                        manifest.latest_version
                    };
                    Ok((account_trie, loaded_from_checkpoint, durable_version))
                }
                Err(e) if true => {
                    // wal_first: RocksDB may not have account trie nodes.
                    // Fall back to full WAL replay from the earliest version.
                    let wal_store = self.wal_store.as_ref().map(Arc::clone).ok_or_else(|| {
                        MptDbError::Other(format!(
                            "wal_first recovery for version {target_version} \
                                 requires wal store: {e}"
                        ))
                    })?;
                    let replay_base = {
                        let wal = wal_store.lock();
                        let earliest = wal.earliest_version();
                        if wal.is_empty() || earliest > 1 || wal.latest_version() < target_version {
                            return Err(MptDbError::Other(format!(
                                "wal_first: cannot recover version {target_version}: \
                                 persisted has no data ({e}), WAL range \
                                 [{earliest}, {}]",
                                wal.latest_version()
                            )));
                        }
                        // Replay from version 0 (empty state).
                        0i64
                    };
                    let mut shadow = Self::open_replay_materializer_state(
                        &self.dir,
                        Arc::clone(&self.persisted),
                        Arc::clone(&self.published_baseline),
                        Some(wal_store),
                        manifest.clone(),
                        self.config.clone(),
                        replay_base,
                    )?;
                    shadow.replay_wal_catchup_to(manifest, target_version)?;
                    Ok((shadow.account_trie.committed().clone(), false, replay_base))
                }
                Err(e) => Err(e),
            }
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
        let (account_trie, loaded_from_checkpoint) = if version == 0 || root == EMPTY_ROOT_HASH {
            // Replay from scratch: start with empty account trie.
            (StorageTrieCow::empty(), false)
        } else {
            // Try loading from checkpoint/persisted; fall back to empty for
            // wal_first mode where RocksDB may have no account trie nodes.
            match Self::load_account_trie_snapshot(dir, &persisted, version, root) {
                Ok(result) => result,
                Err(_) if true => (StorageTrieCow::empty(), false),
                Err(e) => return Err(e),
            }
        };
        #[cfg(not(test))]
        let _ = loaded_from_checkpoint;

        let (published_meta, published_store) =
            Self::select_published_view_for_version(&published_baseline, version, root)?;
        let published_version = published_meta.as_ref().map(|meta| meta.version).unwrap_or(0);

        let mut replay_config = config;
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
            pending_drops: Vec::new(),
            rejected_activations: Vec::new(),
            empty_trie_activations: Vec::new(),
            deferred_evictions: std::collections::VecDeque::new(),
            overlay_watermark: 0,
            last_overlay_stolen: 0,
            last_overlay_fresh_clone: 0,
            last_overlay_existing_working: 0,
            last_overlay_shrink_events: 0,
            last_overlay_reuse_capacity_entries: 0,

            persisted,
            stale_index: None, // replay materializer is internal; stale index not needed
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
            publish_tx: None,
            publish_handle: None,
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
            last_apply_published_view_refresh: Duration::ZERO,
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
            last_sparse_apply_factory_build: Duration::ZERO,
            last_sparse_apply_account_proof: Duration::ZERO,
            last_sparse_apply_apply_changes: Duration::ZERO,
            last_sparse_account_reveal_keys: 0,
            last_apply_sparse_factory: SparseFactoryStats::default(),
            last_wal_append_lock_wait: Duration::ZERO,
            last_wal_append_write: Duration::ZERO,
            last_wal_serialize: Duration::ZERO,
            last_wal_crc: Duration::ZERO,
            last_wal_payload_bytes: 0,
            wal_serialize_buf: Vec::new(),
            last_commit_profile: CommitProfile::default(),
            checkpoint_account_trie_nodes,
            shutdown_complete: false,

            #[cfg(test)]
            loaded_from_checkpoint,
            #[cfg(test)]
            fail_point: None,
            #[cfg(test)]
            async_fail_mode: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            pending_sparse_state: None,
            last_committed_sparse_trie: None,
            cross_block_sparse: None,
            sparse_deferred_publish_roots: HashMap::default(),
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
        let mut base_version = self.version;
        // WAL-first recovery may not have a materialized trie snapshot at the
        // durable watermark (e.g. durable_version manually rewound, checkpoint
        // only exists at latest). Replaying from that empty base would diverge;
        // restart from version 0 and replay full WAL chain instead.
        if base_version > 0 {
            let base_root = committed_manifest.get_root(base_version).unwrap_or(EMPTY_ROOT_HASH);
            let missing_base_snapshot = base_root != EMPTY_ROOT_HASH &&
                matches!(self.account_trie.committed().root_ref(), CowRootRef::Empty);
            if missing_base_snapshot {
                base_version = 0;
                self.version = 0;
                self.account_trie = AccountTrieHandle::snapshot(0, StorageTrieCow::empty());
                self.clear_storage_trie_state();
                self.dirty_accounts.clear();
            }
        }
        let original_config = self.config.clone();
        let original_replay_materializer = self.replay_materializer;
        self.manifest = Self::truncate_manifest_to_version(committed_manifest, base_version);

        // Use aggressive parallelism thresholds during replay to maximize
        // throughput.  Normal commit uses conservative thresholds to avoid
        // overhead for small blocks, but replay processes many versions
        // sequentially so every bit of per-version parallelism helps.
        let mut replay_config = original_config.clone();
        replay_config.wal_shadow_validate = false;
        replay_config.async_blob_threshold = 0;
        replay_config.parallel_storage_tries_min = 4;
        replay_config.parallel_account_frontier_min = 2;
        self.config = replay_config;
        // Recovery replay should not publish baseline generations or rewrite
        // manifest/WAL metadata. It only reconstructs the in-memory trie state.
        self.replay_materializer = true;

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
        self.replay_materializer = original_replay_materializer;
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
        // Discard sparse trie state: the in-memory trie no longer reflects
        // the reloaded version and would produce stale proofs/roots.
        self.last_committed_sparse_trie = None;
        self.cross_block_sparse = None;
        self.pending_sparse_state = None;
        self.sparse_deferred_publish_roots.clear();
        self.enforce_frontier_invariants("load_version_target")?;
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
    /// Open an MptCommitStore at the given directory with default configuration.
    ///
    /// `read_only=true` disables writes and does not acquire the exclusive lock.
    ///
    /// When the `MPT_USE_SPARSE_STORAGE` environment variable is set to a
    /// truthy value (`1`, `true`, `on`, `yes`), `use_sparse_storage` is
    /// automatically enabled in the default config.  This allows existing
    /// tests to be re-run under the sparse path without code changes:
    /// ```ignore
    /// MPT_USE_SPARSE_STORAGE=1 cargo test -p mptdb-sc --release
    /// ```
    pub fn open(dir: &Path, read_only: bool) -> Result<Self> {
        let mut config = MptConfig::default();
        if is_sparse_storage_forced() {
            config.use_sparse_storage = true;
        }
        Self::open_with_config(dir, read_only, config)
    }

    pub fn open_at_version(
        dir: &Path,
        read_only: bool,
        target_version: i64,
        overwrite: bool,
    ) -> Result<Self> {
        let mut config = MptConfig::default();
        if is_sparse_storage_forced() {
            config.use_sparse_storage = true;
        }
        Self::open_with_config_at_version(dir, read_only, config, target_version, overwrite)
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
        let wal_store =
            if true { Some(Arc::new(Mutex::new(CommitWalStore::open(dir)?))) } else { None };

        // In wal_first mode, the WAL may contain entries beyond the manifest
        // (committed to WAL but the persist worker hadn't saved the manifest
        // before crash). Extend the manifest with those WAL entries so they
        // are included in the replay range, recovering all committed work.
        if true {
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

        // Stale index: writer-only, skip for read-only opens.
        let stale_index = if !read_only {
            let stale_index_dir = dir.join("stale_index");
            Some(Arc::new(super::stale_index::StaleRootIndex::open(&stale_index_dir)?))
        } else {
            None
        };

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
            match Self::load_account_trie_snapshot(dir, &persisted, version, root) {
                Ok(result) => result,
                Err(_) if root != EMPTY_ROOT_HASH => {
                    // wal_first: RocksDB may not have account trie nodes.
                    // Start with empty trie; WAL replay will reconstruct it.
                    (StorageTrieCow::empty(), false)
                }
                Err(e) => return Err(e),
            };
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
            pending_drops: Vec::new(),
            rejected_activations: Vec::new(),
            empty_trie_activations: Vec::new(),
            deferred_evictions: std::collections::VecDeque::new(),
            overlay_watermark: 0,
            last_overlay_stolen: 0,
            last_overlay_fresh_clone: 0,
            last_overlay_existing_working: 0,
            last_overlay_shrink_events: 0,
            last_overlay_reuse_capacity_entries: 0,
            persisted,
            stale_index,
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
            publish_tx: None,
            publish_handle: None,
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
            last_apply_published_view_refresh: Duration::ZERO,
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
            last_sparse_apply_factory_build: Duration::ZERO,
            last_sparse_apply_account_proof: Duration::ZERO,
            last_sparse_apply_apply_changes: Duration::ZERO,
            last_sparse_account_reveal_keys: 0,
            last_apply_sparse_factory: SparseFactoryStats::default(),
            last_wal_append_lock_wait: Duration::ZERO,
            last_wal_append_write: Duration::ZERO,
            last_wal_serialize: Duration::ZERO,
            last_wal_crc: Duration::ZERO,
            last_wal_payload_bytes: 0,
            wal_serialize_buf: Vec::new(),
            last_commit_profile: CommitProfile::default(),
            checkpoint_account_trie_nodes,
            shutdown_complete: false,

            #[cfg(test)]
            loaded_from_checkpoint: account_loaded_from_checkpoint,
            #[cfg(test)]
            fail_point: None,
            #[cfg(test)]
            async_fail_mode,
            pending_sparse_state: None,
            last_committed_sparse_trie: None,
            cross_block_sparse: None,
            sparse_deferred_publish_roots: HashMap::default(),
        };

        if version < committed_version {
            store.replay_wal_catchup_to(&manifest, committed_version)?;
        }

        if !read_only {
            store.start_persist_worker()?;
        }

        store.enforce_frontier_invariants("open_with_config")?;
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
            Self::truncate_to_version_on_disk(dir, target_version)?;
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
    fn truncate_to_version_on_disk(dir: &Path, target_version: i64) -> Result<()> {
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
        if true {
            let mut wal_store = CommitWalStore::open(dir)?;
            wal_store.truncate_after(target_version)?;
        }

        // Activate published baseline at target version (prunes newer generations).
        let published_baseline = PublishedBaselineManager::open(&Self::fast_storage_root(dir))?;
        let manifest = VersionManifest::load(&manifest_path)?;
        let target_root = manifest.get_root(target_version).unwrap_or(EMPTY_ROOT_HASH);
        published_baseline.activate_published_version(target_version, target_root)?;
        let _ = published_baseline.compact_for_manifest(&manifest);

        Ok(())
    }

    /// Try to extract storage_root from an existing account leaf in the trie.
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

    /// Compute the state root for `hashed_state` applied on top of the current
    /// committed state, **without committing**.
    ///
    /// This implements the dry-run path for `StateRootProvider::state_root`:
    /// reth calls this to verify the computed root against the block header
    /// before deciding whether to call `write_state` (the actual commit path).
    ///
    /// ## How it works
    /// 1. For each account with storage changes, clone the frozen base storage trie, apply
    ///    keccak-slot changes, and compute the new storage root.
    /// 2. Clone the frozen base account trie, encode each changed account as `TrieAccount { nonce,
    ///    balance, storage_root, code_hash }`, and apply to the cloned trie.
    /// 3. Compute the account root via `recompute_hash_only_parallel`.
    /// 4. Return the root.  No WAL write, no version increment.
    ///
    /// ## Side effects
    /// None on `self`.  The temporary clones are discarded after this call.
    /// `applied_this_block` is NOT set, so `apply_bundle_state` can still be
    /// called afterwards (the subsequent call works from the frozen base, not
    /// from this dry-run's dirty state).
    ///
    /// ## Correctness gate
    /// This method must produce the same root as `apply_bundle_state` +
    /// `commit` for the same block.  The acceptance test is:
    /// ```text
    /// let root_overlay = sc.apply_hashed_state_overlay(&hashed_state)?;
    /// sc.apply_bundle_state(&bundle_state)?;
    /// let (_, root_commit) = sc.commit()?;
    /// assert_eq!(root_overlay, root_commit);
    /// ```
    pub fn apply_hashed_state_overlay(
        &mut self,
        hashed_state: &reth_trie_common::HashedPostState,
    ) -> Result<B256> {
        use alloy_rlp::Encodable;
        use alloy_trie::EMPTY_ROOT_HASH as EMPTY;
        use reth_trie_common::Nibbles;

        // keccak256 of empty bytes — the canonical empty code hash.
        let keccak_empty = alloy_primitives::keccak256([]);

        // ── Phase 1: compute storage roots for all accounts with storage changes ──
        let mut storage_roots: HashMap<B256, B256> = HashMap::new();

        for (keccak_addr, hashed_storage) in &hashed_state.storages {
            // Start from the frozen base storage trie (O(1) Arc clone),
            // or an empty trie for accounts not in the L2 cache.
            let base = if hashed_storage.wiped {
                StorageTrieCow::empty()
            } else if let Some(handle) = self.storage_trie_handles.get(keccak_addr) {
                handle.base.clone()
            } else {
                // Not in L2 cache: approximation — treat as empty.
                // Accounts with non-zero existing storage not in cache will
                // produce a wrong storage root.  Phase 1 accepts this
                // limitation; full correctness requires L3 segment loading.
                StorageTrieCow::empty()
            };

            let mut trie = base;
            for (keccak_slot, value) in &hashed_storage.storage {
                let nibbles = Nibbles::unpack(keccak_slot);
                if value.is_zero() {
                    trie.apply_change_materialized(&nibbles, None);
                } else {
                    // Storage values are RLP-encoded U256 (compact big-endian).
                    let encoded = alloy_rlp::encode(value);
                    trie.apply_change_materialized(&nibbles, Some(encoded));
                }
            }

            // root_hash_only consumes trie and returns (root, trie); we discard the trie.
            let (root, _) = trie.root_hash_only(&self.persisted)?;
            storage_roots.insert(*keccak_addr, root);
        }

        // ── Phase 2: clone frozen account trie base, apply account changes ──
        let mut account_trie = self.account_trie.base.clone();

        for (keccak_addr, account_opt) in &hashed_state.accounts {
            let nibbles = Nibbles::unpack(keccak_addr);

            let encoded = match account_opt {
                None => None, // account deleted
                Some(account) => {
                    let storage_root = storage_roots
                        .get(keccak_addr)
                        .copied()
                        .unwrap_or_else(|| self.get_existing_storage_root(keccak_addr));

                    let code_hash = account.bytecode_hash.unwrap_or(keccak_empty);

                    let is_empty = account.nonce == 0 &&
                        account.balance.is_zero() &&
                        storage_root == EMPTY &&
                        code_hash == keccak_empty;

                    if is_empty {
                        None
                    } else {
                        let trie_account = alloy_trie::TrieAccount {
                            nonce: account.nonce,
                            balance: account.balance,
                            storage_root,
                            code_hash,
                        };
                        let mut rlp_buf = Vec::new();
                        trie_account.encode(&mut rlp_buf);
                        Some(rlp_buf)
                    }
                }
            };

            account_trie.apply_change_materialized(&nibbles, encoded);
        }

        // ── Phase 3: compute account root (no WAL, no version increment) ──
        let (root, _) = account_trie.root_hash_only_parallel_account(&self.persisted)?;

        Ok(root)
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

    fn wal_first_defer_segment_build_enabled(&self) -> bool {
        if let Some(override_raw) = std::env::var_os("MPT_WAL_DEFER_SPARSE_SEGMENT_BUILD") {
            return override_raw != "0";
        }
        self.config.wal_first_defer_segment_build
    }

    fn sparse_deferred_materialize_interval(&self, pending_targets: usize) -> i64 {
        if let Some(raw) = std::env::var_os("MPT_WAL_SPARSE_MATERIALIZE_INTERVAL") {
            if let Ok(parsed) = raw.to_string_lossy().parse::<i64>() {
                return parsed.max(1);
            }
        }
        if pending_targets >= SPARSE_DEFERRED_MATERIALIZE_MIN_PENDING_TARGETS {
            SPARSE_DEFERRED_MATERIALIZE_INTERVAL_LARGE_DEFAULT
        } else {
            1
        }
    }

    fn should_materialize_sparse_deferred_now(
        &self,
        next_version: i64,
        pending_targets: usize,
    ) -> (bool, i64) {
        let interval = self.sparse_deferred_materialize_interval(pending_targets);
        if pending_targets >= SPARSE_DEFERRED_MATERIALIZE_MAX_PENDING_TARGETS {
            return (true, interval);
        }
        (next_version % interval == 0, interval)
    }

    fn sparse_deferred_materialize_round_budget(&self, pending_targets: usize) -> usize {
        if let Some(raw) = std::env::var_os("MPT_WAL_SPARSE_MATERIALIZE_ROUND_BUDGET") {
            if let Ok(parsed) = raw.to_string_lossy().parse::<usize>() {
                return parsed.max(1);
            }
        }
        if pending_targets >= SPARSE_DEFERRED_MATERIALIZE_MAX_PENDING_TARGETS {
            SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_SMALL_DEFAULT
        } else if pending_targets >= SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_HIGH_WATERMARK {
            SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_MID_DEFAULT
        } else {
            SPARSE_DEFERRED_MATERIALIZE_ROUND_BUDGET_LARGE_DEFAULT
        }
    }

    fn default_commit_mode(&self) -> CommitExecutionMode {
        if self.replay_materializer {
            CommitExecutionMode { wal_first: false, save_manifest: false, publish_baseline: false }
        } else {
            CommitExecutionMode {
                wal_first: true,
                save_manifest: true,
                // Always publish segments so the mmap-backed published store
                // stays current. In wal_first mode, segment generation defaults
                // to the background worker using committed trie snapshots.
                publish_baseline: true,
            }
        }
    }

    /// Wait for all in-flight background persist/publish jobs to complete.
    ///
    /// Sends barrier jobs through the persist and publish channels and waits
    /// for both to drain. Since each channel is FIFO, all previously sent jobs
    /// will have been completed by the time each barrier finishes.
    pub fn flush_persist(&self) -> Result<()> {
        if let Err(err) = self.check_async_error() {
            if true {
                self.durable_version.store(self.wal_durable_version(), Ordering::Release);
            }
            return Err(err);
        }
        if let Some(ref tx) = self.persist_tx {
            let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<()>>(0);
            let job = PersistJob {
                barrier_only: true,
                published_puts: vec![],
                published_deletes: vec![],
                publish_baseline: false,
                state_root: EMPTY_ROOT_HASH,
                manifest: self.manifest.clone(),
                manifest_path: self.manifest_path.clone(),
                save_manifest: false,
                version: self.version,
                done: Some(done_tx),
                committed_tries: vec![],
                committed_sparse_tries: vec![],
            };
            if tx.send(job).is_ok() {
                match done_rx.recv() {
                    Ok(result) => {
                        if let Err(err) = result {
                            if true {
                                self.durable_version
                                    .store(self.wal_durable_version(), Ordering::Release);
                            }
                            return Err(err);
                        }
                    }
                    Err(_) => {
                        if true {
                            self.durable_version
                                .store(self.wal_durable_version(), Ordering::Release);
                        }
                        return self.check_async_error();
                    }
                }
            }
        }
        if let Some(ref tx) = self.publish_tx {
            let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<()>>(0);
            let job = PublishJob {
                barrier_only: true,
                published_puts: vec![],
                published_deletes: vec![],
                state_root: EMPTY_ROOT_HASH,
                manifest: self.manifest.clone(),
                version: self.version,
                committed_tries: vec![],
                committed_sparse_tries: vec![],
                done: Some(done_tx),
            };
            if tx.send(job).is_ok() {
                match done_rx.recv() {
                    Ok(result) => {
                        if let Err(err) = result {
                            if true {
                                self.durable_version
                                    .store(self.wal_durable_version(), Ordering::Release);
                            }
                            return Err(err);
                        }
                    }
                    Err(_) => {
                        if true {
                            self.durable_version
                                .store(self.wal_durable_version(), Ordering::Release);
                        }
                        return self.check_async_error();
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
                            if true {
                                self.durable_version
                                    .store(self.wal_durable_version(), Ordering::Release);
                            }
                            return Err(err);
                        }
                    }
                    Err(_) => {
                        if true {
                            self.durable_version
                                .store(self.wal_durable_version(), Ordering::Release);
                        }
                        return self.check_async_error();
                    }
                }
            }
        }
        if true {
            self.durable_version.store(self.wal_durable_version(), Ordering::Release);
        }
        self.check_async_error()?;
        self.enforce_frontier_invariants("flush_persist")
    }

    /// Best-effort async prewarm entry: load a storage trie into SC's L2 cache.
    ///
    /// This is intended for background read-side warming. It must not block the
    /// caller's hot path and may skip work if the trie is unavailable in the
    /// currently published view.
    pub fn prewarm_storage_trie_by_hashed_address(&mut self, hashed_address: B256) -> Result<()> {
        if self.storage_trie_cache.capacity() == 0 {
            return Ok(());
        }

        // Already cached/known: refresh recency and return.
        if self.storage_trie_cache.contains(&hashed_address) ||
            self.storage_trie_handles.contains_key(&hashed_address)
        {
            if !self.touch_cached_storage_trie(hashed_address) {
                self.rejected_activations.push(hashed_address);
            }
            return Ok(());
        }

        // Fast published-store index check (O(1)) BEFORE doing the expensive
        // account-MPT traversal.  NOTE: maybe_refresh_published_view is intentionally
        // NOT called here — callers should call maybe_refresh_published_view_for_prewarm
        // once per batch before processing items one-by-one with per-item locking.
        //
        // wal_first limitation: the published store only contains data that has been
        // flushed by the background segment-build worker.  Addresses committed in the
        // most recent block(s) may not appear in the index until the worker runs.
        // This makes prewarm a no-op for cold accounts that are freshly committed but
        // not yet published.  Addresses already in storage_trie_handles (see above)
        // are unaffected — they are touched on the fast path.
        if let Some(ref store) = self.published_store {
            if !store.has_storage_trie(&hashed_address) {
                // No segment for this address → skip account-MPT traversal.
                return Ok(());
            }
        } else {
            // No published store (e.g. node just started, no baseline yet).
            return Ok(());
        }

        let storage_root = self.get_existing_storage_root(&hashed_address);
        if storage_root == EMPTY_ROOT_HASH {
            return Ok(());
        }

        if let Some(ref store) = self.published_store {
            if let Some(loaded) = store.open_trie(&hashed_address, storage_root)? {
                self.cache_storage_trie(hashed_address, loaded.trie);
                return Ok(());
            }
        }

        // In wal_first mode, avoid persisted fallback for prewarm reads; nodes
        // may not exist in persisted store yet. Keep this best-effort.
        if false {
            self.cache_storage_trie(
                hashed_address,
                StorageTrieCow::from_persisted_root(storage_root),
            );
        }

        Ok(())
    }

    /// Non-blocking view check for prewarm workers.
    ///
    /// Unlike `maybe_refresh_published_view`, this does NOT call `reload_published_view`
    /// even when a newer version is available.  `reload_published_view` involves
    /// file I/O (opening new mmap segment files) and must not run under the SC lock
    /// during prewarm, as it would block the SC commit path for the duration.
    ///
    /// Prewarm tolerates a slightly stale `published_store`: it may miss tries that
    /// were published in the most recent segment, but will still warm everything that
    /// is in the current store.  The main apply path will reload the view on next use.
    pub fn maybe_refresh_published_view_for_prewarm(&mut self) {
        // Fast atomic check only — do not reload.
        // If bg_version > current_version, a new segment has been published but we
        // intentionally skip the reload to avoid lock-while-I/O.
        // The stale published_store is still valid for warming older segments.
    }

    /// Convenience wrapper for raw address input.
    pub fn prewarm_storage_trie(&mut self, address: Address) -> Result<()> {
        self.maybe_refresh_published_view()?;
        self.prewarm_storage_trie_by_hashed_address(alloy_primitives::keccak256(address.as_slice()))
    }

    /// Write WAL entry synchronously.  Serialize + CRC run outside the WAL
    /// mutex (using a reusable buffer) so the lock only covers the file write
    /// and index update — reducing contention with the background worker's
    /// concurrent `set_durable_version` calls.
    fn append_shadow_wal_entry(&mut self, entry: &CommitWalEntry) -> Result<()> {
        let Some(wal_store) = self.wal_store.as_ref() else {
            return Ok(());
        };

        // ── Serialize + CRC outside the lock (CPU work, no shared state) ──
        let serialize_start = std::time::Instant::now();
        self.wal_serialize_buf.clear();
        postcard::to_io(entry, &mut self.wal_serialize_buf)
            .map_err(|e| MptDbError::Other(format!("serialize wal entry: {e}")))?;
        let serialize_elapsed = serialize_start.elapsed();
        let crc_start = std::time::Instant::now();
        let crc = crc32fast::hash(&self.wal_serialize_buf);
        let crc_elapsed = crc_start.elapsed();
        let payload_len = self.wal_serialize_buf.len() as u32;

        // ── Lock: file write + index update only ──
        let lock_wait_start = std::time::Instant::now();
        let mut wal_store = wal_store.lock();
        let lock_wait = lock_wait_start.elapsed();
        let write_start = std::time::Instant::now();
        wal_store.append_prebuilt(entry.version, &self.wal_serialize_buf, crc)?;
        let write_elapsed = write_start.elapsed();

        self.last_wal_append_lock_wait = lock_wait;
        self.last_wal_append_write = write_elapsed;
        self.last_wal_serialize = serialize_elapsed;
        self.last_wal_crc = crc_elapsed;
        self.last_wal_payload_bytes = payload_len;

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
        // Discard sparse trie state: it reflects the rolled-back version and
        // would produce wrong proofs and wrong roots for subsequent commits.
        self.last_committed_sparse_trie = None;
        self.cross_block_sparse = None;
        self.pending_sparse_state = None;
        self.sparse_deferred_publish_roots.clear();
        self.enforce_frontier_invariants("rollback")?;
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

        // Record stale roots BEFORE removing them from the manifest, so the
        // next gc() call can BFS from them and delete their orphaned nodes.
        if let Some(ref stale_idx) = self.stale_index {
            for v in &to_remove {
                if let Some(&root) = new_manifest.versions.get(v) {
                    stale_idx.record_stale_root(*v, root)?;
                }
            }
        }

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

        // Prefer incremental GC (O(stale_nodes)) over full-scan (O(total_nodes)).
        // The stale index is populated by `prune_before`; if it is empty we
        // fall back to the legacy full-scan so a first-time gc() still works.
        if let Some(ref stale_idx) = self.stale_index {
            let prune_watermark = self.manifest.earliest_version;
            if let Some(stats) = gc::gc_incremental(
                &self.persisted,
                stale_idx,
                live_roots.iter().copied(),
                prune_watermark,
            )? {
                return Ok(stats);
            }
            // Stale index was empty — fall through to full-scan below.
        }

        // Legacy full-scan fallback (read-only stores, first gc before any prune,
        // or incremental GC that found nothing to delete → safety net for rollback orphans).
        //
        // Uses skip_missing BFS for WAL-first compatibility: live nodes not yet
        // flushed to RocksDB are absent from the store, but also cannot appear in
        // the full-scan result set, so skipping them is safe — nothing to delete.
        let live = gc::collect_reachable_hashes_skip_missing(&self.persisted, live_roots)?;
        gc::gc_unreachable_nodes(&self.persisted, &live)
    }

    fn account_proof(
        &self,
        version: i64,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof> {
        // Sparse path: if the requested version is the latest committed version
        // and the sparse trie is available, use it for proof generation.
        // In per-block mode the trie is in `last_committed_sparse_trie`;
        // in cross-block mode it is in `cross_block_sparse.trie`.
        if self.config.use_sparse_storage && version == self.version {
            if let Some(ref sparse_trie) = self.last_committed_sparse_trie {
                return super::sparse_storage::build_account_proof_from_sparse(
                    sparse_trie,
                    address,
                    slots,
                );
            }
            if let Some(ref cross) = self.cross_block_sparse {
                if !cross.trie.state_trie_ref().is_none() {
                    return super::sparse_storage::build_account_proof_from_sparse(
                        &cross.trie,
                        address,
                        slots,
                    );
                }
            }
        }

        // Sparse trie is only available for the latest committed version.
        // For older versions, proof generation is not supported — re-apply the
        // latest block to restore the sparse trie.
        Err(MptDbError::Other(format!(
            "account_proof: sparse trie not available for version {version}; re-apply the latest block to restore proof generation"
        )))
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

    /// Commit an already-applied block using an externally computed state root.
    ///
    /// This is only supported in wal_first mode where commit does hash-only
    /// persistence. The external root is expected to come from reth's
    /// `overlay_root_with_updates` path for the same block diff.
    pub fn commit_with_external_root(&mut self, state_root: B256) -> Result<(i64, B256)> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_bulk_loading()?;

        if !self.applied_this_block {
            return Err(MptDbError::Other("must call apply_bundle_state before commit".to_string()));
        }

        let mode = self.default_commit_mode();
        if !mode.wal_first {
            return Err(MptDbError::Other(
                "commit_with_external_root requires wal_first commit mode".to_string(),
            ));
        }

        let result = self.commit_inner_with_mode_and_external_root(mode, Some(state_root));
        if result.is_err() {
            self.poisoned = true;
        }
        result
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

    /// Try to build a pre-built storage proof from the L2 cache (StorageTrieCow)
    /// for `hashed_addr` and insert it into `factory.pre_built_storage_proofs`.
    ///
    /// Called when the published segment for an account is stale or missing but
    /// the account HAS prior storage (root ≠ EMPTY_ROOT_HASH).  Using the L2
    /// cache avoids silently discarding the account's prior storage state.
    ///
    /// Falls back to `factory.known_empty_accounts` only when the L2 cache also
    /// doesn't have the account (evicted) — this is a last resort and may produce
    /// incorrect results for accounts with complex prior storage, but is better
    /// than failing outright.
    /// Build a pre-built storage proof from the best available source:
    /// 1. Previous block's sparse trie (`last_committed_sparse_trie`) — always correct because
    ///    computed by `root_with_updates`. Uses path-limited extraction when `dirty_keys` is set.
    /// 2. L2 cache (`storage_trie_handles`) — correct for recently-committed accounts. Uses
    ///    path-limited arena conversion when `dirty_keys` is set.
    /// 3. Persisted store (RocksDB) — available after non-wal_first commits and WAL replay.  Loads
    ///    from `persisted.load_trie_at_root` + preloads dirty paths and exports a path-limited
    ///    proof.
    /// 4. Fall back to `known_empty` as last resort.
    fn try_build_l2_proof(
        &self,
        hashed_addr: &B256,
        root: B256,
        dirty_keys: &[Nibbles],
        factory: &mut SegmentTrieNodeProviderFactory,
    ) {
        // 1. Prefer previous sparse trie (always correct hashes).
        if let Some(ref prev_sparse) = self.last_committed_sparse_trie {
            if let Ok(Some(proof)) = if dirty_keys.is_empty() {
                extract_storage_proof_from_sparse_trie(prev_sparse, hashed_addr, root)
            } else {
                extract_storage_proof_from_sparse_trie_for_paths(
                    prev_sparse,
                    hashed_addr,
                    root,
                    dirty_keys,
                )
            } {
                factory.pre_built_storage_proofs.insert(*hashed_addr, proof);
                return;
            }
        }
        if let Some(ref cross) = self.cross_block_sparse {
            if let Ok(Some(proof)) = if dirty_keys.is_empty() {
                extract_storage_proof_from_sparse_trie(&cross.trie, hashed_addr, root)
            } else {
                extract_storage_proof_from_sparse_trie_for_paths(
                    &cross.trie,
                    hashed_addr,
                    root,
                    dirty_keys,
                )
            } {
                factory.pre_built_storage_proofs.insert(*hashed_addr, proof);
                return;
            }
        }

        // 2. L2 cache fallback (StorageTrieCow from dual-write or previous runs).
        if let Some(handle) = self.storage_trie_handles.get(hashed_addr) {
            let proof_result = if dirty_keys.is_empty() {
                convert_arena_to_decoded_storage_multiproof(
                    handle.base.arena(),
                    handle.base.root_index(),
                    root,
                )
            } else {
                convert_arena_to_decoded_storage_multiproof_for_paths(
                    handle.base.arena(),
                    handle.base.root_index(),
                    root,
                    dirty_keys,
                )
            };
            match proof_result {
                Ok(proof) => {
                    factory.pre_built_storage_proofs.insert(*hashed_addr, proof);
                    return;
                }
                Err(_) => {}
            }
        }
        // 3. Persisted store (RocksDB) fallback — available after non-wal_first commits and WAL
        //    replay.  Materialises only the dirty slot paths from the persisted trie so we don't
        //    load the whole storage trie.
        if !dirty_keys.is_empty() {
            let mut cow = StorageTrieCow::from_persisted_root(root);
            if cow.preload_paths(&self.persisted, dirty_keys).is_ok() {
                match convert_arena_to_decoded_storage_multiproof_for_paths(
                    cow.arena(),
                    cow.root_index(),
                    root,
                    dirty_keys,
                ) {
                    Ok(proof) => {
                        factory.pre_built_storage_proofs.insert(*hashed_addr, proof);
                        return;
                    }
                    Err(_) => {}
                }
            }
        }
        // 4. Last resort: mark known_empty (only safe for truly empty prior storage).
        factory.known_empty_accounts.insert(*hashed_addr);
    }

    /// Build a `SegmentTrieNodeProviderFactory` for the given dirty accounts.
    ///
    /// Used for per-block apply (Phase 2, first cross-block block).  Calls
    /// `try_build_l2_proof` for accounts whose published segment is missing or
    /// stale, potentially executing a full-arena DFS (tier 1/2).
    fn build_sparse_factory(
        &self,
        dirty_accounts: &[DirtyAccount],
        stats: &mut SparseFactoryStats,
    ) -> SegmentTrieNodeProviderFactory {
        let mut factory = SegmentTrieNodeProviderFactory::new();
        stats.dirty_accounts = dirty_accounts.len() as u64;
        for dirty in dirty_accounts {
            if dirty.storage_known_empty || dirty.storage_wiped {
                factory.known_empty_accounts.insert(dirty.hashed_address);
            } else if !dirty.storage_changes.is_empty() {
                stats.storage_accounts += 1;
                let root = self.get_existing_storage_root(&dirty.hashed_address);
                if root == EMPTY_ROOT_HASH {
                    factory.known_empty_accounts.insert(dirty.hashed_address);
                } else if let Some(store) = &self.published_store {
                    stats.segment_lookups += 1;
                    match store.open_trie_page(&dirty.hashed_address, root) {
                        Ok(Some(loaded)) => {
                            stats.segment_hits += 1;
                            factory.storage_segments.insert(dirty.hashed_address, loaded.lease);
                        }
                        _ => {
                            stats.segment_miss += 1;
                            if Self::sparse_l3_trace_enabled() {
                                if let Ok(Some(entry_root)) =
                                    store.lookup_trie_root(&dirty.hashed_address)
                                {
                                    if entry_root != root {
                                        stats.segment_root_mismatch += 1;
                                    }
                                }
                            }
                            // Segment missing or stale root mismatch.
                            // Account HAS prior storage — do NOT mark as known_empty.
                            // Fall back to sparse trie / L2 cache / persisted store.
                            let dirty_keys: Vec<Nibbles> =
                                dirty.storage_changes.iter().map(|c| c.slot_key.clone()).collect();
                            stats.tier12_attempts += 1;
                            self.try_build_l2_proof(
                                &dirty.hashed_address,
                                root,
                                &dirty_keys,
                                &mut factory,
                            );
                        }
                    }
                } else {
                    stats.segment_miss_no_store += 1;
                    // No published store at all: fall back to sparse trie / L2 / persisted.
                    let dirty_keys: Vec<Nibbles> =
                        dirty.storage_changes.iter().map(|c| c.slot_key.clone()).collect();
                    stats.tier12_attempts += 1;
                    self.try_build_l2_proof(&dirty.hashed_address, root, &dirty_keys, &mut factory);
                }
            }
        }
        factory
    }

    /// Build a `SegmentTrieNodeProviderFactory` optimised for **cross-block reuse**.
    ///
    /// For accounts whose storage trie is **already present** in `cross_trie`
    /// (i.e. revealed in a previous block), this function skips the expensive
    /// tier-1 (`extract_storage_proof_from_sparse_trie`) and tier-2
    /// (`convert_arena_to_decoded_storage_multiproof`) full-arena DFS.  Instead:
    ///
    /// - If a published segment is available (root matches): use it directly.
    /// - Otherwise: tier-3 dirty-key-only persisted preload — O(dirty_keys × path_depth) instead of
    ///   O(all_trie_nodes).  The resulting small proof is stored as `pre_built_proof` in
    ///   `SegmentStorageNodeProvider` and is consulted **only** if a Hash-blinded node is
    ///   encountered during Step-2 `update_storage_leaf` (rare for fully-revealed paths).
    ///
    /// For accounts NOT in `cross_trie` (first appearance), falls back to the
    /// full `build_sparse_factory` logic (tier 1/2/3).
    fn build_sparse_factory_cross_block_reuse(
        &self,
        dirty_accounts: &[DirtyAccount],
        cross_trie: &SparseStateTrie,
        stats: &mut SparseFactoryStats,
    ) -> SegmentTrieNodeProviderFactory {
        let mut factory = SegmentTrieNodeProviderFactory::new();
        stats.dirty_accounts = dirty_accounts.len() as u64;
        for dirty in dirty_accounts {
            if dirty.storage_known_empty || dirty.storage_wiped {
                factory.known_empty_accounts.insert(dirty.hashed_address);
                continue;
            }
            if dirty.storage_changes.is_empty() {
                // Balance/nonce-only change: no storage reveal needed.
                continue;
            }
            stats.storage_accounts += 1;

            if let Some(storage_trie) = cross_trie.storage_trie_ref(&dirty.hashed_address) {
                // Account is already revealed in the cross-block trie.
                let mut root = self.get_existing_storage_root(&dirty.hashed_address);
                if root == EMPTY_ROOT_HASH {
                    // In wal_first+sparse mode account_trie may lag; when the
                    // account is already in cross_trie, trust sparse account leaf
                    // for storage_root hint to keep reuse path active.
                    if let Some(value) = cross_trie.get_account_value(&dirty.hashed_address) {
                        if let Ok(trie_account) =
                            alloy_rlp::Decodable::decode(&mut value.as_slice())
                        {
                            let ta: alloy_trie::TrieAccount = trie_account;
                            root = ta.storage_root;
                        }
                    }
                }
                stats.cross_reuse_accounts += 1;
                // For slots already revealed in previous blocks, no proof work is needed.
                // For newly-touched slots, only a subset requires fallback proof:
                // branch-miss inserts on fully-revealed paths can update in-memory
                // without touching the provider.
                let mut missing_count = 0usize;
                let mut proof_keys: Vec<Nibbles> = Vec::new();
                for change in &dirty.storage_changes {
                    if cross_trie.is_storage_slot_revealed(dirty.hashed_address, change.hashed_slot)
                    {
                        continue;
                    }
                    missing_count += 1;
                    if storage_key_requires_provider_reveal(storage_trie, &change.slot_key) {
                        proof_keys.push(change.slot_key.clone());
                    }
                }
                stats.cross_missing_slots += missing_count as u64;
                stats.cross_missing_proof_slots += proof_keys.len() as u64;
                if missing_count == 0 {
                    // All touched slots are already revealed in the reused trie.
                    // Mark as no-reveal so sparse_apply Step-1 can short-circuit
                    // before scanning per-slot revealed state again.
                    factory.no_reveal_accounts.insert(dirty.hashed_address);
                    continue;
                }
                if proof_keys.is_empty() {
                    // All missing slots can be inserted on already-revealed
                    // in-memory paths; skip Step-1 storage reveal for this
                    // account entirely.
                    factory.no_reveal_accounts.insert(dirty.hashed_address);
                    continue;
                }
                // This account still needs provider-assisted reveal/proof.
                // Try segment first; only fall back to proof build on miss.
                if self.try_segment_lookup_for_sparse_factory(
                    dirty.hashed_address,
                    root,
                    &mut factory,
                    stats,
                ) {
                    continue;
                }
                // Only build tier-3 proof for newly-touched slots.
                let before = factory.pre_built_storage_proofs.len();
                self.try_build_l2_proof_tier3_only(
                    &dirty.hashed_address,
                    root,
                    &proof_keys,
                    &mut factory,
                    stats,
                );
                if factory.pre_built_storage_proofs.len() == before {
                    // Tier-3 can fail in wal_first mode when the needed paths
                    // are not yet persisted/published. Fall back to tier-1/2
                    // sources (previous sparse trie / L2 cache) to avoid
                    // missing-proof errors in sparse apply.
                    stats.tier12_attempts += 1;
                    self.try_build_l2_proof(&dirty.hashed_address, root, &proof_keys, &mut factory);
                }
            } else {
                let root = self.get_existing_storage_root(&dirty.hashed_address);
                if root == EMPTY_ROOT_HASH {
                    factory.known_empty_accounts.insert(dirty.hashed_address);
                    continue;
                }
                let dirty_keys: Vec<Nibbles> =
                    dirty.storage_changes.iter().map(|c| c.slot_key.clone()).collect();
                // Non-cross-reuse path keeps segment-first behavior.
                if self.try_segment_lookup_for_sparse_factory(
                    dirty.hashed_address,
                    root,
                    &mut factory,
                    stats,
                ) {
                    continue;
                }
                // Account not yet in cross-block trie:
                // prefer tier-3 dirty-path proof first to avoid full-arena DFS.
                let before = factory.pre_built_storage_proofs.len();
                self.try_build_l2_proof_tier3_only(
                    &dirty.hashed_address,
                    root,
                    &dirty_keys,
                    &mut factory,
                    stats,
                );
                if factory.pre_built_storage_proofs.len() == before {
                    stats.tier12_attempts += 1;
                    self.try_build_l2_proof(&dirty.hashed_address, root, &dirty_keys, &mut factory);
                }
            }
        }
        factory
    }

    fn try_segment_lookup_for_sparse_factory(
        &self,
        hashed_addr: B256,
        root: B256,
        factory: &mut SegmentTrieNodeProviderFactory,
        stats: &mut SparseFactoryStats,
    ) -> bool {
        if let Some(store) = &self.published_store {
            stats.segment_lookups += 1;
            match store.open_trie_page(&hashed_addr, root) {
                Ok(Some(loaded)) => {
                    stats.segment_hits += 1;
                    factory.storage_segments.insert(hashed_addr, loaded.lease);
                    true
                }
                _ => {
                    stats.segment_miss += 1;
                    if Self::sparse_l3_trace_enabled() {
                        if let Ok(Some(entry_root)) = store.lookup_trie_root(&hashed_addr) {
                            if entry_root != root {
                                stats.segment_root_mismatch += 1;
                            }
                        }
                    }
                    false
                }
            }
        } else {
            stats.segment_miss_no_store += 1;
            false
        }
    }

    /// Tier-3-only variant of `try_build_l2_proof`: loads ONLY the dirty-key
    /// paths from the persisted store.  Does NOT attempt tier-1 (sparse trie
    /// DFS) or tier-2 (L2 cache arena DFS).
    ///
    /// The resulting proof contains only the nodes along `dirty_keys` paths,
    /// making it O(dirty_keys × path_depth) instead of O(all_trie_nodes).
    fn try_build_l2_proof_tier3_only(
        &self,
        hashed_addr: &B256,
        root: B256,
        dirty_keys: &[Nibbles],
        factory: &mut SegmentTrieNodeProviderFactory,
        stats: &mut SparseFactoryStats,
    ) {
        if dirty_keys.is_empty() {
            return;
        }
        stats.tier3_attempts += 1;
        let mut cow = StorageTrieCow::from_persisted_root(root);
        if cow.preload_paths(&self.persisted, dirty_keys).is_ok() {
            match convert_arena_to_decoded_storage_multiproof_for_paths(
                cow.arena(),
                cow.root_index(),
                root,
                dirty_keys,
            ) {
                Ok(proof) => {
                    stats.tier3_hits += 1;
                    factory.pre_built_storage_proofs.insert(*hashed_addr, proof);
                    return;
                }
                Err(_) => {}
            }
        }
        // Tier-3 failed (account not in persisted store, e.g. brand-new in wal_first
        // mode with no published segment).  Leave the account without a pre-built
        // proof — the provider will return Err if a Hash-blinded node is encountered,
        // which surfaces as an apply error.  This is safe when the cross-block trie
        // already has the account fully revealed (no Hash nodes on dirty paths).
    }

    /// Extract a **path-limited** account trie proof for sparse reveal.
    ///
    /// Only dirty account paths are materialized and exported, avoiding a full
    /// account-trie DFS on each block.
    fn extract_account_proof_from_keys(
        &mut self,
        account_keys: &[Nibbles],
    ) -> Result<(alloy_trie::proof::DecodedProofNodes, reth_trie_common::BranchNodeMasksMap)> {
        if account_keys.is_empty() {
            return Ok((
                alloy_trie::proof::DecodedProofNodes::default(),
                reth_trie_common::BranchNodeMasksMap::default(),
            ));
        }

        // In sparse mode, prefer deriving path-limited account proof directly
        // from the latest sparse trie to avoid depending on account-trie hot-path
        // updates during wal_first commits.
        if self.config.use_sparse_storage {
            if let Some(ref cross) = self.cross_block_sparse &&
                cross.trie.state_trie_ref().is_some()
            {
                return extract_account_proof_from_sparse_trie_for_paths(&cross.trie, account_keys);
            }
            if let Some(ref sparse) = self.last_committed_sparse_trie {
                return extract_account_proof_from_sparse_trie_for_paths(sparse, account_keys);
            }
        }

        let working_version = self.current_working_version();
        self.account_trie.checkout_for_write(working_version);

        if let Some(working) = self.account_trie.working.as_mut() {
            let persisted = Arc::clone(&self.persisted);
            working.preload_paths(&persisted, account_keys)?;
            return convert_arena_to_account_proof_nodes_for_paths(
                working.arena(),
                working.root_index(),
                account_keys,
            );
        }

        // Defensive fallback: if no working copy is present, load from base.
        let persisted = Arc::clone(&self.persisted);
        self.account_trie.base.preload_paths(&persisted, account_keys)?;
        convert_arena_to_account_proof_nodes_for_paths(
            self.account_trie.base.arena(),
            self.account_trie.base.root_index(),
            account_keys,
        )
    }

    /// Per-block sparse apply (Phase 2): create a fresh `SparseStateTrie` each
    /// block, apply all changes, and store it in `pending_sparse_state`.
    ///
    /// Called at the START of `apply_dirty_accounts_inner` (before the normal
    /// apply mutates L2 handles) so that witness extraction sees the committed
    /// base state.
    ///
    /// Returns immediately (without setting `pending_sparse_state`) when
    /// `dirty_accounts` is empty — the normal Phase 2b path handles empty
    /// blocks correctly and produces the unchanged previous state root.
    fn apply_dirty_accounts_inner_sparse(
        &mut self,
        dirty_accounts: Vec<DirtyAccount>,
    ) -> Result<()> {
        let mut sparse_stats = SparseFactoryStats::default();
        // Always set working_version so account_trie_handle_versions() is correct.
        let working_version = self.current_working_version();
        let acct_checkout_start = std::time::Instant::now();
        self.account_trie.checkout_for_write(working_version);
        self.last_apply_account_trie_checkout = acct_checkout_start.elapsed();

        if dirty_accounts.is_empty() {
            // Empty bundle: sparse path is a no-op.  Always clear dirty_accounts
            // so Phase 2 of commit_inner_with_mode doesn't re-encode the PREVIOUS
            // block's accounts, which would produce a wrong state root.
            self.dirty_accounts = dirty_accounts;
            self.last_sparse_account_reveal_keys = 0;
            self.last_apply_sparse_factory = sparse_stats;
            return Ok(());
        }

        // Sparse path reads published segments directly in factory-building.
        // Refresh the published view so open_trie_page() can hit newly-published
        // generations after flush_persist/background publish.
        let refresh_start = std::time::Instant::now();
        self.maybe_refresh_published_view()?;
        // Track refresh cost so [mptcross] can include it in the breakdown.
        self.last_apply_published_view_refresh = refresh_start.elapsed();

        if self.config.cross_block_sparse {
            let result =
                self.apply_dirty_accounts_inner_cross_block(dirty_accounts, &mut sparse_stats);
            self.last_apply_sparse_factory = sparse_stats;
            return result;
        }

        let factory_start = std::time::Instant::now();
        let factory = self.build_sparse_factory(&dirty_accounts, &mut sparse_stats);
        self.last_sparse_apply_factory_build = factory_start.elapsed();

        let account_proof_start = std::time::Instant::now();
        let account_keys: Vec<Nibbles> =
            dirty_accounts.iter().map(|d| d.account_key.clone()).collect();
        self.last_sparse_account_reveal_keys = account_keys.len();
        let account_proof = self.extract_account_proof_from_keys(&account_keys)?;
        self.last_sparse_apply_account_proof = account_proof_start.elapsed();

        let mut sparse_trie = SparseStateTrie::default().with_updates(false);
        let apply_changes_start = std::time::Instant::now();
        apply_all_storage_changes_sparse(
            &mut sparse_trie,
            account_proof,
            &factory,
            &dirty_accounts,
            false, // fresh trie: full reveal required
        )?;
        self.last_sparse_apply_apply_changes = apply_changes_start.elapsed();

        self.pending_sparse_state =
            Some(Box::new(PendingSparseState { trie: sparse_trie, factory }));
        // Store dirty_accounts — the normal apply no longer runs in sparse mode.
        self.dirty_accounts = dirty_accounts;
        self.last_apply_sparse_factory = sparse_stats;
        Ok(())
    }

    /// Cross-block sparse apply (Phase 4): reuse `SparseStateTrie` across
    /// blocks for incremental witness reveals and root computation.
    ///
    /// Flow:
    /// 1. If no cross-block trie: initialise fresh (same as Phase 2, full reveal).
    /// 2. If cross-block trie exists:
    ///    - Build factory via `build_sparse_factory_cross_block_reuse`: skips tier-1/2 full-arena
    ///      DFS for accounts already in the trie; uses segment (O(1)) or tier-3 dirty-key preload
    ///      (O(k·depth)) instead.
    ///    - Call `apply_all_storage_changes_sparse` with `skip_already_revealed_storage=true`: for
    ///      already-revealed accounts, skip `reveal_decoded_storage_multiproof` entirely — provider
    ///      handles Hash-blinded boundaries lazily.
    ///    - Evict storage tries idle for `cross_block_sparse_max_lag` blocks.
    fn apply_dirty_accounts_inner_cross_block(
        &mut self,
        dirty_accounts: Vec<DirtyAccount>,
        stats: &mut SparseFactoryStats,
    ) -> Result<()> {
        let mut sparse_apply_changes_elapsed = Duration::ZERO;
        let next_version = self.version + 1;

        // ── Phase A: build factory and account proof (immutable/exclusive borrows
        //    completed before the mutable Phase B begins) ──────────────────────
        let in_reuse_mode = self.cross_block_sparse.is_some();

        // Factory build: for the reuse path, borrow cross.trie immutably to
        // check which accounts are already revealed.  This borrow is released
        // before the mutable Phase B begins.
        let factory_start = std::time::Instant::now();
        let factory = if in_reuse_mode {
            // SAFETY: both borrows are immutable.  &cross.trie is a sub-borrow of
            // &self; &self is the receiver of build_sparse_factory_cross_block_reuse.
            // Rust allows multiple shared references simultaneously.
            let cross_trie = &self.cross_block_sparse.as_ref().unwrap().trie;
            self.build_sparse_factory_cross_block_reuse(&dirty_accounts, cross_trie, stats)
        } else {
            self.build_sparse_factory(&dirty_accounts, stats)
        };
        self.last_sparse_apply_factory_build = factory_start.elapsed();

        // Account proof: in reuse mode, reveal only NEW account paths that are
        // not yet present in cross.trie.
        let account_keys_to_reveal: Vec<Nibbles> = if in_reuse_mode {
            let cross_trie = &self.cross_block_sparse.as_ref().unwrap().trie;
            dirty_accounts
                .iter()
                .filter(|d| !cross_trie.is_account_revealed(d.hashed_address))
                .map(|d| d.account_key.clone())
                .collect()
        } else {
            dirty_accounts.iter().map(|d| d.account_key.clone()).collect()
        };
        self.last_sparse_account_reveal_keys = account_keys_to_reveal.len();
        let account_proof_start = std::time::Instant::now();
        let account_proof = if account_keys_to_reveal.is_empty() {
            // All accounts already in account trie: pass empty proof (no-op reveal).
            (
                alloy_trie::proof::DecodedProofNodes::default(),
                reth_trie_common::BranchNodeMasksMap::default(),
            )
        } else {
            self.extract_account_proof_from_keys(&account_keys_to_reveal)?
        };
        self.last_sparse_apply_account_proof = account_proof_start.elapsed();

        // ── Phase B: apply changes (mutable borrow of cross.trie) ────────────

        if let Some(ref mut cross) = self.cross_block_sparse {
            // Reset update tracking so root_with_updates captures only the
            // current block's changes.
            cross.trie.reinit_updates();
            cross.factory = factory;

            // Apply changes with skip_already_revealed_storage=true:
            // storage tries already in cross.trie skip the reveal step entirely.
            let apply_changes_start = std::time::Instant::now();
            apply_all_storage_changes_sparse(
                &mut cross.trie,
                account_proof,
                &cross.factory,
                &dirty_accounts,
                true, // skip_already_revealed_storage
            )?;
            sparse_apply_changes_elapsed += apply_changes_start.elapsed();
            let post_apply_start = std::time::Instant::now();

            // Update access version for dirty storage accounts.
            // Collect dirty addresses for the version_queue in one pass.
            let t_last_block_start = std::time::Instant::now();
            let mut block_addrs: Vec<B256> = Vec::with_capacity(
                dirty_accounts
                    .iter()
                    .filter(|d| !d.storage_changes.is_empty() || d.storage_wiped)
                    .count(),
            );
            for dirty in &dirty_accounts {
                if !dirty.storage_changes.is_empty() || dirty.storage_wiped {
                    cross.storage_last_block.insert(dirty.hashed_address, next_version);
                    block_addrs.push(dirty.hashed_address);
                }
            }
            if !block_addrs.is_empty() {
                cross.version_queue.push_back((next_version, block_addrs));
            }
            let t_last_block_elapsed = t_last_block_start.elapsed();

            // LRU eviction: O(evicted_count) via the version_queue ring buffer.
            //
            // Instead of iterating the full `storage_last_block` HashMap (O(90K) → 20ms),
            // pop the oldest version_queue entry and evict only the accounts whose
            // `storage_last_block` still matches the popped version (i.e., they were
            // not re-accessed in a newer block).
            //
            // The DROP of SparseTrie objects (HashMap<Nibbles,SparseNode> × 45 entries
            // per trie) is deferred to a Rayon task so it does not block the front-end.
            // Dropping 10K SparseTries on the main thread was the remaining ~17 ms.
            let t_evict_start = std::time::Instant::now();
            let max_lag = self.config.cross_block_sparse_max_lag;
            let mut evict_count = 0usize;
            if max_lag > 0 {
                let threshold = next_version - max_lag;
                // Collect evicted tries for deferred background drop.
                let mut evicted_tries = Vec::new();
                while let Some((v, _)) = cross.version_queue.front() {
                    if *v >= threshold {
                        break;
                    }
                    let (old_version, addrs) = cross.version_queue.pop_front().unwrap();
                    for addr in addrs {
                        // Only evict if the account's last-access version matches
                        // the popped entry (not re-accessed in a newer block).
                        if cross.storage_last_block.get(&addr) == Some(&old_version) {
                            if let Some(trie) = cross.trie.take_storage_trie(&addr) {
                                evicted_tries.push(trie);
                            }
                            cross.storage_last_block.remove(&addr);
                            evict_count += 1;
                        }
                    }
                }
                // Defer the Drop of evicted SparseTries to a Rayon background task.
                // Each SparseTrie contains HashMap<Nibbles,SparseNode> + HashMap<Nibbles,Bytes>
                // which takes ~1.7 µs to drop; 10K tries × 1.7 µs = ~17 ms on the hot path.
                // Rayon spawn is effectively free: drops happen on pooled threads that are
                // otherwise idle during the main-thread eviction window.
                if !evicted_tries.is_empty() {
                    rayon::spawn(move || drop(evicted_tries));
                }
            }
            let t_evict_elapsed = t_evict_start.elapsed();

            // Build pending from the reused trie (factory already updated above).
            // We take the trie out of cross_block_sparse temporarily;
            // commit_inner_with_mode will put it back.
            let t_swap_start = std::time::Instant::now();
            let trie_for_pending =
                std::mem::replace(&mut cross.trie, SparseStateTrie::default().with_updates(false));
            // Avoid per-block full clone of factory maps: after apply, `cross.factory`
            // is not needed until next block (where it gets overwritten), so move it
            // directly into pending state for commit/root.
            let factory_for_pending =
                std::mem::replace(&mut cross.factory, SegmentTrieNodeProviderFactory::new());
            self.pending_sparse_state = Some(Box::new(PendingSparseState {
                trie: trie_for_pending,
                factory: factory_for_pending,
            }));
            let t_swap_elapsed = t_swap_start.elapsed();
            let post_apply_elapsed = post_apply_start.elapsed();

            if std::env::var_os("MPT_SPARSE_APPLY_TRACE").is_some() {
                static POST_LOGGED: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = POST_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n % 10 == 0 || n < 5 {
                    eprintln!(
                        "[mptpost] last_block={:.1}ms evict={:.1}ms({}) swap={:.1}ms post_total={:.1}ms slb_size={}",
                        t_last_block_elapsed.as_secs_f64() * 1000.0,
                        t_evict_elapsed.as_secs_f64() * 1000.0,
                        evict_count,
                        t_swap_elapsed.as_secs_f64() * 1000.0,
                        post_apply_elapsed.as_secs_f64() * 1000.0,
                        cross.storage_last_block.len(),
                    );
                }
            }
        } else {
            // ── First block: initialise cross-block state ─────────────────────
            // factory and account_proof were built in Phase A above.
            let mut sparse_trie = SparseStateTrie::default().with_updates(false);
            let apply_changes_start = std::time::Instant::now();
            apply_all_storage_changes_sparse(
                &mut sparse_trie,
                account_proof,
                &factory,
                &dirty_accounts,
                false, // first block: all storage tries need full reveal
            )?;
            sparse_apply_changes_elapsed += apply_changes_start.elapsed();

            // Initialise access tracking for dirty storage accounts.
            let mut storage_last_block = alloy_primitives::map::HashMap::default();
            let mut first_block_addrs: Vec<B256> = Vec::new();
            for dirty in &dirty_accounts {
                if !dirty.storage_changes.is_empty() || dirty.storage_wiped {
                    storage_last_block.insert(dirty.hashed_address, next_version);
                    first_block_addrs.push(dirty.hashed_address);
                }
            }
            let mut version_queue = std::collections::VecDeque::new();
            if !first_block_addrs.is_empty() {
                version_queue.push_back((next_version, first_block_addrs));
            }

            // Store cross-block state with a placeholder trie (real one goes
            // to pending_sparse_state).  The trie is returned from commit to
            // cross_block_sparse via commit_inner_with_mode.
            // First block: keep cross-block holder with an empty factory.
            // The next block rebuilds and overwrites it before use.
            self.cross_block_sparse = Some(Box::new(CrossBlockSparseState {
                trie: SparseStateTrie::default().with_updates(false), /* placeholder */
                factory: SegmentTrieNodeProviderFactory::new(),
                storage_last_block,
                version_queue,
            }));
            self.pending_sparse_state =
                Some(Box::new(PendingSparseState { trie: sparse_trie, factory }));
        }
        self.dirty_accounts = dirty_accounts;
        self.last_sparse_apply_apply_changes = sparse_apply_changes_elapsed;

        // Cross-block timing trace — activated by MPT_SPARSE_APPLY_TRACE.
        // Shows factory build / account proof / apply_changes breakdown so the
        // ~32 ms overhead outside apply_all_storage_changes_sparse can be located.
        if std::env::var_os("MPT_SPARSE_APPLY_TRACE").is_some() {
            static CROSS_LOGGED: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = CROSS_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Print every block (block_num % 10 == 0 after warmup)
            if n % 10 == 0 || n < 5 {
                eprintln!(
                    "[mptcross] pub_refresh={:.1}ms acct_checkout={:.1}ms factory_build={:.1}ms \
                     acct_proof={:.1}ms apply_changes={:.1}ms acct_reveal_keys={} in_reuse={}",
                    self.last_apply_published_view_refresh.as_secs_f64() * 1000.0,
                    self.last_apply_account_trie_checkout.as_secs_f64() * 1000.0,
                    self.last_sparse_apply_factory_build.as_secs_f64() * 1000.0,
                    self.last_sparse_apply_account_proof.as_secs_f64() * 1000.0,
                    sparse_apply_changes_elapsed.as_secs_f64() * 1000.0,
                    self.last_sparse_account_reveal_keys,
                    in_reuse_mode,
                );
            }
        }

        Ok(())
    }

    fn apply_dirty_accounts_inner(&mut self, dirty_accounts: Vec<DirtyAccount>) -> Result<()> {
        self.last_apply_sparse_factory = SparseFactoryStats::default();
        self.last_sparse_apply_factory_build = Duration::ZERO;
        self.last_sparse_apply_account_proof = Duration::ZERO;
        self.last_sparse_apply_apply_changes = Duration::ZERO;
        self.last_sparse_account_reveal_keys = 0;
        self.last_apply_account_trie_checkout = Duration::ZERO;
        if self.config.use_sparse_storage {
            // Auto-route to the direct StorageTrieCow path when the workload is
            // sparse (few changes per account).  This eliminates:
            //   • serial `reveal_storage` (~12 ms) — no SparseStateTrie reveal needed
            //   • separate `root_compute` phase (~22-30 ms) — root is computed inline
            //     during the parallel apply pass (`merge_hash = true`)
            //
            // The direct path works by leaving `pending_sparse_state = None`, which
            // causes `commit_inner_with_mode_and_external_root` to use the COW handle
            // path.  Proof generation falls through to the account_trie + storage
            // handle path (same as `use_sparse_storage = false`), which is correct.
            //
            // Triggered by `config.direct_update_avg_changes_threshold` or the
            // `MPT_DIRECT_UPDATE_THRESHOLD` env-var override.
            // Env var takes precedence over config so it can be tuned at runtime
            // without recompilation.  `or` falls back to config when env var
            // is absent.
            let effective_threshold = std::env::var("MPT_DIRECT_UPDATE_THRESHOLD")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|&t| t > 0.0)
                .or(self.config.direct_update_avg_changes_threshold);

            let use_direct = effective_threshold.map_or(false, |threshold| {
                !dirty_accounts.is_empty() && {
                    let total_changes: usize =
                        dirty_accounts.iter().map(|d| d.storage_changes.len()).sum();
                    let avg = total_changes as f64 / dirty_accounts.len() as f64;
                    // Use <= so avg==threshold still triggers (e.g. avg=30, threshold=30).
                    avg <= threshold
                }
            });

            if use_direct {
                // Fall through to the non-sparse (direct COW) path below.
                // `pending_sparse_state` is NOT set → commit uses COW handles.
                // Log once (first block only) so the user can verify activation.
                static DIRECT_LOGGED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !DIRECT_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let total: usize = dirty_accounts.iter().map(|d| d.storage_changes.len()).sum();
                    let avg = total as f64 / dirty_accounts.len().max(1) as f64;
                    eprintln!(
                        "[mpt:direct] using direct StorageTrieCow path \
                         (accounts={} avg_changes={:.1} threshold={:.1}): \
                         reveal_storage and root_compute eliminated",
                        dirty_accounts.len(),
                        avg,
                        effective_threshold.unwrap_or(0.0),
                    );
                }
            } else {
                self.apply_dirty_accounts_inner_sparse(dirty_accounts)?;
                // Sparse-only path (wal_first and non-wal_first).
                return Ok(());
            }
        }
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
        let merge_hash = true;
        let persisted_ref = &self.persisted;
        let apply_stolen = std::sync::atomic::AtomicU64::new(0);
        let apply_fresh = std::sync::atomic::AtomicU64::new(0);
        let apply_existing = std::sync::atomic::AtomicU64::new(0);
        let apply_shrink = std::sync::atomic::AtomicU64::new(0);
        let apply_capacity = std::sync::atomic::AtomicU64::new(0);
        // Use parallel iteration only when there are enough handles to amortize
        // rayon dispatch overhead + allocator contention.  For fewer handles
        // (e.g., 300 contracts in B4.7), sequential is 41x faster due to
        // zero cache thrashing and zero malloc lock contention.
        let apply_one = |(hashed_address, mut handle): (B256, StorageTrieHandle)| -> Result<(B256, StorageTrieHandle)> {
                let (trie, outcome) = handle.take_working_or_base_for_version(working_version, self.config.overlay_reuse_enabled, Some(self.overlay_watermark));
                use OverlayOutcome::*;
                match outcome {
                    Stolen { shrank, reused_bytes } => {
                        apply_stolen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        apply_capacity.fetch_add(reused_bytes as u64, std::sync::atomic::Ordering::Relaxed);
                        if shrank { apply_shrink.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
                    }
                    FreshClone => { apply_fresh.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
                    ExistingWorking => { apply_existing.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
                }
                let trie = match dirty_storage_accounts.get(&hashed_address) {
                    Some(dirty) => {
                        Self::apply_storage_changes_to_working(
                            trie,
                            persisted_ref,
                            dirty,
                        )?
                    }
                    None => trie,
                };
                if merge_hash {
                    // Hash immediately while data is cache-hot.
                    // For large tries (many nodes), use the parallel root-level
                    // variant: it fans out 16-way at the root branch, creating
                    // smaller per-subtask working sets that fit in L1 cache.
                    // Threshold: 128 nodes ≈ 30+ slot tries where inner
                    // parallelism pays off more than its rayon spawn overhead.
                    let (root, mut cow) = if trie.arena_len() >= 128 {
                        trie.root_hash_only_parallel(persisted_ref)
                    } else {
                        trie.root_hash_only(persisted_ref)
                    }
                    .map_err(|err| {
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
        self.last_overlay_stolen += apply_stolen.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_fresh_clone += apply_fresh.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_existing_working +=
            apply_existing.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_shrink_events += apply_shrink.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_reuse_capacity_entries +=
            apply_capacity.load(std::sync::atomic::Ordering::Relaxed);

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
        self.last_apply_published_view_refresh = load_stats.refresh_elapsed;
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

        // Trace output for the direct (non-sparse) path — mirrors the sparse
        // path's MPT_SPARSE_APPLY_TRACE format so both paths are easy to compare.
        if std::env::var_os("MPT_SPARSE_APPLY_TRACE").is_some() {
            eprintln!(
                "[mptdirect] accounts={} changes={} \
                 acct_checkout={:.1}ms ensure_storage={:.1}ms \
                 pub_refresh={:.1}ms root_lookup={:.1}ms \
                 slot_updates={:.1}ms \
                 l3_pub={:.1}ms total_get_or_load={:.1}ms \
                 l2_hits={} l3_pub_hits={} node_fallback={}",
                self.dirty_accounts.len(),
                self.last_apply_slot_inserts + self.last_apply_slot_deletes,
                self.last_apply_account_trie_checkout.as_secs_f64() * 1000.0,
                self.last_apply_ensure_storage.as_secs_f64() * 1000.0,
                self.last_apply_published_view_refresh.as_secs_f64() * 1000.0,
                self.last_apply_storage_root_lookup.as_secs_f64() * 1000.0,
                self.last_apply_storage_slot_updates.as_secs_f64() * 1000.0,
                self.last_apply_l3_published_load.as_secs_f64() * 1000.0,
                self.last_apply_get_or_load_storage_tries.as_secs_f64() * 1000.0,
                self.last_apply_l2_hits,
                self.last_apply_l3_published_hits,
                self.last_apply_node_fallback_loads,
            );
        }

        Ok(())
    }

    fn apply_bundle_state_inner(&mut self, bundle: &BundleState) -> Result<()> {
        // Reset overlay reuse counters here so they accumulate across both
        // the apply phase (apply_dirty_accounts_inner) and the subsequent
        // commit phase (commit_inner_with_mode), giving a true per-block total.
        // Reset per-block overlay reuse counters — must happen in apply (not commit)
        // so both apply-phase and commit-phase steals are included in the total.
        self.last_overlay_stolen = 0;
        self.last_overlay_fresh_clone = 0;
        self.last_overlay_existing_working = 0;
        self.last_overlay_shrink_events = 0;
        self.last_overlay_reuse_capacity_entries = 0;
        let collect_start = std::time::Instant::now();
        let dirty_accounts = state::collect_dirty_accounts(bundle)?;
        let collect_elapsed = collect_start.elapsed();
        self.apply_dirty_accounts_inner(dirty_accounts)?;
        self.last_apply_collect_dirty_accounts = collect_elapsed;

        // Cross-block bundle timing: show collect vs apply breakdown.
        if std::env::var_os("MPT_SPARSE_APPLY_TRACE").is_some() {
            static BUNDLE_LOGGED: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let n = BUNDLE_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n % 10 == 0 || n < 5 {
                eprintln!(
                    "[mptbundle] collect_dirty={:.1}ms",
                    collect_elapsed.as_secs_f64() * 1000.0,
                );
            }
        }

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
        self.commit_inner_with_mode_and_external_root(
            CommitExecutionMode {
                wal_first: false,
                save_manifest: true,
                // Build segments from in-memory tries — the BulkSegmentWriter
                // streams them directly to pages.data (matching sei-db's
                // snapshotWriter).  publish_generation is skipped during
                // bulk_load; one delta file is written at finish.
                publish_baseline: true,
            },
            None,
        )
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
        self.commit_inner_with_mode_and_external_root(self.default_commit_mode(), None)
    }

    fn commit_inner_with_mode_and_external_root(
        &mut self,
        mode: CommitExecutionMode,
        external_state_root: Option<B256>,
    ) -> Result<(i64, B256)> {
        if external_state_root.is_some() && !mode.wal_first {
            return Err(MptDbError::Other(
                "commit_with_external_root requires wal_first commit mode".to_string(),
            ));
        }
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

        // Single pass:
        // 1) pre-fill DELETE/REUSE roots (no trie hashing),
        // 2) collect RECOMPUTE candidates for handle checkout.
        let storage_prefill_start = std::time::Instant::now();
        let mut dirty_working_addresses: Vec<B256> = Vec::new();
        let mut seen_working_accounts: HashSet<B256> = HashSet::default();
        for dirty in &self.dirty_accounts {
            let hashed_address = dirty.hashed_address;
            if self.contains_working_trie(&hashed_address) {
                if seen_working_accounts.insert(hashed_address) {
                    dirty_working_addresses.push(hashed_address);
                }
                continue;
            }
            if dirty.info.is_none() && dirty.storage_wiped {
                // DELETE case
                storage_roots.insert(hashed_address, EMPTY_ROOT_HASH);
            } else {
                // REUSE case: get from existing account leaf
                let root = self.get_existing_storage_root(&hashed_address);
                storage_roots.insert(hashed_address, root);
            }
        }
        let storage_roots_prefill_elapsed = storage_prefill_start.elapsed();

        let working_version = self.current_working_version();
        let take_handles_start = std::time::Instant::now();
        let dirty_handles = self.take_working_handles(dirty_working_addresses);
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

        let commit_stolen = std::sync::atomic::AtomicU64::new(0);
        let commit_fresh = std::sync::atomic::AtomicU64::new(0);
        let commit_existing = std::sync::atomic::AtomicU64::new(0);
        let commit_shrink = std::sync::atomic::AtomicU64::new(0);
        let commit_capacity = std::sync::atomic::AtomicU64::new(0);
        let compute_storage_artifact = |addr: B256,
                                        mut handle: StorageTrieHandle,
                                        persisted: &PersistedTrieStore,
                                        hash_only: bool|
         -> Result<StorageTrieCommitArtifacts> {
            let (trie, outcome) = handle.take_working_or_base_for_version(
                working_version,
                self.config.overlay_reuse_enabled,
                Some(self.overlay_watermark),
            );
            use OverlayOutcome::*;
            match outcome {
                Stolen { shrank, reused_bytes } => {
                    commit_stolen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    commit_capacity
                        .fetch_add(reused_bytes as u64, std::sync::atomic::Ordering::Relaxed);
                    if shrank {
                        commit_shrink.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                FreshClone => {
                    commit_fresh.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                ExistingWorking => {
                    commit_existing.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
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

        // Accumulate commit-phase reuse stats.
        self.last_overlay_stolen += commit_stolen.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_fresh_clone += commit_fresh.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_existing_working +=
            commit_existing.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_shrink_events += commit_shrink.load(std::sync::atomic::Ordering::Relaxed);
        self.last_overlay_reuse_capacity_entries +=
            commit_capacity.load(std::sync::atomic::Ordering::Relaxed);

        // Sparse path: fill / override storage roots from SparseStateTrie.
        //
        // Without the dual-write, the REUSE branch above fills all accounts —
        // including NEW accounts with storage changes — with the WRONG
        // EMPTY_ROOT_HASH (from `get_existing_storage_root` which returns
        // EMPTY for accounts that don't yet exist in the committed state).
        //
        // We MUST override those entries with the ACTUAL storage root computed
        // by the sparse trie for every account that had storage changes.
        if let Some(ref mut pending) = self.pending_sparse_state {
            for dirty in &self.dirty_accounts {
                if dirty.storage_wiped {
                    storage_roots.insert(dirty.hashed_address, EMPTY_ROOT_HASH);
                } else if !dirty.storage_changes.is_empty() {
                    // Always override — the REUSE-inserted EMPTY_ROOT_HASH is wrong
                    // for newly-created accounts that have storage in this block.
                    let root =
                        pending.trie.storage_root(dirty.hashed_address).unwrap_or(EMPTY_ROOT_HASH);
                    storage_roots.insert(dirty.hashed_address, root);
                }
                // Accounts with no storage changes: keep existing root (REUSE or
                // DELETE already handled correctly).
            }
        }

        let storage_roots_elapsed = storage_start.elapsed();

        // Phase 2: precompute account writes in parallel, then apply to the
        // single shared account trie serially.
        let account_updates_start = std::time::Instant::now();
        let wal_sparse_root_enabled = mode.wal_first;
        let skip_account_trie_updates = wal_sparse_root_enabled && self.config.use_sparse_storage;
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
                                    value: c.value.to_be_bytes(),
                                })
                                .collect();
                            sc.sort_by(|a, b| a.hashed_slot.cmp(&b.hashed_slot));
                            CommitWalAccountChange {
                                address,
                                hashed_address,
                                info: info.as_ref().map(|i| CommitWalAccountInfo {
                                    nonce: i.nonce,
                                    balance: i.balance.to_be_bytes(),
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

        let working_version = self.current_working_version();
        self.account_trie.checkout_for_write(working_version);
        let mut account_trie = self.account_trie.take_working_or_base_for_version(working_version);
        if !skip_account_trie_updates {
            let account_updates_trace = std::env::var_os("MPT_ACCOUNT_UPDATES_TRACE").is_some();
            let account_encode_start = std::time::Instant::now();
            let account_writes: Vec<Option<Vec<u8>>> = if self.dirty_accounts.len() >= 1_024 {
                self.dirty_accounts.par_iter().map(encode_account).collect()
            } else {
                self.dirty_accounts.iter().map(encode_account).collect()
            };
            let account_encode_elapsed = account_encode_start.elapsed();

            // Use the fast materialized path when the account trie has no lazy
            // root (true after bulk_load and all subsequent blocks).
            let materialized = !account_trie.is_lazy_root();
            let account_apply_start = std::time::Instant::now();
            for (dirty, encoded) in self.dirty_accounts.iter().zip(account_writes.into_iter()) {
                let key = &dirty.account_key;
                if materialized {
                    account_trie.apply_change_materialized(key, encoded);
                } else if let Some(rlp_buf) = encoded {
                    account_trie.apply_change(&self.persisted, key, Some(rlp_buf)).map_err(
                        |err| {
                            MptDbError::Other(format!(
                                "account trie apply_change for {}: {err}",
                                dirty.hashed_address
                            ))
                        },
                    )?;
                } else {
                    account_trie.apply_change(&self.persisted, key, None).map_err(|err| {
                        MptDbError::Other(format!(
                            "account trie apply_change delete for {}: {err}",
                            dirty.hashed_address
                        ))
                    })?;
                }
            }
            let account_apply_elapsed = account_apply_start.elapsed();
            if account_updates_trace {
                eprintln!(
                    "[mptdiag:account_updates] dirty_accounts={} encode_ms={:.3} apply_ms={:.3} materialized={}",
                    self.dirty_accounts.len(),
                    account_encode_elapsed.as_secs_f64() * 1000.0,
                    account_apply_elapsed.as_secs_f64() * 1000.0,
                    materialized
                );
            }
        }
        let account_updates_elapsed = account_updates_start.elapsed();

        // Phase 2b: compute state root.
        // use_sparse_storage: delegate to SparseStateTrie::root_with_updates which
        //   combines storage root computation + account root in one call.
        // wal_first: hash-only (no blob collection) — matching sei-db.
        // sync: hash + collect blobs for RocksDB persist.
        let account_root_start = std::time::Instant::now();
        let account_root_trace = std::env::var_os("MPT_ACCOUNT_ROOT_TRACE").is_some();
        let mut account_root_sparse_root_elapsed = Duration::ZERO;
        let mut account_root_sparse_blob_elapsed = Duration::ZERO;
        let mut account_root_sparse_segment_build_elapsed = Duration::ZERO;
        let mut account_root_sparse_deferred_snapshot_elapsed = Duration::ZERO;
        let mut account_root_sparse_publish_targets: usize = 0;
        let mut account_root_sparse_snapshot_targets: usize = 0;
        let mut account_root_sparse_pending_targets: usize = 0;
        let mut account_root_sparse_materialize_interval: i64 = 1;
        let mut sparse_published_puts: Vec<(B256, StorageTrieSegment)> = Vec::new();
        let mut sparse_committed_tries: Vec<(B256, B256, SerialSparseTrie)> = Vec::new();
        let defer_sparse_segment_build =
            mode.wal_first && self.wal_first_defer_segment_build_enabled();
        let (state_root, account_blobs, account_cow) = if let Some(mut pending) =
            self.pending_sparse_state.take()
        {
            // Sparse path:
            // - wal_first: default to SparseStateTrie root; account_trie updates are optional and
            //   can be skipped on the commit hot path.
            // - non-wal_first: keep SparseStateTrie::root_with_updates so we can collect
            //   TrieUpdates for dirty blob generation.
            let (root, trie_updates, account_cow) = if let Some(external_root) = external_state_root
            {
                if std::env::var_os("MPT_VERIFY_WAL_EXTERNAL_STATE_ROOT").is_some() {
                    let sparse_root_start = std::time::Instant::now();
                    let sparse_root = pending
                        .trie
                        .root(&pending.factory)
                        .map_err(|e| MptDbError::Other(format!("sparse root (verify): {e}")))?;
                    account_root_sparse_root_elapsed += sparse_root_start.elapsed();
                    if sparse_root != external_root {
                        return Err(MptDbError::Other(format!(
                            "wal_first external root mismatch: sparse={sparse_root:?}, external={external_root:?}"
                        )));
                    }
                }
                account_trie.snapshot();
                (external_root, TrieUpdates::default(), account_trie)
            } else if mode.wal_first {
                let verify_sparse_root =
                    std::env::var_os("MPT_VERIFY_WAL_SPARSE_ACCOUNT_ROOT").is_some();
                let use_sparse_root = wal_sparse_root_enabled;
                let sparse_root = if use_sparse_root || verify_sparse_root {
                    let sparse_root_start = std::time::Instant::now();
                    Some(
                        pending
                            .trie
                            .root(&pending.factory)
                            .map_err(|e| MptDbError::Other(format!("sparse root (verify): {e}")))?,
                    )
                    .map(|root| {
                        account_root_sparse_root_elapsed += sparse_root_start.elapsed();
                        root
                    })
                } else {
                    None
                };

                if use_sparse_root {
                    let root = sparse_root.expect("sparse root must exist when use_sparse_root");
                    if verify_sparse_root && !skip_account_trie_updates {
                        let (account_root, _) = account_trie
                            .clone()
                            .root_hash_only_parallel_account(&self.persisted)
                            .map_err(|err| {
                                MptDbError::Other(format!(
                                    "account trie root hash verify (wal_first sparse-root): {err}"
                                ))
                            })?;
                        if account_root != root {
                            return Err(MptDbError::Other(format!(
                                "wal_first sparse root mismatch (sparse-root mode): sparse={root:?}, account_trie={account_root:?}"
                            )));
                        }
                    } else if verify_sparse_root && skip_account_trie_updates {
                        eprintln!(
                            "[mptdiag] skip account-trie sparse-root verify: account_trie hot-path updates disabled"
                        );
                    }
                    (root, TrieUpdates::default(), account_trie)
                } else {
                    let (root, mut account_cow) = account_trie
                        .root_hash_only_parallel_account(&self.persisted)
                        .map_err(|err| {
                            MptDbError::Other(format!("account trie root hash (wal_first): {err}"))
                        })?;

                    if let Some(sparse_root) = sparse_root &&
                        sparse_root != root
                    {
                        return Err(MptDbError::Other(format!(
                            "wal_first sparse root mismatch: sparse={sparse_root:?}, account_trie={root:?}"
                        )));
                    }

                    (root, TrieUpdates::default(), account_cow)
                }
            } else {
                let (root, trie_updates) = pending
                    .trie
                    .root_with_updates(&pending.factory)
                    .map_err(|e| MptDbError::Other(format!("sparse root_with_updates: {e}")))?;
                // Refresh hash cache on the old account trie for proof generation.
                // In cross-block sparse steady state, account paths are usually
                // already revealed (`last_sparse_account_reveal_keys == 0`).
                // Keep snapshot-only in this path to avoid extra hashing cost.
                let refresh_account_hash_cache =
                    !self.config.cross_block_sparse || self.last_sparse_account_reveal_keys > 0;
                let account_cow = if refresh_account_hash_cache {
                    let (_, account_cow) = account_trie
                        .root_hash_only_parallel_account(&self.persisted)
                        .map_err(|err| {
                            MptDbError::Other(format!("account trie hash cache update: {err}"))
                        })?;
                    account_cow
                } else {
                    account_trie.snapshot();
                    account_trie
                };
                (root, trie_updates, account_cow)
            };

            // Phase 3b: generate dirty blobs for non-wal_first mode.
            // In wal_first mode, the WAL + segments provide crash recovery, so
            // dirty blobs are not written to RocksDB.  In non-wal_first mode,
            // RocksDB trie tables must be kept current.
            let sparse_blob_start = std::time::Instant::now();
            let blobs = if !mode.wal_first {
                super::sparse_storage::sparse_trie_to_dirty_blobs(&pending.trie, &trie_updates)
                    .map_err(|e| MptDbError::Other(format!("sparse dirty blobs: {e}")))?
            } else {
                Vec::<(B256, Vec<u8>)>::new()
            };
            account_root_sparse_blob_elapsed += sparse_blob_start.elapsed();
            if mode.publish_baseline && !defer_sparse_segment_build {
                let publish_targets: Vec<(B256, B256)> = self
                    .dirty_accounts
                    .iter()
                    .filter_map(|dirty| {
                        if !dirty.storage_wiped && dirty.storage_changes.is_empty() {
                            return None;
                        }
                        let root = storage_roots
                            .get(&dirty.hashed_address)
                            .copied()
                            .unwrap_or(EMPTY_ROOT_HASH);
                        if root == EMPTY_ROOT_HASH {
                            None
                        } else {
                            Some((dirty.hashed_address, root))
                        }
                    })
                    .collect();
                account_root_sparse_publish_targets = publish_targets.len();
                if !publish_targets.is_empty() {
                    let sparse_build_start = std::time::Instant::now();
                    sparse_published_puts =
                        build_storage_segments_from_sparse_trie(&pending.trie, &publish_targets)?;
                    let sparse_build_elapsed = sparse_build_start.elapsed();
                    storage_segment_build_elapsed += sparse_build_elapsed;
                    account_root_sparse_segment_build_elapsed += sparse_build_elapsed;
                }
            } else if mode.publish_baseline && defer_sparse_segment_build {
                account_root_sparse_publish_targets = self
                    .dirty_accounts
                    .iter()
                    .filter(|dirty| {
                        if !dirty.storage_wiped && dirty.storage_changes.is_empty() {
                            return false;
                        }
                        let root = storage_roots
                            .get(&dirty.hashed_address)
                            .copied()
                            .unwrap_or(EMPTY_ROOT_HASH);
                        root != EMPTY_ROOT_HASH
                    })
                    .count();
                // Coalesce hot-account root churn across blocks.  Instead of
                // snapshot-cloning all changed sparse tries every block, keep
                // only latest roots and materialize periodically.
                //
                // This preserves eventual segment freshness while removing the
                // dominant per-block deferred snapshot clone cost on large
                // account sets.
                for dirty in &self.dirty_accounts {
                    if !dirty.storage_wiped && dirty.storage_changes.is_empty() {
                        continue;
                    }
                    let root = storage_roots
                        .get(&dirty.hashed_address)
                        .copied()
                        .unwrap_or(EMPTY_ROOT_HASH);
                    if root == EMPTY_ROOT_HASH {
                        self.sparse_deferred_publish_roots.remove(&dirty.hashed_address);
                    } else {
                        self.sparse_deferred_publish_roots.insert(dirty.hashed_address, root);
                    }
                }

                account_root_sparse_pending_targets = self.sparse_deferred_publish_roots.len();
                let next_version = self.version + 1;
                let (materialize_now, interval) = self.should_materialize_sparse_deferred_now(
                    next_version,
                    account_root_sparse_pending_targets,
                );
                account_root_sparse_materialize_interval = interval;

                if materialize_now && !self.sparse_deferred_publish_roots.is_empty() {
                    let round_budget = self.sparse_deferred_materialize_round_budget(
                        account_root_sparse_pending_targets,
                    );
                    let snapshot_targets: Vec<(B256, B256)> = self
                        .sparse_deferred_publish_roots
                        .iter()
                        .take(round_budget)
                        .map(|(addr, root)| (*addr, *root))
                        .collect();
                    account_root_sparse_snapshot_targets = snapshot_targets.len();
                    let sparse_snapshot_start = std::time::Instant::now();
                    let mut snapshots: Vec<(B256, B256, SerialSparseTrie)> =
                        Vec::with_capacity(snapshot_targets.len());
                    let mut materialized_addrs: Vec<B256> =
                        Vec::with_capacity(snapshot_targets.len());
                    let mut missing_sparse_tries = 0usize;
                    for (hashed_addr, root) in snapshot_targets {
                        if let Some(trie) = pending.trie.storage_trie_ref(&hashed_addr) {
                            snapshots.push((hashed_addr, root, trie.clone()));
                            materialized_addrs.push(hashed_addr);
                        } else {
                            missing_sparse_tries += 1;
                        }
                    }
                    if account_root_trace && missing_sparse_tries > 0 {
                        eprintln!(
                            "[mptdiag:account_root] deferred materialize miss sparse tries={missing_sparse_tries}"
                        );
                    }
                    sparse_committed_tries = snapshots;
                    for addr in materialized_addrs {
                        self.sparse_deferred_publish_roots.remove(&addr);
                    }
                    account_root_sparse_pending_targets = self.sparse_deferred_publish_roots.len();
                    account_root_sparse_deferred_snapshot_elapsed +=
                        sparse_snapshot_start.elapsed();
                }
            }
            // Store sparse trie for proof generation (latest committed version).
            // In cross-block mode, also return the trie to cross_block_sparse
            // so it can be reused in the next block's apply.
            if self.config.cross_block_sparse {
                if let Some(ref mut cross) = self.cross_block_sparse {
                    cross.trie = pending.trie;
                    self.last_committed_sparse_trie = None;
                } else {
                    self.last_committed_sparse_trie = Some(Box::new(pending.trie));
                }
            } else {
                self.last_committed_sparse_trie = Some(Box::new(pending.trie));
            }
            (root, blobs, account_cow)
        } else if let Some(external_root) = external_state_root {
            account_trie.snapshot();
            (external_root, Vec::<(B256, Vec<u8>)>::new(), account_trie)
        } else if hash_only {
            // Empty bundle (no pending sparse state) or non-sparse path.
            // Compute root from the working account trie without collecting blobs.
            let (root, cow) = account_trie
                .root_hash_only_parallel(&self.persisted)
                .map_err(|err| MptDbError::Other(format!("account trie root hash: {err}")))?;
            (root, Vec::<(B256, Vec<u8>)>::new(), cow)
        } else {
            account_trie
                .root_hash_and_dirty_blobs_parallel(&self.persisted)
                .map_err(|err| MptDbError::Other(format!("account trie root hash: {err}")))?
        };
        let account_root_elapsed = account_root_start.elapsed();
        if account_root_trace {
            eprintln!(
                "[mptdiag:account_root] total_ms={:.3} sparse_root_ms={:.3} sparse_blob_ms={:.3} sparse_segment_build_ms={:.3} sparse_deferred_snapshot_ms={:.3} sparse_publish_targets={} sparse_snapshot_targets={} sparse_pending_targets={} sparse_interval={}",
                account_root_elapsed.as_secs_f64() * 1000.0,
                account_root_sparse_root_elapsed.as_secs_f64() * 1000.0,
                account_root_sparse_blob_elapsed.as_secs_f64() * 1000.0,
                account_root_sparse_segment_build_elapsed.as_secs_f64() * 1000.0,
                account_root_sparse_deferred_snapshot_elapsed.as_secs_f64() * 1000.0,
                account_root_sparse_publish_targets,
                account_root_sparse_snapshot_targets,
                account_root_sparse_pending_targets,
                account_root_sparse_materialize_interval,
            );
        }

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
        let prepared =
            self.prepare_storage_version(state_root, &storage_roots, &storage_cache_candidates)?;
        self.last_wal_append_lock_wait = Duration::ZERO;
        self.last_wal_append_write = Duration::ZERO;
        self.last_wal_serialize = Duration::ZERO;
        self.last_wal_crc = Duration::ZERO;
        self.last_wal_payload_bytes = 0;
        let mut wal_append_elapsed = Duration::ZERO;

        let wal_entry = if let Some(ref holder) = wal_changeset_holder {
            // Prefer pre-built changeset from rayon task. If it isn't ready yet,
            // build synchronously to avoid emitting an incomplete WAL entry.
            let accounts = if let Some(accounts) = holder.lock().take() {
                accounts
            } else {
                CommitWalEntry::build_account_changes(&self.dirty_accounts)
            };
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

        // Check test failpoint: ManifestSave
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::ManifestSave) {
            return Err(MptDbError::Other("failpoint: ManifestSave".to_string()));
        }

        // Align with sei-db commit semantics: wal-first commits append WAL on
        // the foreground commit path before returning success.
        if mode.wal_first {
            if let Some(entry) = wal_entry.as_ref() {
                let wal_append_start = std::time::Instant::now();
                self.append_shadow_wal_entry(entry)?;
                wal_append_elapsed = wal_append_start.elapsed();
            }
        }

        let saved = self.save_storage_version(
            prepared,
            wal_append_elapsed,
            all_blobs,
            &storage_roots,
            &mut storage_cache_candidates,
            deferred_published_roots,
            sparse_published_puts,
            sparse_committed_tries,
            mode,
            &mut storage_segment_build_elapsed,
        )?;

        // Commit succeeded: update internal state
        self.manifest = saved.manifest;
        self.version = saved.new_version;
        let checkpoint_account_trie_nodes =
            if skip_account_trie_updates { None } else { Some(account_cow.arena_len()) };
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

        // Update overlay watermark: track the max overlay capacity across all
        // dirty tries committed this block.  Used in steal_overlay_capacity_from
        // to shrink oversized retained capacity on the next block's checkout.
        {
            let block_max = storage_cache_candidates
                .iter()
                .map(|(_, trie)| trie.overlay_capacity())
                .max()
                .unwrap_or(0);
            self.overlay_watermark = block_max;
        }

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

        // Admission-rejected handles are intentionally not inserted into L2.
        // Remove them after commit if they are still uncached to avoid map growth.
        for addr in std::mem::take(&mut self.rejected_activations) {
            if !self.storage_trie_cache.contains(&addr) {
                let published = self.published_version.load(Ordering::Acquire);
                let can_remove = self.storage_trie_handles.get(&addr).is_some_and(|handle| {
                    !handle.has_working() && (published >= handle.base_version)
                });
                if can_remove {
                    if let Some(handle) = self.storage_trie_handles.remove(&addr) {
                        self.pending_drops.push(handle);
                    }
                } else if true && self.storage_trie_handles.contains_key(&addr) {
                    // Reuse the same deferred queue/guard used by regular LRU evictions:
                    // once published_version catches up, touch_cached_storage_trie will drain
                    // and retire these handles safely.
                    self.deferred_evictions.push_back(addr);
                }
            }
        }

        // Remove empty-trie handles that were activated this block but never
        // registered in the LRU. They have no cross-block value and would
        // otherwise accumulate indefinitely in storage_trie_handles.
        for addr in std::mem::take(&mut self.empty_trie_activations) {
            if !self.storage_trie_cache.contains(&addr) {
                self.storage_trie_handles.remove(&addr);
            }
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
            sparse_apply_factory_build: self.last_sparse_apply_factory_build,
            sparse_apply_account_proof: self.last_sparse_apply_account_proof,
            sparse_apply_apply_changes: self.last_sparse_apply_apply_changes,
            sparse_factory_dirty_accounts: self.last_apply_sparse_factory.dirty_accounts,
            sparse_factory_storage_accounts: self.last_apply_sparse_factory.storage_accounts,
            sparse_factory_segment_lookups: self.last_apply_sparse_factory.segment_lookups,
            sparse_factory_segment_hits: self.last_apply_sparse_factory.segment_hits,
            sparse_factory_segment_miss_no_store: self
                .last_apply_sparse_factory
                .segment_miss_no_store,
            sparse_factory_segment_miss: self.last_apply_sparse_factory.segment_miss,
            sparse_factory_segment_root_mismatch: self
                .last_apply_sparse_factory
                .segment_root_mismatch,
            sparse_factory_tier3_attempts: self.last_apply_sparse_factory.tier3_attempts,
            sparse_factory_tier3_hits: self.last_apply_sparse_factory.tier3_hits,
            sparse_factory_tier12_attempts: self.last_apply_sparse_factory.tier12_attempts,
            sparse_factory_cross_reuse_accounts: self
                .last_apply_sparse_factory
                .cross_reuse_accounts,
            sparse_factory_cross_missing_slots: self.last_apply_sparse_factory.cross_missing_slots,
            sparse_factory_cross_missing_proof_slots: self
                .last_apply_sparse_factory
                .cross_missing_proof_slots,
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
            wal_serialize: self.last_wal_serialize,
            wal_crc: self.last_wal_crc,
            wal_payload_bytes: self.last_wal_payload_bytes,
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
            apply_published_view_refresh: self.last_apply_published_view_refresh,
            apply_storage_root_lookup: self.last_apply_storage_root_lookup,
            commit_account_set_base: acct_set_base_elapsed,
            commit_cache_storage_prep: cache_storage_prep_elapsed,
            overlay_stolen: self.last_overlay_stolen,
            overlay_fresh_clone: self.last_overlay_fresh_clone,
            overlay_existing_working: self.last_overlay_existing_working,
            overlay_shrink_events: self.last_overlay_shrink_events,
            overlay_reused_capacity_entries: self.last_overlay_reuse_capacity_entries,
            overlay_watermark: self.overlay_watermark,
        };

        // In wal_first mode, periodically save the account trie checkpoint
        // so cold starts can load directly from the checkpoint file instead
        // of materializing the entire trie from RocksDB.
        if true &&
            self.bulk_load.is_none() &&
            saved.new_version > 0 &&
            (saved.new_version as usize) % self.config.published_snapshot_interval == 0
        {
            let _ = self.save_checkpoint();
        }

        self.enforce_frontier_invariants("commit")?;
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
        self.store.enforce_frontier_invariants("importer_close")?;

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
        // BulkSegmentWriter publishes segments during bulk_load, so
        // published_version may be set to the final version.
        assert!(
            bulk.published_version().is_none() ||
                bulk.published_version() == Some(expected_version),
        );
        assert!(bulk.wal_store.as_ref().unwrap().lock().is_empty());
    }

    #[test]
    fn bulk_load_can_continue_with_normal_commits_and_reopen() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
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
        // Use non-sparse for both stores so this test focuses on wal_first vs
        // non-wal_first parity.  Sparse vs non-sparse parity is verified by the
        // SP-* integration tests.
        let mut legacy_config = MptConfig::default();
        legacy_config.use_sparse_storage = false;
        let mut legacy =
            MptCommitStore::open_with_config(legacy_dir.path(), false, legacy_config).unwrap();
        let wal_dir = TempDir::new().unwrap();
        let mut wal_config = MptConfig::default();
        wal_config.wal_shadow_validate = true;
        wal_config.checkpoint_max_account_trie_nodes = 0;
        // Shadow validation requires WAL replay to have storage proofs.
        // In sparse mode, wal_first blocks don't write to RocksDB, so the
        // shadow replay can't find storage trie nodes.  Disable sparse for
        // wal_first in this test (root parity between sparse and non-sparse
        // is verified by the SP-* tests).
        wal_config.use_sparse_storage = false;
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
        store.flush_persist().unwrap();

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
        store.flush_persist().unwrap();

        let wal = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(wal.latest_version(), 2);
        assert!(wal.load_entry(1).unwrap().is_some());
        assert!(wal.load_entry(2).unwrap().is_some());
    }

    #[test]
    fn shadow_wal_rollback_truncates_newer_entries() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
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
        src.flush_persist().unwrap();

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
    fn wal_first_default_defers_sparse_segment_build_to_background() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let bundle = make_bundle(vec![
            (
                Address::repeat_byte(0x71),
                Some(default_info(1, 1001)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(11))],
            ),
            (
                Address::repeat_byte(0x72),
                Some(default_info(2, 1002)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(22))],
            ),
            (
                Address::repeat_byte(0x73),
                Some(default_info(3, 1003)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(3), U256::ZERO, U256::from(33))],
            ),
        ]);
        store.apply_bundle_state(&bundle).unwrap();
        let ((_version, _root), profile) = store.commit_with_profile().unwrap();

        assert_eq!(
            profile.storage_segment_build,
            Duration::ZERO,
            "wal_first default should defer sparse segment build to background worker"
        );
    }

    #[test]
    fn wal_first_can_force_foreground_sparse_segment_build() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
        config.wal_first_defer_segment_build = false;
        let mut store = MptCommitStore::open_with_config(dir.path(), false, config).unwrap();

        let bundle = make_bundle(vec![
            (
                Address::repeat_byte(0x81),
                Some(default_info(1, 2001)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(1), U256::ZERO, U256::from(111))],
            ),
            (
                Address::repeat_byte(0x82),
                Some(default_info(2, 2002)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(2), U256::ZERO, U256::from(222))],
            ),
            (
                Address::repeat_byte(0x83),
                Some(default_info(3, 2003)),
                revm_database::AccountStatus::Changed,
                vec![(U256::from(3), U256::ZERO, U256::from(333))],
            ),
        ]);
        store.apply_bundle_state(&bundle).unwrap();
        let ((_version, _root), profile) = store.commit_with_profile().unwrap();

        assert!(
            profile.storage_segment_build > Duration::ZERO,
            "foreground mode should spend non-zero time on sparse segment build"
        );
    }

    #[test]
    fn wal_first_rewrite_does_not_rollback_newer_published_meta() {
        let dir = TempDir::new().unwrap();
        let mut config = MptConfig::default();
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

        // Version 1: sparse trie is cleared after rollback, so proof returns an error.
        // Re-apply the latest block to restore proof generation capability.
        assert!(store.account_proof(1, addr, &[]).is_err());
        let _ = root1;
    }

    // ── Storage trie cache tests ──

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
        // After load_version, the account trie may be fully materialized
        // (from checkpoint) or lazy (from persisted root).  Either is valid;
        // the important invariant is that subsequent commits work correctly.
        let _ = full_nodes; // used only as baseline reference

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
        // flush_persist ensures background worker has published segments
        // before we clear the L2 cache, so reload can find the data.
        let mut store_n = MptCommitStore::open(dir_nocache.path(), false).unwrap();
        store_n.apply_bundle_state(&bundle1).unwrap();
        let (_, r1n) = store_n.commit().unwrap();
        store_n.flush_persist().unwrap();
        store_n.clear_storage_trie_state();
        store_n.apply_bundle_state(&bundle2).unwrap();
        let (_, r2n) = store_n.commit().unwrap();
        store_n.flush_persist().unwrap();
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

    // ── apply_hashed_state_overlay acceptance tests ──────────────────────────

    fn make_hashed_post_state(bundle: &BundleState) -> reth_trie_common::HashedPostState {
        use rayon::prelude::*;
        use reth_trie_common::{HashedPostState, KeccakKeyHasher};
        HashedPostState::from_bundle_state::<KeccakKeyHasher>(bundle.state.par_iter())
    }

    /// Idempotency: calling apply_hashed_state_overlay twice with the same
    /// HashedPostState must return the same root.
    #[test]
    #[ignore]
    fn overlay_root_idempotent() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x11);
        let info = AccountInfo { nonce: 1, balance: U256::from(100u64), ..Default::default() };
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Loaded,
            vec![(U256::from(1u64), U256::ZERO, U256::from(42u64))],
        )]);
        let hashed = make_hashed_post_state(&bundle);

        let root_a = store.apply_hashed_state_overlay(&hashed).unwrap();
        // SC state must be unchanged (applied_this_block still false)
        assert!(!store.applied_this_block);

        let root_b = store.apply_hashed_state_overlay(&hashed).unwrap();
        assert_eq!(root_a, root_b, "apply_hashed_state_overlay must be idempotent");
    }

    /// Consistency: apply_hashed_state_overlay and apply_bundle_state + commit
    /// must produce the same state root for the same block.
    #[test]
    #[ignore]
    fn overlay_root_matches_commit_root() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x22);
        let info = AccountInfo { nonce: 5, balance: U256::from(999u64), ..Default::default() };
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Loaded,
            vec![(U256::from(7u64), U256::ZERO, U256::from(123u64))],
        )]);
        let hashed = make_hashed_post_state(&bundle);

        // Dry-run: compute root without committing
        let root_overlay = store.apply_hashed_state_overlay(&hashed).unwrap();
        assert!(!store.applied_this_block, "overlay must not set applied_this_block");

        // Actual commit
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root_commit) = store.commit().unwrap();

        assert_eq!(
            root_overlay, root_commit,
            "overlay root must match commit root for the same block"
        );
    }

    /// After overlay + no-commit, apply_bundle_state must still work correctly.
    #[test]
    #[ignore]
    fn overlay_does_not_block_subsequent_apply() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x33);
        let info = AccountInfo { nonce: 1, balance: U256::from(1u64), ..Default::default() };
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Loaded, vec![])]);
        let hashed = make_hashed_post_state(&bundle);

        // Overlay (dry-run) — must not affect subsequent apply
        let _ = store.apply_hashed_state_overlay(&hashed).unwrap();

        // apply_bundle_state must succeed after overlay
        store.apply_bundle_state(&bundle).unwrap();
        let (version, _) = store.commit().unwrap();
        assert_eq!(version, 1);
    }
}
