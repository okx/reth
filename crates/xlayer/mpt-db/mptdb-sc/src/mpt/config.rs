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
            checkpoint_max_account_trie_nodes: 200_000,
        }
    }
}
