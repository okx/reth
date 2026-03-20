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
}

impl Default for MptConfig {
    fn default() -> Self {
        Self {
            storage_trie_cache_capacity: 250_000,
            persisted_node_cache_capacity: 500_000,
            parallel_storage_tries_min: 64,
            parallel_account_frontier_min: 4,
            async_queue_depth: 64,
            async_blob_threshold: 50_000,
            wal_first_commit: false,
            wal_shadow_validate: false,
            published_snapshot_interval: 64,
            published_rewrite_timeout_secs: 60,
            max_durable_lag: 128,
            max_published_lag: 0,
            snapshot_write_rate_mb_per_sec: 0,
            checkpoint_max_account_trie_nodes: 200_000,
        }
    }
}
