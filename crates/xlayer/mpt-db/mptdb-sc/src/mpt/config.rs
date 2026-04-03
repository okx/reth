/// Unified configuration for the MPT commit store.
#[derive(Debug, Clone)]
pub struct MptConfig {
    /// Maximum number of storage tries to cache across blocks.
    pub storage_trie_cache_capacity: usize,
    /// Maximum number of persisted trie nodes to cache in memory.
    pub persisted_node_cache_capacity: usize,
    /// Minimum number of storage tries before parallelizing root computation.
    pub parallel_storage_tries_min: usize,
    /// Minimum account trie frontier width before parallelizing root hash.
    pub parallel_account_frontier_min: usize,
    /// Depth of the async persist queue (bounded channel capacity).
    pub async_queue_depth: usize,
    /// Threshold: blobs below this count use async persist, above use sync.
    pub async_blob_threshold: usize,
    /// Enable phase-1 shadow WAL append on commit.
    pub wal_first_commit: bool,
    /// In wal_first mode, build published storage segments in the background
    /// persist worker from committed trie snapshots instead of on the frontend
    /// commit hot path.
    ///
    /// This keeps `commit+root` CPU cost low on large account sets while still
    /// publishing segments for mmap/L3 reads after the worker catches up.
    /// Default: `true`.
    pub wal_first_defer_segment_build: bool,
    /// When enabled alongside `wal_first_commit`, perform extra parity checks.
    pub wal_shadow_validate: bool,
    /// How many committed versions to advance before rewriting a fresh published snapshot.
    pub published_snapshot_interval: usize,
    /// Maximum seconds the published-rewrite worker waits for durable_version
    /// to reach the target before giving up on the current rewrite job.
    pub published_rewrite_timeout_secs: u64,
    /// Maximum number of versions that committed_version may lead durable_version.
    /// When the lag reaches this limit, the frontend commit blocks until the
    /// persist worker catches up.  0 = no limit (rely on channel capacity only).
    pub max_durable_lag: i64,
    /// Maximum number of versions that committed_version may lead published_version.
    /// When the lag reaches this limit, the frontend commit blocks until the
    /// publish worker catches up.  0 = no limit.
    pub max_published_lag: i64,
    /// Maximum MB/s for background snapshot rewrite IO.  Limits how fast the
    /// published-rewrite worker writes segment pages, preventing it from
    /// starving the frontend of disk bandwidth.  0 = unlimited.
    pub snapshot_write_rate_mb_per_sec: u64,
    /// Skip account-trie checkpoint writes once the committed trie grows beyond this size.
    pub checkpoint_max_account_trie_nodes: usize,
    /// Maximum WAL size in bytes before the frontend applies backpressure.
    /// 0 = unlimited. Enforcement is deferred to a future change.
    pub max_wal_bytes: u64,
    /// Enable overlay capacity stealing between consecutive blocks.
    /// When true (default), the working trie inherits the cleared overlay
    /// capacity from the previous base, eliminating per-block HashMap resizes.
    /// Set to false to disable and fall back to fresh allocations — useful for
    /// bisecting regressions or diagnosing allocator issues in production.
    pub overlay_reuse_enabled: bool,
    /// Replace `StorageTrieCow`-based storage apply with reth `SparseStateTrie`.
    ///
    /// When true, `apply_dirty_accounts_inner` creates a `SparseStateTrie`, reveals
    /// dirty paths from published L3 segments, and applies all storage + account
    /// changes via the sparse engine.  `commit_inner_with_mode` then calls
    /// `root_with_updates` instead of the custom per-account root aggregation.
    ///
    /// Default: `false`.  The feature flag allows toggling without data migration.
    pub use_sparse_storage: bool,

    /// Keep the `SparseStateTrie` alive across blocks (Phase 4 optimisation).
    ///
    /// When `true`, the sparse trie built in one block is reused in the next:
    /// already-revealed storage tries are skipped (no re-reveal DFS), and the
    /// factory skips tier-1/tier-2 full-arena DFS for those accounts.  Only new
    /// dirty paths require reveal.  This eliminates the per-block
    /// `convert_arena_to_decoded_storage_multiproof` overhead.
    ///
    /// Requires `use_sparse_storage=true`.  Default: `true`.
    pub cross_block_sparse: bool,

    /// Maximum number of blocks a storage account's trie can remain in the
    /// cross-block `SparseStateTrie` without being accessed before it is
    /// evicted.
    ///
    /// Eviction bounds memory growth for workloads with many distinct accounts.
    /// `0` disables eviction (unbounded; only use for testing).
    /// Default: `8`.
    pub cross_block_sparse_max_lag: i64,
}

impl Default for MptConfig {
    fn default() -> Self {
        Self {
            storage_trie_cache_capacity: 50_000,
            persisted_node_cache_capacity: 500_000,
            parallel_storage_tries_min: 64,
            parallel_account_frontier_min: 4,
            async_queue_depth: 64,
            async_blob_threshold: 50_000,
            wal_first_commit: false,
            wal_first_defer_segment_build: true,
            wal_shadow_validate: false,
            published_snapshot_interval: 64,
            published_rewrite_timeout_secs: 60,
            max_durable_lag: 128,
            max_published_lag: 16,
            snapshot_write_rate_mb_per_sec: 0,
            checkpoint_max_account_trie_nodes: 200_000,
            max_wal_bytes: 0,
            overlay_reuse_enabled: true,
            use_sparse_storage: true,
            cross_block_sparse: true,
            cross_block_sparse_max_lag: 8,
        }
    }
}
