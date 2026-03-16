use mptdb_traits::ss::StateStore;
use rand::Rng as _;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Once,
    },
    thread,
    time::Duration,
};
use tracing::{error, info};

/// Background pruning manager. Periodically prunes old versions with random
/// jitter so that multiple nodes don't all prune at the same instant.
pub struct PruningManager {
    store: Arc<dyn StateStore>,
    keep_recent: i64,
    prune_interval: i64,
    start_once: Once,
    stop_flag: Arc<AtomicBool>,
    worker_handle: Option<thread::JoinHandle<()>>,
}

impl PruningManager {
    /// Create a new pruning manager.
    ///
    /// * `store` – the state store to prune.
    /// * `keep_recent` – number of recent versions to keep.
    /// * `prune_interval` – base interval between prune cycles, in seconds.
    pub fn new(store: Arc<dyn StateStore>, keep_recent: i64, prune_interval: i64) -> Self {
        Self {
            store,
            keep_recent,
            prune_interval,
            start_once: Once::new(),
            stop_flag: Arc::new(AtomicBool::new(false)),
            worker_handle: None,
        }
    }

    /// Start the background pruning thread (at most once).
    ///
    /// If `keep_recent <= 0` or `prune_interval <= 0` the manager is
    /// considered disabled and no thread is spawned.
    pub fn start(&mut self) {
        // Capture values before moving into the Once closure.
        let keep_recent = self.keep_recent;
        let prune_interval = self.prune_interval;
        let store = Arc::clone(&self.store);
        let stop_flag = Arc::clone(&self.stop_flag);

        // We need a way to smuggle the JoinHandle out of the Once closure.
        let mut handle: Option<thread::JoinHandle<()>> = None;

        self.start_once.call_once(|| {
            if keep_recent <= 0 || prune_interval <= 0 {
                return;
            }
            handle = Some(thread::spawn(move || {
                Self::prune_loop(store, keep_recent, prune_interval, stop_flag);
            }));
        });

        if handle.is_some() {
            self.worker_handle = handle;
        }
    }

    /// Stop the background pruning thread and wait for it to exit.
    /// Safe to call multiple times.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
    }

    /// Returns `true` when the background thread is alive and has not been
    /// asked to stop.
    pub fn is_running(&self) -> bool {
        self.worker_handle.is_some() && !self.stop_flag.load(Ordering::SeqCst)
    }

    /// Core prune loop executed on the background thread.
    fn prune_loop(
        store: Arc<dyn StateStore>,
        keep_recent: i64,
        prune_interval: i64,
        stop_flag: Arc<AtomicBool>,
    ) {
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                info!("Pruning manager stopped");
                return;
            }

            let latest = store.get_latest_version();
            let prune_version = latest - keep_recent;

            if prune_version > store.get_earliest_version() {
                let start = std::time::Instant::now();
                if let Err(err) = store.prune(prune_version) {
                    error!(?err, version = prune_version, "failed to prune versions");
                } else {
                    info!(
                        version = prune_version,
                        elapsed = ?start.elapsed(),
                        "pruned state store"
                    );
                }
            }

            // Random jitter: sleep for interval + rand(0..interval) seconds.
            let jitter = rand::rng().random_range(0..prune_interval.max(1));
            let total_secs = prune_interval + jitter;

            // Sleep in 1-second segments for responsive shutdown.
            for _ in 0..total_secs {
                if stop_flag.load(Ordering::SeqCst) {
                    info!("Pruning manager stopped");
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

impl Drop for PruningManager {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;
    use mptdb_common::error::Result;
    use mptdb_traits::{iterator::DbIterator, types::SnapshotNode};
    use std::sync::atomic::AtomicI64;

    /// Minimal mock state store for pruning tests.
    struct MockStateStore {
        latest_version: AtomicI64,
        earliest_version: AtomicI64,
        prune_called_up_to: AtomicI64,
    }

    impl MockStateStore {
        fn new(earliest: i64, latest: i64) -> Self {
            Self {
                latest_version: AtomicI64::new(latest),
                earliest_version: AtomicI64::new(earliest),
                prune_called_up_to: AtomicI64::new(0),
            }
        }
    }

    impl StateStore for MockStateStore {
        fn get(&self, _: &str, _: i64, _: &[u8]) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn has(&self, _: &str, _: i64, _: &[u8]) -> Result<bool> {
            Ok(false)
        }

        fn iterator(&self, _: &str, _: i64, _: &[u8], _: &[u8]) -> Result<Box<dyn DbIterator>> {
            unimplemented!()
        }

        fn reverse_iterator(
            &self,
            _: &str,
            _: i64,
            _: &[u8],
            _: &[u8],
        ) -> Result<Box<dyn DbIterator>> {
            unimplemented!()
        }

        fn raw_iterate(
            &self,
            _: &str,
            _: &mut dyn FnMut(&[u8], &[u8], i64) -> bool,
        ) -> Result<bool> {
            Ok(false)
        }

        fn get_latest_version(&self) -> i64 {
            self.latest_version.load(Ordering::SeqCst)
        }

        fn set_latest_version(&self, version: i64) -> Result<()> {
            self.latest_version.store(version, Ordering::SeqCst);
            Ok(())
        }

        fn get_earliest_version(&self) -> i64 {
            self.earliest_version.load(Ordering::SeqCst)
        }

        fn set_earliest_version(&self, version: i64, _ignore_version: bool) -> Result<()> {
            self.earliest_version.store(version, Ordering::SeqCst);
            Ok(())
        }

        fn apply_changeset_sync(&self, _: i64, _: &[mptdb_proto::NamedChangeSet]) -> Result<()> {
            Ok(())
        }

        fn apply_changeset_async(&self, _: i64, _: &[mptdb_proto::NamedChangeSet]) -> Result<()> {
            Ok(())
        }

        fn prune(&self, version: i64) -> Result<()> {
            self.prune_called_up_to.store(version, Ordering::SeqCst);
            // Advance earliest_version to simulate real pruning.
            self.earliest_version.store(version, Ordering::SeqCst);
            Ok(())
        }

        fn import(&self, _: i64, _: Receiver<SnapshotNode>) -> Result<()> {
            Ok(())
        }

        fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_pruning_manager_start_stop() {
        let store: Arc<dyn StateStore> = Arc::new(MockStateStore::new(0, 100));
        let mut mgr = PruningManager::new(store, 10, 600);
        mgr.start();
        assert!(mgr.is_running());
        mgr.stop();
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_pruning_manager_stop_idempotent() {
        let store: Arc<dyn StateStore> = Arc::new(MockStateStore::new(0, 100));
        let mut mgr = PruningManager::new(store, 10, 600);
        mgr.start();
        mgr.stop();
        // Second stop should not panic.
        mgr.stop();
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_pruning_manager_start_idempotent() {
        let store: Arc<dyn StateStore> = Arc::new(MockStateStore::new(0, 100));
        let mut mgr = PruningManager::new(store, 10, 600);
        mgr.start();
        // Second start should be a no-op; only one thread.
        mgr.start();
        assert!(mgr.is_running());
        mgr.stop();
    }

    #[test]
    fn test_pruning_manager_disabled() {
        // keep_recent = 0 means pruning is disabled.
        let store: Arc<dyn StateStore> = Arc::new(MockStateStore::new(0, 100));
        let mut mgr = PruningManager::new(store, 0, 10);
        mgr.start();
        assert!(!mgr.is_running());

        // prune_interval = 0 also disables pruning.
        let store2: Arc<dyn StateStore> = Arc::new(MockStateStore::new(0, 100));
        let mut mgr2 = PruningManager::new(store2, 10, 0);
        mgr2.start();
        assert!(!mgr2.is_running());
    }

    #[test]
    fn test_pruning_manager_actually_prunes() {
        let mock = Arc::new(MockStateStore::new(0, 200));
        let store: Arc<dyn StateStore> = Arc::clone(&mock) as Arc<dyn StateStore>;

        // Use a very short prune interval (1 second) so the loop runs fast.
        // The prune_loop sleeps in 1-second segments, so with interval=1 the
        // total sleep is 1 + rand(0..1) = 1 second.
        let mut mgr = PruningManager::new(store, 10, 1);
        mgr.start();

        // Wait enough time for at least one prune cycle.
        thread::sleep(Duration::from_millis(500));

        // The mock store should have been pruned: earliest_version should
        // have advanced to latest(200) - keep_recent(10) = 190.
        let earliest = mock.earliest_version.load(Ordering::SeqCst);
        let pruned_up_to = mock.prune_called_up_to.load(Ordering::SeqCst);

        assert!(earliest > 0, "expected earliest_version to advance, got {earliest}");
        assert_eq!(pruned_up_to, 190, "expected prune up to 190, got {pruned_up_to}");

        mgr.stop();
    }
}
