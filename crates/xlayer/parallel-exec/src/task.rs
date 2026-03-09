//! Execution task definition.
//!
//! An `ExeTask` groups non-conflicting transactions that can be executed in parallel.
//! Each task carries the merged read/write sets of its constituent transactions,
//! enabling the Framer to reason about inter-task conflicts via bloom filter queries.

use crate::crw_sets::CrwSets;

/// Result from the Simulator's pre-execution of a single transaction.
///
/// Contains the predicted read/write sets and the transaction's position in the
/// original block ordering. The actual transaction object is not stored here;
/// it will be resolved by `original_index` when execution is dispatched.
#[derive(Debug, Clone)]
pub struct SimResult {
    /// Read/write sets extracted during pre-execution.
    pub crw_sets: CrwSets,
    /// Index in the original transaction list (for result ordering).
    pub original_index: usize,
    /// Whether the simulation succeeded (false if EVM reverted or errored).
    pub success: bool,
}

/// A group of non-conflicting transactions assigned to the same execution frame.
///
/// All transactions within a single `ExeTask` are executed sequentially, but
/// multiple `ExeTask`s within the same frame can run in parallel because their
/// merged read/write sets are guaranteed not to conflict.
#[derive(Debug, Clone)]
pub struct ExeTask {
    /// Simulation results for each transaction in this task.
    pub sim_results: Vec<SimResult>,
    /// Merged read/write sets of all transactions (used for Bloom filter queries).
    pub merged_crw_sets: CrwSets,
}

impl ExeTask {
    /// Create a new `ExeTask` from a single `SimResult`.
    pub fn new(sim_result: SimResult) -> Self {
        let merged_crw_sets = sim_result.crw_sets.clone();
        Self { sim_results: vec![sim_result], merged_crw_sets }
    }

    /// Add another `SimResult` to this task, merging its `CrwSets`.
    pub fn add(&mut self, sim_result: SimResult) {
        self.merged_crw_sets.merge(&sim_result.crw_sets);
        self.sim_results.push(sim_result);
    }

    /// Number of transactions in this task.
    pub fn len(&self) -> usize {
        self.sim_results.len()
    }

    /// Whether this task has no transactions.
    pub fn is_empty(&self) -> bool {
        self.sim_results.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sim_result(index: usize, reads: Vec<[u8; 10]>, writes: Vec<[u8; 10]>) -> SimResult {
        SimResult {
            crw_sets: CrwSets {
                account_reads: reads,
                account_writes: writes,
                storage_reads: vec![],
                storage_writes: vec![],
            },
            original_index: index,
            success: true,
        }
    }

    #[test]
    fn test_exe_task_new() {
        let sr = make_sim_result(5, vec![[1u8; 10]], vec![[2u8; 10]]);
        let task = ExeTask::new(sr);

        assert_eq!(task.len(), 1);
        assert!(!task.is_empty());
        assert_eq!(task.sim_results[0].original_index, 5);
        assert_eq!(task.merged_crw_sets.account_reads, vec![[1u8; 10]]);
        assert_eq!(task.merged_crw_sets.account_writes, vec![[2u8; 10]]);
    }

    #[test]
    fn test_exe_task_add() {
        let sr1 = make_sim_result(0, vec![[1u8; 10]], vec![]);
        let sr2 = make_sim_result(1, vec![[2u8; 10]], vec![[3u8; 10]]);

        let mut task = ExeTask::new(sr1);
        task.add(sr2);

        assert_eq!(task.len(), 2);
        assert_eq!(task.merged_crw_sets.account_reads.len(), 2);
        assert_eq!(task.merged_crw_sets.account_writes.len(), 1);
        assert!(task.merged_crw_sets.account_reads.contains(&[1u8; 10]));
        assert!(task.merged_crw_sets.account_reads.contains(&[2u8; 10]));
        assert!(task.merged_crw_sets.account_writes.contains(&[3u8; 10]));
    }

    #[test]
    fn test_exe_task_len() {
        let sr = make_sim_result(0, vec![], vec![]);
        let task = ExeTask::new(sr);
        assert_eq!(task.len(), 1);

        let empty_sets = CrwSets::default();
        let empty_task = ExeTask { sim_results: vec![], merged_crw_sets: empty_sets };
        assert!(empty_task.is_empty());
        assert_eq!(empty_task.len(), 0);
    }
}
