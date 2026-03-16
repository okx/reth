/// Parallelism thresholds for MPT commit operations.
///
/// Controls when rayon parallel paths are used instead of serial iteration.
/// Phase 4 does not support runtime config files; thresholds come from `Default`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ParallelismThresholds {
    /// Minimum dirty storage tries to enable rayon parallel root computation.
    pub storage_tries_min: usize,
    /// Minimum account trie frontier children to enable parallel hash.
    pub account_frontier_min: usize,
}

impl Default for ParallelismThresholds {
    fn default() -> Self {
        Self { storage_tries_min: 64, account_frontier_min: 4 }
    }
}

impl ParallelismThresholds {
    /// Whether storage trie root computation should use rayon parallel iteration.
    pub(crate) fn should_parallelize_storage_tries(&self, trie_count: usize) -> bool {
        trie_count >= self.storage_tries_min
    }

    /// Whether the account trie root hash should use the parallel frontier path.
    pub(crate) fn should_parallelize_account_frontier(&self, frontier_children: usize) -> bool {
        frontier_children >= self.account_frontier_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T1.1: Default thresholds are correct.
    #[test]
    fn t1_1_default_thresholds() {
        let t = ParallelismThresholds::default();
        assert_eq!(t.storage_tries_min, 64);
        assert_eq!(t.account_frontier_min, 4);
    }

    /// T1.2: trie_count < storage_tries_min -> false.
    #[test]
    fn t1_2_storage_below_threshold() {
        let t = ParallelismThresholds::default();
        assert!(!t.should_parallelize_storage_tries(0));
        assert!(!t.should_parallelize_storage_tries(63));
    }

    /// T1.3: trie_count >= storage_tries_min -> true.
    #[test]
    fn t1_3_storage_at_or_above_threshold() {
        let t = ParallelismThresholds::default();
        assert!(t.should_parallelize_storage_tries(64));
        assert!(t.should_parallelize_storage_tries(1000));
    }

    /// T1.4: frontier_children < account_frontier_min -> false.
    #[test]
    fn t1_4_frontier_below_threshold() {
        let t = ParallelismThresholds::default();
        assert!(!t.should_parallelize_account_frontier(0));
        assert!(!t.should_parallelize_account_frontier(3));
    }

    /// T1.5: frontier_children >= account_frontier_min -> true.
    #[test]
    fn t1_5_frontier_at_or_above_threshold() {
        let t = ParallelismThresholds::default();
        assert!(t.should_parallelize_account_frontier(4));
        assert!(t.should_parallelize_account_frontier(16));
    }
}
