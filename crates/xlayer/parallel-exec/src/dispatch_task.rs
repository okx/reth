//! DispatchTask: connects the ParallelDispatcher to ParallelBlockContext.
//!
//! Each DispatchTask wraps a shared [`ParallelBlockContext`] and a task index.
//! The dispatcher calls [`Dispatchable`] methods on these tasks to compute
//! dependencies (warmup) and execute transactions (execute).

use crate::{
    block_context::ParallelBlockContext,
    dashboard::{Dashboard, EARLY_EXE_WINDOW_SIZE, FIRST_FRAME},
    dispatcher_new::Dispatchable,
    task::ExeTask,
};
use std::sync::Arc;

/// A dispatchable task that executes via [`ParallelBlockContext`].
///
/// Cheap to clone (only an `Arc` bump and an `i32` copy), so the dispatcher
/// can freely create siblings for ignited dependents.
#[derive(Clone, Debug)]
pub struct DispatchTask {
    /// Shared block context.
    pub blk_ctx: Arc<ParallelBlockContext>,
    /// Task index within the block.
    pub idx: i32,
}

impl DispatchTask {
    /// Create a new dispatch task.
    pub fn new(blk_ctx: Arc<ParallelBlockContext>, idx: i32) -> Self {
        Self { blk_ctx, idx }
    }
}

impl Dispatchable for DispatchTask {
    /// Compute the Earliest Execution Index (EEI) via backward collision scan.
    ///
    /// Scans earlier tasks (within EARLY_EXE_WINDOW_SIZE) for read-write
    /// collisions. Returns the index of the latest conflicting task, or
    /// FIRST_FRAME if no dependency exists.
    fn warm_up(&self) -> i32 {
        let my_idx = self.idx as usize;

        let task_guard = self.blk_ctx.tasks_manager.task_for_read(my_idx);
        let task = match task_guard.as_ref() {
            Some(t) => t,
            None => return FIRST_FRAME,
        };

        let task_out_start = task.get_task_out_start();
        if task_out_start == 0 {
            return FIRST_FRAME;
        }

        // Backward scan window
        let stop = if my_idx > EARLY_EXE_WINDOW_SIZE { my_idx - EARLY_EXE_WINDOW_SIZE } else { 0 };

        let mut eei = if stop == 0 { FIRST_FRAME } else { (stop - 1) as i32 };

        for earlier_idx in (stop..task_out_start).rev() {
            let other_guard = self.blk_ctx.tasks_manager.task_for_read(earlier_idx);
            if let Some(other) = other_guard.as_ref() {
                if ExeTask::has_collision(task, other) {
                    eei = earlier_idx as i32;
                    break;
                }
            }
        }

        drop(task_guard);
        eei
    }

    fn execute(&self) {
        self.blk_ctx.execute_task(self.idx as usize);
    }

    fn get_dashboard(&self) -> &Dashboard {
        &self.blk_ctx.dashboard
    }

    fn get_idx(&self) -> i32 {
        self.idx
    }

    fn get_sibling(&self, idx: i32) -> Self {
        DispatchTask { blk_ctx: self.blk_ctx.clone(), idx }
    }

    fn end_block(&self) {
        // Reserved for future cleanup (e.g., flushing caches to BundleState)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crw_sets::CrwSets,
        parallel_state_cache::ParallelStateCache,
        task::{ExeTask, SimResult},
    };
    use revm::context::{BlockEnv, CfgEnv};

    fn make_sim_result_with_writes(index: usize, writes: Vec<[u8; 10]>) -> SimResult {
        SimResult {
            crw_sets: CrwSets {
                account_reads: vec![],
                account_writes: writes,
                storage_reads: vec![],
                storage_writes: vec![],
            },
            original_index: index,
            success: true,
        }
    }

    fn make_ctx(max_tasks: usize) -> Arc<ParallelBlockContext> {
        Arc::new(ParallelBlockContext::new(
            max_tasks,
            Arc::new(ParallelStateCache::new()),
            None,
            BlockEnv::default(),
            CfgEnv::default(),
        ))
    }

    #[test]
    fn test_dispatch_task_creation() {
        let ctx = make_ctx(10);
        let dt = DispatchTask::new(ctx.clone(), 3);
        assert_eq!(dt.idx, 3);
        assert_eq!(dt.get_idx(), 3);
    }

    #[test]
    fn test_warmup_no_task() {
        let ctx = make_ctx(10);
        let dt = DispatchTask::new(ctx, 0);
        // No task set -> returns FIRST_FRAME
        assert_eq!(dt.warm_up(), FIRST_FRAME);
    }

    #[test]
    fn test_warmup_no_dependencies() {
        let ctx = make_ctx(10);
        // Task with task_out_start = 0 (first frame)
        let task = ExeTask::new(make_sim_result_with_writes(0, vec![[1u8; 10]]));
        ctx.tasks_manager.set_task(0, task);

        let dt = DispatchTask::new(ctx, 0);
        assert_eq!(dt.warm_up(), FIRST_FRAME);
    }

    #[test]
    fn test_warmup_with_collision() {
        let ctx = make_ctx(10);

        // Task 0: writes [1u8; 10]
        let task0 = ExeTask::new(make_sim_result_with_writes(0, vec![[1u8; 10]]));
        ctx.tasks_manager.set_task(0, task0);

        // Task 1: also writes [1u8; 10], task_out_start = 1 (from frame after task 0)
        let task1 = ExeTask::new(make_sim_result_with_writes(1, vec![[1u8; 10]]));
        task1.set_task_out_start(1);
        ctx.tasks_manager.set_task(1, task1);

        let dt = DispatchTask::new(ctx, 1);
        let eei = dt.warm_up();
        // Should find collision with task 0
        assert_eq!(eei, 0);
    }

    #[test]
    fn test_warmup_no_collision() {
        let ctx = make_ctx(10);

        // Task 0: writes [1u8; 10]
        let task0 = ExeTask::new(make_sim_result_with_writes(0, vec![[1u8; 10]]));
        ctx.tasks_manager.set_task(0, task0);

        // Task 1: writes [2u8; 10] (different), task_out_start = 1
        let task1 = ExeTask::new(make_sim_result_with_writes(1, vec![[2u8; 10]]));
        task1.set_task_out_start(1);
        ctx.tasks_manager.set_task(1, task1);

        let dt = DispatchTask::new(ctx, 1);
        let eei = dt.warm_up();
        // No collision -> FIRST_FRAME
        assert_eq!(eei, FIRST_FRAME);
    }

    #[test]
    fn test_get_sibling() {
        let ctx = make_ctx(10);
        let dt = DispatchTask::new(ctx, 0);
        let sibling = dt.get_sibling(5);
        assert_eq!(sibling.get_idx(), 5);
        // Shares the same block context
        assert!(Arc::ptr_eq(&dt.blk_ctx, &sibling.blk_ctx));
    }

    #[test]
    fn test_get_dashboard() {
        let ctx = make_ctx(10);
        let dt = DispatchTask::new(ctx.clone(), 0);
        // Should return reference to the context's dashboard
        let dash = dt.get_dashboard();
        assert_eq!(dash.get_all_done_index(), -1);
    }
}
