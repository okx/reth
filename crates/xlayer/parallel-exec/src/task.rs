//! Execution task definition.
//!
//! An `ExeTask` groups non-conflicting transactions that can be executed in parallel.
//! Each task carries the merged read/write sets of its constituent transactions,
//! enabling the Framer to reason about inter-task conflicts via bloom filter queries.

use crate::crw_sets::CrwSets;
use std::sync::atomic::{AtomicU32, Ordering};

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

/// Simplified access set for collision detection.
///
/// Separates read-only and read-and-write accesses so that read-read pairs
/// are correctly identified as non-conflicting, while any pair involving a
/// write is flagged as a collision.
#[derive(Debug, Clone, Default)]
pub struct AccessSet {
    /// Read-only hashes (accounts/slots only read, not written).
    pub rdo_set: Vec<[u8; 10]>,
    /// Read-and-write hashes (accounts/slots that are written).
    pub rnw_set: Vec<[u8; 10]>,
}

impl AccessSet {
    /// Derive an `AccessSet` from `CrwSets`.
    ///
    /// `rdo` = reads that are NOT in writes.
    /// `rnw` = all writes.
    pub fn from_crw_sets(crw: &CrwSets) -> Self {
        let all_writes: Vec<[u8; 10]> =
            crw.account_writes.iter().chain(crw.storage_writes.iter()).copied().collect();

        let all_reads: Vec<[u8; 10]> =
            crw.account_reads.iter().chain(crw.storage_reads.iter()).copied().collect();

        // rdo = reads that are NOT also in writes
        let rdo_set: Vec<[u8; 10]> =
            all_reads.into_iter().filter(|r| !all_writes.contains(r)).collect();

        AccessSet { rdo_set, rnw_set: all_writes }
    }

    /// Check if this access set has a collision with another.
    ///
    /// Collision rules:
    /// - `my_rnw` intersects `other_rnw` -> conflict (write-write)
    /// - `my_rnw` intersects `other_rdo` -> conflict (write vs read)
    /// - `my_rdo` intersects `other_rnw` -> conflict (read vs write)
    /// - `my_rdo` intersects `other_rdo` -> no conflict (read-read is fine)
    pub fn has_collision(&self, other: &AccessSet) -> bool {
        // Check rnw vs rnw
        for h in &self.rnw_set {
            if other.rnw_set.contains(h) {
                return true;
            }
        }
        // Check rnw vs other's rdo
        for h in &self.rnw_set {
            if other.rdo_set.contains(h) {
                return true;
            }
        }
        // Check other's rnw vs my rdo
        for h in &other.rnw_set {
            if self.rdo_set.contains(h) {
                return true;
            }
        }
        false
    }

    /// Merge another `AccessSet` into this one.
    ///
    /// Writes from the other set promote any existing read-only entries to
    /// read-and-write, and new reads are only added as read-only if not
    /// already covered by a write.
    pub fn merge(&mut self, other: &AccessSet) {
        for h in &other.rdo_set {
            if !self.rdo_set.contains(h) && !self.rnw_set.contains(h) {
                self.rdo_set.push(*h);
            }
        }
        for h in &other.rnw_set {
            if !self.rnw_set.contains(h) {
                self.rnw_set.push(*h);
                // Promote from rdo to rnw
                self.rdo_set.retain(|r| r != h);
            }
        }
    }
}

/// A group of non-conflicting transactions assigned to the same execution frame.
///
/// All transactions within a single `ExeTask` are executed sequentially, but
/// multiple `ExeTask`s within the same frame can run in parallel because their
/// merged read/write sets are guaranteed not to conflict.
#[derive(Debug)]
pub struct ExeTask {
    /// Simulation results for each transaction in this task.
    pub sim_results: Vec<SimResult>,
    /// Merged read/write sets of all transactions (used for Bloom filter queries).
    pub merged_crw_sets: CrwSets,
    /// Access set for collision detection (derived from merged_crw_sets).
    pub access_set: AccessSet,
    /// Transaction environments for actual execution (one per transaction).
    pub tx_envs: Vec<revm::context::TxEnv>,
    /// Starting index when this task's frame was flushed.
    /// Used by the Dispatcher for EEI (Earliest Execution Index) computation.
    task_out_start: AtomicU32,
}

impl Clone for ExeTask {
    fn clone(&self) -> Self {
        Self {
            sim_results: self.sim_results.clone(),
            merged_crw_sets: self.merged_crw_sets.clone(),
            access_set: self.access_set.clone(),
            tx_envs: self.tx_envs.clone(),
            task_out_start: AtomicU32::new(self.task_out_start.load(Ordering::Relaxed)),
        }
    }
}

impl ExeTask {
    /// Create a new `ExeTask` from a single `SimResult`.
    pub fn new(sim_result: SimResult) -> Self {
        let merged_crw_sets = sim_result.crw_sets.clone();
        let access_set = AccessSet::from_crw_sets(&merged_crw_sets);
        Self {
            sim_results: vec![sim_result],
            merged_crw_sets,
            access_set,
            tx_envs: Vec::new(),
            task_out_start: AtomicU32::new(0),
        }
    }

    /// Add another `SimResult` to this task, merging its `CrwSets` and `AccessSet`.
    pub fn add(&mut self, sim_result: SimResult) {
        let new_access = AccessSet::from_crw_sets(&sim_result.crw_sets);
        self.access_set.merge(&new_access);
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

    /// Get the task output start index (set by Framer when flushing a frame).
    pub fn get_task_out_start(&self) -> usize {
        self.task_out_start.load(Ordering::Relaxed) as usize
    }

    /// Set the task output start index.
    pub fn set_task_out_start(&self, start: usize) {
        self.task_out_start.store(start as u32, Ordering::Relaxed);
    }

    /// Check whether two tasks have overlapping read-write sets (collision).
    pub fn has_collision(a: &ExeTask, b: &ExeTask) -> bool {
        a.access_set.has_collision(&b.access_set)
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
        let empty_task = ExeTask {
            sim_results: vec![],
            merged_crw_sets: empty_sets,
            access_set: AccessSet::default(),
            tx_envs: Vec::new(),
            task_out_start: AtomicU32::new(0),
        };
        assert!(empty_task.is_empty());
        assert_eq!(empty_task.len(), 0);
    }

    // --- AccessSet tests ---

    #[test]
    fn test_access_set_from_crw_sets() {
        let crw = CrwSets {
            account_reads: vec![[1u8; 10], [2u8; 10]],
            account_writes: vec![[2u8; 10]],
            storage_reads: vec![[3u8; 10]],
            storage_writes: vec![[4u8; 10]],
        };
        let access = AccessSet::from_crw_sets(&crw);

        // [2u8;10] is both read and written, so it goes to rnw only
        assert!(access.rdo_set.contains(&[1u8; 10]));
        assert!(access.rdo_set.contains(&[3u8; 10]));
        assert!(!access.rdo_set.contains(&[2u8; 10]));

        // rnw = all writes
        assert!(access.rnw_set.contains(&[2u8; 10]));
        assert!(access.rnw_set.contains(&[4u8; 10]));
        assert_eq!(access.rnw_set.len(), 2);
        assert_eq!(access.rdo_set.len(), 2);
    }

    #[test]
    fn test_access_set_no_collision_read_read() {
        // Both tasks only read the same slot -> no conflict
        let a = AccessSet { rdo_set: vec![[1u8; 10]], rnw_set: vec![] };
        let b = AccessSet { rdo_set: vec![[1u8; 10]], rnw_set: vec![] };
        assert!(!a.has_collision(&b));
    }

    #[test]
    fn test_access_set_collision_write_write() {
        let a = AccessSet { rdo_set: vec![], rnw_set: vec![[1u8; 10]] };
        let b = AccessSet { rdo_set: vec![], rnw_set: vec![[1u8; 10]] };
        assert!(a.has_collision(&b));
    }

    #[test]
    fn test_access_set_collision_write_read() {
        // a writes, b reads -> conflict
        let a = AccessSet { rdo_set: vec![], rnw_set: vec![[1u8; 10]] };
        let b = AccessSet { rdo_set: vec![[1u8; 10]], rnw_set: vec![] };
        assert!(a.has_collision(&b));
    }

    #[test]
    fn test_access_set_collision_read_write() {
        // a reads, b writes -> conflict
        let a = AccessSet { rdo_set: vec![[1u8; 10]], rnw_set: vec![] };
        let b = AccessSet { rdo_set: vec![], rnw_set: vec![[1u8; 10]] };
        assert!(a.has_collision(&b));
    }

    #[test]
    fn test_access_set_merge() {
        let mut a = AccessSet { rdo_set: vec![[1u8; 10]], rnw_set: vec![[2u8; 10]] };

        let b = AccessSet { rdo_set: vec![[3u8; 10], [2u8; 10]], rnw_set: vec![[1u8; 10]] };

        a.merge(&b);

        // [1u8;10] was rdo in a, rnw in b -> promoted to rnw
        assert!(!a.rdo_set.contains(&[1u8; 10]));
        assert!(a.rnw_set.contains(&[1u8; 10]));

        // [2u8;10] was already rnw in a, rdo in b -> stays rnw, not duplicated
        assert!(a.rnw_set.contains(&[2u8; 10]));
        assert!(!a.rdo_set.contains(&[2u8; 10]));

        // [3u8;10] was rdo in b, not in a -> added as rdo
        assert!(a.rdo_set.contains(&[3u8; 10]));
    }

    #[test]
    fn test_exe_task_has_collision() {
        // Task a writes [1u8;10], task b reads [1u8;10] -> collision
        let sr_a = make_sim_result(0, vec![], vec![[1u8; 10]]);
        let sr_b = make_sim_result(1, vec![[1u8; 10]], vec![]);

        let task_a = ExeTask::new(sr_a);
        let task_b = ExeTask::new(sr_b);

        assert!(ExeTask::has_collision(&task_a, &task_b));

        // Task c reads [5u8;10], task d reads [5u8;10] -> no collision
        let sr_c = make_sim_result(2, vec![[5u8; 10]], vec![]);
        let sr_d = make_sim_result(3, vec![[5u8; 10]], vec![]);

        let task_c = ExeTask::new(sr_c);
        let task_d = ExeTask::new(sr_d);

        assert!(!ExeTask::has_collision(&task_c, &task_d));
    }

    #[test]
    fn test_task_out_start() {
        let sr = make_sim_result(0, vec![], vec![]);
        let task = ExeTask::new(sr);

        assert_eq!(task.get_task_out_start(), 0);

        task.set_task_out_start(42);
        assert_eq!(task.get_task_out_start(), 42);

        task.set_task_out_start(1000);
        assert_eq!(task.get_task_out_start(), 1000);
    }

    #[test]
    fn test_exe_task_new_with_access_set() {
        // Create task with reads [1,2] and writes [2,3]
        let sr = SimResult {
            crw_sets: CrwSets {
                account_reads: vec![[1u8; 10], [2u8; 10]],
                account_writes: vec![[2u8; 10]],
                storage_reads: vec![],
                storage_writes: vec![[3u8; 10]],
            },
            original_index: 0,
            success: true,
        };

        let task = ExeTask::new(sr);

        // rdo should only contain [1u8;10] (the read not in writes)
        assert_eq!(task.access_set.rdo_set.len(), 1);
        assert!(task.access_set.rdo_set.contains(&[1u8; 10]));

        // rnw should contain [2u8;10] and [3u8;10] (all writes)
        assert_eq!(task.access_set.rnw_set.len(), 2);
        assert!(task.access_set.rnw_set.contains(&[2u8; 10]));
        assert!(task.access_set.rnw_set.contains(&[3u8; 10]));
    }
}
