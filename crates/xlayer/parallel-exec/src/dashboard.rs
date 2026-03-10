//! Lock-free dependency graph for parallel task scheduling.
//!
//! The Dashboard tracks task dependencies using atomic linked lists.
//! When a task completes, it "ignites" dependent tasks that were waiting on it.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

/// Batch size for grouping tasks.
pub const BATCH_SIZE: i32 = 4096;

/// Number of parallel batch scanner threads.
pub const SCAN_COUNT: usize = 4;

/// Window size for EEI backward scan.
pub const EARLY_EXE_WINDOW_SIZE: usize = 128;

/// Sentinel value indicating no dependency (can execute immediately).
pub const FIRST_FRAME: i32 = -1;

/// Sentinel value for empty linked list.
const EMPTY: i32 = -1;

/// Bit used to mark a linked list node as dispatched.
const DISPATCH_MASK: i32 = i32::MIN; // 0x80000000

/// A node in the per-task ignition linked list.
#[derive(Debug)]
pub struct LinkedListItem {
    /// Head of the linked list of tasks to ignite when this task completes.
    /// Uses atomic swap for lock-free insertion.
    pub to_ignite: AtomicI32,
    /// Next pointer in the linked list chain.
    pub next: AtomicI32,
}

impl LinkedListItem {
    fn new() -> Self {
        Self { to_ignite: AtomicI32::new(EMPTY), next: AtomicI32::new(EMPTY) }
    }
}

/// Lock-free dependency graph for parallel task scheduling.
///
/// Tracks which tasks depend on which, and coordinates batch-level
/// completion for the Dispatcher.
#[derive(Debug)]
pub struct Dashboard {
    /// Highest contiguously completed task index.
    pub all_done_index: AtomicI32,
    /// Total number of valid tasks in the current block.
    pub valid_count: AtomicI32,
    /// Per-batch warmup completion counters.
    warmed_counts: Vec<AtomicI32>,
    /// Bitvec tracking which tasks have been executed.
    executed_bitvec: Vec<AtomicU64>,
    /// Per-task ignition linked lists.
    ignite_ll: Vec<LinkedListItem>,
    /// Batch size.
    batch_size: i32,
}

impl Dashboard {
    /// Create a new Dashboard with capacity for `max_tasks` tasks.
    pub fn new(max_tasks: usize) -> Self {
        let num_batches = (max_tasks as i32 + BATCH_SIZE - 1) / BATCH_SIZE;
        let bitvec_len = (max_tasks + 63) / 64;

        let mut warmed_counts = Vec::with_capacity(num_batches as usize);
        for _ in 0..num_batches {
            warmed_counts.push(AtomicI32::new(0));
        }

        let mut executed_bitvec = Vec::with_capacity(bitvec_len);
        for _ in 0..bitvec_len {
            executed_bitvec.push(AtomicU64::new(0));
        }

        let mut ignite_ll = Vec::with_capacity(max_tasks);
        for _ in 0..max_tasks {
            ignite_ll.push(LinkedListItem::new());
        }

        Self {
            all_done_index: AtomicI32::new(-1),
            valid_count: AtomicI32::new(0),
            warmed_counts,
            executed_bitvec,
            ignite_ll,
            batch_size: BATCH_SIZE,
        }
    }

    /// Set the total number of valid tasks for this block.
    pub fn set_valid_count(&self, count: i32) {
        self.valid_count.store(count, Ordering::Release);
    }

    /// Register a dependency: when task `eei` completes, task `my_idx` should be ignited.
    ///
    /// If `eei == FIRST_FRAME`, the task has no dependencies and can execute immediately.
    /// Otherwise, `my_idx` is inserted into the ignition linked list at position `eei`.
    pub fn set_eei(&self, my_idx: i32, eei: i32) {
        if eei == FIRST_FRAME {
            return;
        }

        let eei_usize = eei as usize;
        // Atomically insert my_idx at the head of eei's ignite list.
        // old_head = swap(eei.to_ignite, my_idx)
        // my_idx.next = old_head
        let old_head = self.ignite_ll[eei_usize].to_ignite.swap(my_idx, Ordering::AcqRel);
        self.ignite_ll[my_idx as usize].next.store(old_head, Ordering::Release);
    }

    /// Mark a task as executed and update `all_done_index`.
    pub fn set_executed(&self, idx: i32) {
        let word_idx = idx as usize / 64;
        let bit_idx = idx as u64 % 64;
        self.executed_bitvec[word_idx].fetch_or(1u64 << bit_idx, Ordering::AcqRel);

        // Try to advance all_done_index
        self.try_advance_all_done(idx);
    }

    /// Check if a task has been executed.
    pub fn is_executed(&self, idx: i32) -> bool {
        let word_idx = idx as usize / 64;
        let bit_idx = idx as u64 % 64;
        (self.executed_bitvec[word_idx].load(Ordering::Acquire) >> bit_idx) & 1 == 1
    }

    /// Try to advance `all_done_index` as far as possible.
    ///
    /// Called after marking a task as executed. Scans forward from
    /// current `all_done_index + 1` to find the new highest contiguous index.
    fn try_advance_all_done(&self, _completed_idx: i32) {
        loop {
            let current = self.all_done_index.load(Ordering::Acquire);
            let next = current + 1;
            let valid = self.valid_count.load(Ordering::Acquire);

            if next >= valid {
                break;
            }

            if !self.is_executed(next) {
                break;
            }

            // Try to advance
            match self.all_done_index.compare_exchange(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully advanced, try again
                    continue;
                }
                Err(_) => {
                    // Someone else advanced it, retry from their value
                    continue;
                }
            }
        }
    }

    /// Get the list of tasks that should be ignited when task `idx` completes.
    ///
    /// Traverses the linked list at `ignite_ll[idx]` and returns all task indices.
    pub fn get_ignited_list(&self, idx: i32) -> Vec<i32> {
        let mut result = Vec::new();
        let idx_usize = idx as usize;

        // Atomically take the head of the list
        let head =
            self.ignite_ll[idx_usize].to_ignite.swap(EMPTY | DISPATCH_MASK, Ordering::AcqRel);

        if head == EMPTY || head < 0 {
            return result;
        }

        // Traverse the linked list
        let mut current = head;
        while current != EMPTY && current >= 0 {
            result.push(current);
            current = self.ignite_ll[current as usize].next.load(Ordering::Acquire);
        }

        result
    }

    /// Notify that a task's warmup is complete. Returns `Some((batch_end, is_last))`
    /// when the batch is fully warmed and ready for execution.
    pub fn notify_warmed(&self, idx: i32) -> Option<(i32, bool)> {
        let batch_idx = idx / self.batch_size;
        let batch_start = batch_idx * self.batch_size;
        let valid = self.valid_count.load(Ordering::Acquire);
        let batch_end = std::cmp::min(batch_start + self.batch_size, valid);
        let batch_task_count = batch_end - batch_start;

        let prev = self.warmed_counts[batch_idx as usize].fetch_add(1, Ordering::AcqRel);
        if prev + 1 == batch_task_count {
            let is_last = batch_end >= valid;
            Some((batch_end, is_last))
        } else {
            None
        }
    }

    /// Get the current `all_done_index`.
    pub fn get_all_done_index(&self) -> i32 {
        self.all_done_index.load(Ordering::Acquire)
    }

    /// Reset the dashboard for a new block.
    pub fn reset(&self, max_tasks: usize) {
        self.all_done_index.store(-1, Ordering::Release);
        self.valid_count.store(0, Ordering::Release);
        for wc in &self.warmed_counts {
            wc.store(0, Ordering::Release);
        }
        for bv in &self.executed_bitvec {
            bv.store(0, Ordering::Release);
        }
        for i in 0..max_tasks.min(self.ignite_ll.len()) {
            self.ignite_ll[i].to_ignite.store(EMPTY, Ordering::Release);
            self.ignite_ll[i].next.store(EMPTY, Ordering::Release);
        }
    }
}

// Dashboard only contains atomics and Vecs (which are Send+Sync when their contents are).
// AtomicI32, AtomicU64 are Send+Sync. The linked list uses only atomic operations for mutation.
unsafe impl Send for Dashboard {}
unsafe impl Sync for Dashboard {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_dashboard_creation() {
        let dash = Dashboard::new(100);
        assert_eq!(dash.get_all_done_index(), -1);
        assert_eq!(dash.valid_count.load(Ordering::Acquire), 0);
        // No task should be marked executed
        for i in 0..100 {
            assert!(!dash.is_executed(i));
        }
    }

    #[test]
    fn test_set_eei_first_frame() {
        let dash = Dashboard::new(10);
        // FIRST_FRAME means no dependency — should be a no-op
        dash.set_eei(3, FIRST_FRAME);
        // Task 3 should not appear in any ignite list (nothing at index -1)
        // Verify the ignite list at slot 3 is still empty
        assert_eq!(dash.ignite_ll[3].next.load(Ordering::Acquire), EMPTY);
    }

    #[test]
    fn test_set_eei_and_ignite() {
        let dash = Dashboard::new(10);
        // Task 1 depends on task 0
        dash.set_eei(1, 0);
        let ignited = dash.get_ignited_list(0);
        assert_eq!(ignited, vec![1]);
    }

    #[test]
    fn test_multiple_dependents() {
        let dash = Dashboard::new(10);
        // Tasks 1, 2, 3 all depend on task 0
        dash.set_eei(1, 0);
        dash.set_eei(2, 0);
        dash.set_eei(3, 0);

        let mut ignited = dash.get_ignited_list(0);
        ignited.sort();
        assert_eq!(ignited, vec![1, 2, 3]);
    }

    #[test]
    fn test_set_executed_and_bitvec() {
        let dash = Dashboard::new(200);
        dash.set_valid_count(200);

        assert!(!dash.is_executed(0));
        assert!(!dash.is_executed(65));
        assert!(!dash.is_executed(130));

        dash.set_executed(0);
        dash.set_executed(65);
        dash.set_executed(130);

        assert!(dash.is_executed(0));
        assert!(dash.is_executed(65));
        assert!(dash.is_executed(130));
        // Neighbors should not be affected
        assert!(!dash.is_executed(1));
        assert!(!dash.is_executed(64));
        assert!(!dash.is_executed(66));
    }

    #[test]
    fn test_all_done_index_advances() {
        let dash = Dashboard::new(10);
        dash.set_valid_count(10);

        dash.set_executed(0);
        assert_eq!(dash.get_all_done_index(), 0);

        dash.set_executed(1);
        assert_eq!(dash.get_all_done_index(), 1);

        dash.set_executed(2);
        assert_eq!(dash.get_all_done_index(), 2);
    }

    #[test]
    fn test_all_done_index_gap() {
        let dash = Dashboard::new(10);
        dash.set_valid_count(10);

        dash.set_executed(0);
        assert_eq!(dash.get_all_done_index(), 0);

        // Skip task 1, mark task 2
        dash.set_executed(2);
        // all_done should stay at 0 because task 1 is not done
        assert_eq!(dash.get_all_done_index(), 0);

        // Now fill the gap
        dash.set_executed(1);
        assert_eq!(dash.get_all_done_index(), 2);
    }

    #[test]
    fn test_notify_warmed_batch() {
        let dash = Dashboard::new(10);
        dash.set_valid_count(10);

        // With valid_count=10 and BATCH_SIZE=4096, all tasks are in batch 0.
        // Notify warmup for all 10 tasks.
        for i in 0..9 {
            assert_eq!(dash.notify_warmed(i), None);
        }
        // The 10th notification should trigger batch completion
        let result = dash.notify_warmed(9);
        assert!(result.is_some());
        let (batch_end, is_last) = result.unwrap();
        assert_eq!(batch_end, 10);
        assert!(is_last);
    }

    #[test]
    fn test_notify_warmed_partial() {
        let dash = Dashboard::new(100);
        dash.set_valid_count(100);

        // Only warm some tasks — should not trigger batch completion
        for i in 0..50 {
            assert_eq!(dash.notify_warmed(i), None);
        }
    }

    #[test]
    fn test_reset() {
        let dash = Dashboard::new(10);
        dash.set_valid_count(10);

        // Set up some state
        dash.set_eei(1, 0);
        dash.set_executed(0);
        dash.set_executed(1);

        // Reset
        dash.reset(10);

        assert_eq!(dash.get_all_done_index(), -1);
        assert_eq!(dash.valid_count.load(Ordering::Acquire), 0);
        assert!(!dash.is_executed(0));
        assert!(!dash.is_executed(1));
        // Ignite list should be cleared
        assert_eq!(dash.ignite_ll[0].to_ignite.load(Ordering::Acquire), EMPTY);
    }

    #[test]
    fn test_concurrent_set_executed() {
        let dash = Arc::new(Dashboard::new(1000));
        dash.set_valid_count(1000);

        let mut handles = Vec::new();
        // Spawn threads that each mark a range of tasks as executed
        for chunk_start in (0..1000).step_by(100) {
            let dash_clone = Arc::clone(&dash);
            let handle = std::thread::spawn(move || {
                for i in chunk_start..chunk_start + 100 {
                    dash_clone.set_executed(i);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // All tasks should be executed
        for i in 0..1000 {
            assert!(dash.is_executed(i), "task {} should be executed", i);
        }

        // all_done_index should be 999 (all contiguous)
        assert_eq!(dash.get_all_done_index(), 999);
    }
}
