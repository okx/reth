//! True parallel dispatcher with warmup and execution phases.
//!
//! Inspired by fafo's dispatcher architecture:
//! - Warmup phase: compute EEI (Earliest Execution Index) for dependency tracking
//! - Execution phase: parallel execution via rayon thread pool
//! - Cascade: completed tasks "ignite" their dependents via the Dashboard
//!
//! Tasks flow: Framer -> Dispatcher.execute_block() -> warmup -> execute -> ignite dependents

use crate::dashboard::{Dashboard, FIRST_FRAME};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// A raw pointer wrapper that implements `Send` and `Sync`.
///
/// # Safety
/// The caller must guarantee that the pointee outlives all uses of this wrapper.
/// In this module, that invariant is upheld by `std::thread::scope` in `execute_block`:
/// the scope blocks until all spawned work finishes, and the pointee lives on the
/// caller's stack (or in `&self`) which outlives the scope.
struct SendPtr<T>(*const T);

// Safety: the lifetime guarantee described above ensures no data races.
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<T> Copy for SendPtr<T> {}

impl<T> SendPtr<T> {
    fn new(r: &T) -> Self {
        Self(r as *const T)
    }

    /// Dereference the pointer.
    ///
    /// # Safety
    /// The pointee must still be alive and the pointer must be valid.
    unsafe fn as_ref(&self) -> &T {
        unsafe { &*self.0 }
    }
}

/// Trait for dispatchable tasks.
///
/// Implementors define how to warm up (compute EEI) and execute a task.
/// The dispatcher is generic over this trait so different execution backends
/// can be plugged in.
pub trait Dispatchable: Send + Sync + 'static {
    /// Compute the Earliest Execution Index for this task.
    /// Returns `FIRST_FRAME` (-1) if no dependencies exist.
    fn warm_up(&self) -> i32;

    /// Execute this task (called from the rayon thread pool).
    fn execute(&self);

    /// Get the dashboard for this block.
    fn get_dashboard(&self) -> &Dashboard;

    /// Get this task's index.
    fn get_idx(&self) -> i32;

    /// Create a sibling task with a different index.
    /// Used to create tasks for ignited dependents.
    fn get_sibling(&self, idx: i32) -> Self;

    /// Called when the block is complete.
    fn end_block(&self);
}

/// True parallel dispatcher with warmup and execution phases.
///
/// Owns two rayon thread pools:
/// - `warmup_pool`: computes EEI (dependency info) for each task
/// - `exe_pool`: executes tasks once their dependencies are satisfied
///
/// The execution cascade works via the Dashboard's ignition linked lists:
/// when a task completes, it checks for dependents and spawns them.
pub struct ParallelDispatcher {
    /// Thread pool for warmup phase (EEI computation).
    warmup_pool: rayon::ThreadPool,
    /// Thread pool for execution phase.
    exe_pool: rayon::ThreadPool,
    /// Number of warmup threads.
    num_warmup_threads: usize,
    /// Number of execution threads.
    num_exe_threads: usize,
    /// Whether the dispatcher is currently processing a block.
    active: Arc<AtomicBool>,
}

impl ParallelDispatcher {
    /// Create a new dispatcher with the given thread counts.
    pub fn new(num_warmup_threads: usize, num_exe_threads: usize) -> Self {
        let warmup_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_warmup_threads)
            .thread_name(|i| format!("warmup-{i}"))
            .build()
            .expect("failed to build warmup thread pool");

        let exe_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_exe_threads)
            .thread_name(|i| format!("exe-{i}"))
            .build()
            .expect("failed to build execution thread pool");

        Self {
            warmup_pool,
            exe_pool,
            num_warmup_threads,
            num_exe_threads,
            active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Get number of warmup threads.
    pub fn num_warmup_threads(&self) -> usize {
        self.num_warmup_threads
    }

    /// Get number of execution threads.
    pub fn num_exe_threads(&self) -> usize {
        self.num_exe_threads
    }

    /// Run a closure on the execution thread pool.
    ///
    /// Allows callers to leverage the dispatcher's pre-built rayon pool
    /// for parallel work (e.g., intra-frame parallel execution in the
    /// pipeline's MVP mode).
    pub fn exe_pool_install<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R + Send,
        R: Send,
    {
        self.exe_pool.install(f)
    }

    /// Execute all tasks for a block.
    ///
    /// This is the main entry point. Each task goes through:
    /// 1. Warmup: compute EEI and register dependency in Dashboard
    /// 2. Execution: dispatched to rayon pool when dependencies are met
    /// 3. Ignition: completed tasks spawn their dependents
    ///
    /// Returns when all tasks are complete.
    pub fn execute_block<D: Dispatchable + Clone>(&self, tasks: Vec<D>, dashboard: &Dashboard) {
        if tasks.is_empty() {
            return;
        }

        let task_count = tasks.len() as i32;
        dashboard.set_valid_count(task_count);
        self.active.store(true, Ordering::Release);

        // Wrap pointers in SendPtr so they can cross thread boundaries.
        // Safety: std::thread::scope below guarantees all spawned work completes
        // before this function returns, so both &self.exe_pool and &dashboard
        // remain valid for the duration of all spawned closures.
        let ep = SendPtr::new(&self.exe_pool);
        let dp = SendPtr::new(dashboard);

        std::thread::scope(|scope| {
            // Coordinator thread: polls all_done_index until all tasks complete.
            let active = self.active.clone();
            scope.spawn(move || {
                loop {
                    let db = unsafe { dp.as_ref() };
                    let all_done = db.get_all_done_index();
                    if all_done >= task_count - 1 {
                        break;
                    }
                    std::thread::yield_now();
                }
                active.store(false, Ordering::Release);
            });

            // Submit all tasks for warmup -> execution
            for task in tasks {
                self.warmup_pool.spawn(move || {
                    let dashboard = unsafe { dp.as_ref() };
                    let exe_pool = unsafe { ep.as_ref() };

                    let eei = task.warm_up();
                    let my_idx = task.get_idx();

                    // Register dependency in the Dashboard's ignition linked list
                    dashboard.set_eei(my_idx, eei);
                    dashboard.notify_warmed(my_idx);

                    if eei == FIRST_FRAME {
                        // No dependencies: execute immediately
                        Self::spawn_execute(ep, dp, exe_pool, task);
                    }
                    // Tasks with dependencies will be ignited when their
                    // dependency completes (via the cascade in spawn_execute)
                });
            }
        });
    }

    /// Spawn a task for execution and cascade ignition to dependents.
    ///
    /// After executing, retrieves the ignited list from the Dashboard
    /// and recursively spawns dependent tasks.
    fn spawn_execute<D: Dispatchable + Clone>(
        ep: SendPtr<rayon::ThreadPool>,
        dp: SendPtr<Dashboard>,
        exe_pool: &rayon::ThreadPool,
        task: D,
    ) {
        exe_pool.spawn(move || {
            let my_idx = task.get_idx();
            task.execute();

            // Safety: guaranteed by scope lifetime in execute_block
            let dashboard = unsafe { dp.as_ref() };
            let exe_pool = unsafe { ep.as_ref() };

            dashboard.set_executed(my_idx);

            // Cascade: ignite any tasks that were waiting on this one
            let ignited = dashboard.get_ignited_list(my_idx);
            for dep_idx in ignited {
                let sibling = task.get_sibling(dep_idx);
                Self::spawn_execute(ep, dp, exe_pool, sibling);
            }
        });
    }
}

impl std::fmt::Debug for ParallelDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelDispatcher")
            .field("num_warmup_threads", &self.num_warmup_threads)
            .field("num_exe_threads", &self.num_exe_threads)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI32;

    /// Mock task that records execution order and supports dependency chains.
    #[derive(Clone)]
    struct MockTask {
        idx: i32,
        /// Predetermined EEI (set by test to control dependency graph).
        eei: i32,
        dashboard: Arc<Dashboard>,
        /// Shared execution log: tasks append their idx on execution.
        execution_log: Arc<parking_lot::Mutex<Vec<i32>>>,
        /// Map from task index to predetermined EEI (for sibling creation).
        eei_map: Arc<Vec<AtomicI32>>,
    }

    impl MockTask {
        fn new(
            idx: i32,
            eei: i32,
            dashboard: Arc<Dashboard>,
            execution_log: Arc<parking_lot::Mutex<Vec<i32>>>,
            eei_map: Arc<Vec<AtomicI32>>,
        ) -> Self {
            eei_map[idx as usize].store(eei, Ordering::Release);
            Self { idx, eei, dashboard, execution_log, eei_map }
        }
    }

    impl Dispatchable for MockTask {
        fn warm_up(&self) -> i32 {
            self.eei
        }

        fn execute(&self) {
            self.execution_log.lock().push(self.idx);
        }

        fn get_dashboard(&self) -> &Dashboard {
            &self.dashboard
        }

        fn get_idx(&self) -> i32 {
            self.idx
        }

        fn get_sibling(&self, idx: i32) -> Self {
            let eei = self.eei_map[idx as usize].load(Ordering::Acquire);
            Self {
                idx,
                eei,
                dashboard: self.dashboard.clone(),
                execution_log: self.execution_log.clone(),
                eei_map: self.eei_map.clone(),
            }
        }

        fn end_block(&self) {
            // No-op for tests
        }
    }

    fn make_eei_map(size: usize) -> Arc<Vec<AtomicI32>> {
        let mut v = Vec::with_capacity(size);
        for _ in 0..size {
            v.push(AtomicI32::new(FIRST_FRAME));
        }
        Arc::new(v)
    }

    #[test]
    fn test_dispatcher_creation() {
        let d = ParallelDispatcher::new(4, 8);
        assert_eq!(d.num_warmup_threads(), 4);
        assert_eq!(d.num_exe_threads(), 8);
    }

    #[test]
    fn test_execute_empty_block() {
        let d = ParallelDispatcher::new(2, 4);
        let dashboard = Dashboard::new(100);
        let tasks: Vec<MockTask> = vec![];
        // Should return immediately without panic
        d.execute_block(tasks, &dashboard);
    }

    #[test]
    fn test_execute_independent_tasks() {
        let d = ParallelDispatcher::new(4, 4);
        let dashboard = Arc::new(Dashboard::new(100));
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let eei_map = make_eei_map(10);

        let tasks: Vec<MockTask> = (0..10)
            .map(|i| MockTask::new(i, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()))
            .collect();

        d.execute_block(tasks, &dashboard);

        let executed = log.lock().clone();
        assert_eq!(executed.len(), 10, "all 10 tasks should have executed");

        // All tasks should appear exactly once
        let mut sorted = executed.clone();
        sorted.sort();
        assert_eq!(sorted, (0..10).collect::<Vec<i32>>());
    }

    #[test]
    fn test_execute_sequential_chain() {
        // Dependency chain: 0 <- 1 <- 2 <- 3
        // Task 0: no deps, Task 1: depends on 0, Task 2: depends on 1, etc.
        let d = ParallelDispatcher::new(2, 2);
        let dashboard = Arc::new(Dashboard::new(100));
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let eei_map = make_eei_map(4);

        let tasks = vec![
            MockTask::new(0, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()),
            MockTask::new(1, 0, dashboard.clone(), log.clone(), eei_map.clone()),
            MockTask::new(2, 1, dashboard.clone(), log.clone(), eei_map.clone()),
            MockTask::new(3, 2, dashboard.clone(), log.clone(), eei_map.clone()),
        ];

        d.execute_block(tasks, &dashboard);

        let executed = log.lock().clone();
        assert_eq!(executed.len(), 4, "all 4 tasks should have executed");

        // Verify ordering: each task must execute after its dependency.
        let pos = |idx: i32| executed.iter().position(|&x| x == idx).unwrap();
        assert!(pos(0) < pos(1), "task 0 must execute before task 1");
        assert!(pos(1) < pos(2), "task 1 must execute before task 2");
        assert!(pos(2) < pos(3), "task 2 must execute before task 3");
    }

    #[test]
    fn test_execute_diamond_dependency() {
        // Diamond: A(0) -> B(1), A(0) -> C(2), B(1) -> D(3), C(2) -> D(3)
        //
        // D depends on whichever of B,C completes last. Since Dashboard
        // set_eei only supports a single EEI (not multiple), we model this
        // as D depending on B (idx=1). C is independent of D for the
        // Dashboard's purposes -- the higher-level framer handles multi-dep.
        let d = ParallelDispatcher::new(2, 4);
        let dashboard = Arc::new(Dashboard::new(100));
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let eei_map = make_eei_map(4);

        let tasks = vec![
            MockTask::new(0, FIRST_FRAME, dashboard.clone(), log.clone(), eei_map.clone()),
            MockTask::new(1, 0, dashboard.clone(), log.clone(), eei_map.clone()),
            MockTask::new(2, 0, dashboard.clone(), log.clone(), eei_map.clone()),
            MockTask::new(3, 1, dashboard.clone(), log.clone(), eei_map.clone()),
        ];

        d.execute_block(tasks, &dashboard);

        let executed = log.lock().clone();
        assert_eq!(executed.len(), 4, "all 4 tasks should have executed");

        let pos = |idx: i32| executed.iter().position(|&x| x == idx).unwrap();
        // A must execute before B and C
        assert!(pos(0) < pos(1), "A(0) must execute before B(1)");
        assert!(pos(0) < pos(2), "A(0) must execute before C(2)");
        // B must execute before D (D's EEI is B)
        assert!(pos(1) < pos(3), "B(1) must execute before D(3)");
    }
}
