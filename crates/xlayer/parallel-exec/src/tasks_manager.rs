//! Thread-safe task storage for parallel execution.
//!
//! [`TasksManager`] provides indexed access to execution tasks with
//! concurrent read support (for warmup/EEI computation) and exclusive
//! write support (for task execution and result storage).

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::task::ExeTask;

/// Thread-safe indexed storage for execution tasks.
///
/// Each slot holds an `Option<ExeTask>`, protected by a `RwLock` for
/// fine-grained concurrent access. Multiple warmup threads can read
/// different task slots simultaneously, while execution threads take
/// exclusive write locks on their assigned slots.
pub struct TasksManager {
    /// Per-task storage with RwLock for concurrent access.
    tasks: Vec<RwLock<Option<ExeTask>>>,
    /// Number of tasks that have been set (for iteration bounds).
    count: AtomicUsize,
}

impl TasksManager {
    /// Create a new `TasksManager` with capacity for `size` tasks.
    pub fn with_size(size: usize) -> Self {
        let mut tasks = Vec::with_capacity(size);
        for _ in 0..size {
            tasks.push(RwLock::new(None));
        }
        Self { tasks, count: AtomicUsize::new(0) }
    }

    /// Get a read lock on a task slot.
    ///
    /// Used during warmup/EEI computation to check task dependencies
    /// without blocking other readers.
    pub fn task_for_read(&self, idx: usize) -> RwLockReadGuard<'_, Option<ExeTask>> {
        self.tasks[idx].read()
    }

    /// Get a write lock on a task slot.
    ///
    /// Used during task execution to take ownership of the task
    /// and store results.
    pub fn task_for_write(&self, idx: usize) -> RwLockWriteGuard<'_, Option<ExeTask>> {
        self.tasks[idx].write()
    }

    /// Set a task at the given index.
    ///
    /// Called by the Framer when flushing frames to assign tasks to slots.
    pub fn set_task(&self, idx: usize, task: ExeTask) {
        *self.tasks[idx].write() = Some(task);
        // Update count if this is a new highest index
        loop {
            let current = self.count.load(Ordering::Acquire);
            if idx + 1 <= current {
                break;
            }
            match self.count.compare_exchange(current, idx + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
    }

    /// Take a task from a slot, leaving `None` in its place.
    ///
    /// Used by the executor to take ownership before execution.
    pub fn take_task(&self, idx: usize) -> Option<ExeTask> {
        self.tasks[idx].write().take()
    }

    /// Check if a task slot is occupied.
    pub fn has_task(&self, idx: usize) -> bool {
        self.tasks[idx].read().is_some()
    }

    /// Return the number of tasks that have been set.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Return the total capacity.
    pub fn capacity(&self) -> usize {
        self.tasks.len()
    }

    /// Reset all slots to `None`.
    pub fn reset(&self) {
        for task in &self.tasks {
            *task.write() = None;
        }
        self.count.store(0, Ordering::Release);
    }
}

impl std::fmt::Debug for TasksManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TasksManager")
            .field("capacity", &self.tasks.len())
            .field("count", &self.count.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crw_sets::CrwSets,
        task::{ExeTask, SimResult},
    };

    fn make_sim_result(index: usize) -> SimResult {
        SimResult { crw_sets: CrwSets::default(), original_index: index, success: true }
    }

    fn make_task(index: usize) -> ExeTask {
        ExeTask::new(make_sim_result(index))
    }

    #[test]
    fn test_tasks_manager_creation() {
        let mgr = TasksManager::with_size(10);
        assert_eq!(mgr.capacity(), 10);
        assert_eq!(mgr.count(), 0);
        for i in 0..10 {
            assert!(!mgr.has_task(i));
        }
    }

    #[test]
    fn test_set_and_read_task() {
        let mgr = TasksManager::with_size(5);
        let task = make_task(42);
        mgr.set_task(2, task);

        let guard = mgr.task_for_read(2);
        let t = guard.as_ref().expect("task should be present");
        assert_eq!(t.sim_results[0].original_index, 42);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn test_take_task() {
        let mgr = TasksManager::with_size(3);
        mgr.set_task(1, make_task(7));

        let taken = mgr.take_task(1);
        assert!(taken.is_some());
        assert_eq!(taken.unwrap().sim_results[0].original_index, 7);

        // Slot should now be empty
        assert!(!mgr.has_task(1));
        assert!(mgr.take_task(1).is_none());
    }

    #[test]
    fn test_has_task() {
        let mgr = TasksManager::with_size(4);
        assert!(!mgr.has_task(0));
        assert!(!mgr.has_task(3));

        mgr.set_task(0, make_task(0));
        assert!(mgr.has_task(0));
        assert!(!mgr.has_task(1));
        assert!(!mgr.has_task(3));
    }

    #[test]
    fn test_count_tracking() {
        let mgr = TasksManager::with_size(10);
        assert_eq!(mgr.count(), 0);

        mgr.set_task(0, make_task(0));
        assert_eq!(mgr.count(), 1);

        mgr.set_task(5, make_task(5));
        assert_eq!(mgr.count(), 6);

        // Setting a lower index should not decrease count
        mgr.set_task(3, make_task(3));
        assert_eq!(mgr.count(), 6);

        mgr.set_task(9, make_task(9));
        assert_eq!(mgr.count(), 10);
    }

    #[test]
    fn test_reset() {
        let mgr = TasksManager::with_size(5);
        mgr.set_task(0, make_task(0));
        mgr.set_task(2, make_task(2));
        mgr.set_task(4, make_task(4));
        assert_eq!(mgr.count(), 5);

        mgr.reset();
        assert_eq!(mgr.count(), 0);
        for i in 0..5 {
            assert!(!mgr.has_task(i));
        }
    }

    #[test]
    fn test_concurrent_reads() {
        use std::sync::Arc;

        let mgr = Arc::new(TasksManager::with_size(8));
        for i in 0..8 {
            mgr.set_task(i, make_task(i));
        }

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let mgr = Arc::clone(&mgr);
                std::thread::spawn(move || {
                    let guard = mgr.task_for_read(i);
                    let t = guard.as_ref().expect("task should be present");
                    assert_eq!(t.sim_results[0].original_index, i);
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread should not panic");
        }
    }

    #[test]
    fn test_set_task_updates_count_correctly() {
        let mgr = TasksManager::with_size(10);

        // Set tasks out of order
        mgr.set_task(7, make_task(7));
        assert_eq!(mgr.count(), 8);

        mgr.set_task(2, make_task(2));
        assert_eq!(mgr.count(), 8); // still 8, not 3

        mgr.set_task(9, make_task(9));
        assert_eq!(mgr.count(), 10);

        mgr.set_task(0, make_task(0));
        assert_eq!(mgr.count(), 10); // still 10
    }
}
