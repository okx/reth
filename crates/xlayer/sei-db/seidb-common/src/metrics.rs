use std::{collections::HashMap, time::Instant};

/// PhaseTimer tracks time spent in different phases of execution.
///
/// Usage: create a timer, call `set_phase` to switch between named phases,
/// and inspect `durations()` to see accumulated time per phase.
pub struct PhaseTimer {
    name: String,
    current_phase: Option<String>,
    phase_start: Instant,
    durations: HashMap<String, std::time::Duration>,
}

impl PhaseTimer {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            current_phase: None,
            phase_start: Instant::now(),
            durations: HashMap::new(),
        }
    }

    /// Switch to a new phase. Records elapsed time for the previous phase.
    pub fn set_phase(&mut self, phase: &str) {
        let now = Instant::now();
        if let Some(prev) = self.current_phase.take() {
            let elapsed = now - self.phase_start;
            *self.durations.entry(prev).or_default() += elapsed;
        }
        self.current_phase = Some(phase.to_string());
        self.phase_start = now;
    }

    /// End the current phase and clear all recorded durations for reuse.
    pub fn reset(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.current_phase.take() {
            let elapsed = now - self.phase_start;
            *self.durations.entry(prev).or_default() += elapsed;
        }
        self.durations.clear();
    }

    /// Get the timer name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get recorded durations for all phases.
    pub fn durations(&self) -> &HashMap<String, std::time::Duration> {
        &self.durations
    }
}

/// Database operation metrics counters.
///
/// Mirrors the Go `otelMetrics` struct from PebbleDB's MVCC layer. Thread-safe
/// via atomics so any number of readers/writers can record concurrently.
/// Collected by external monitoring systems or sampled via [`DbMetrics::snapshot`].
pub mod db_metrics {
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    /// Atomic counters for every instrumented DB operation.
    #[derive(Default)]
    pub struct DbMetrics {
        // -- Read metrics --
        pub get_count: AtomicU64,
        pub get_latency_ns_total: AtomicU64,
        pub get_miss_count: AtomicU64,

        // -- Write metrics --
        pub set_count: AtomicU64,
        pub changeset_apply_count: AtomicU64,
        pub changeset_apply_latency_ns_total: AtomicU64,

        // -- Iterator metrics --
        pub iterator_count: AtomicU64,
        pub iterator_next_count: AtomicU64,

        // -- Prune metrics --
        pub prune_count: AtomicU64,
        pub prune_latency_ns_total: AtomicU64,

        // -- Import metrics --
        pub import_count: AtomicU64,
        pub import_latency_ns_total: AtomicU64,

        // -- Batch metrics --
        pub batch_write_count: AtomicU64,
        pub batch_write_latency_ns_total: AtomicU64,
        pub batch_size_total: AtomicU64,
    }

    impl DbMetrics {
        /// Create a new metrics handle wrapped in [`Arc`] for shared ownership.
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Record a single GET operation with its latency and hit/miss status.
        pub fn record_get(&self, latency_ns: u64, found: bool) {
            self.get_count.fetch_add(1, Ordering::Relaxed);
            self.get_latency_ns_total.fetch_add(latency_ns, Ordering::Relaxed);
            if !found {
                self.get_miss_count.fetch_add(1, Ordering::Relaxed);
            }
        }

        /// Record a single SET operation.
        pub fn record_set(&self) {
            self.set_count.fetch_add(1, Ordering::Relaxed);
        }

        /// Record a changeset apply with its latency.
        pub fn record_changeset_apply(&self, latency_ns: u64) {
            self.changeset_apply_count.fetch_add(1, Ordering::Relaxed);
            self.changeset_apply_latency_ns_total.fetch_add(latency_ns, Ordering::Relaxed);
        }

        /// Record iterator creation.
        pub fn record_iterator(&self) {
            self.iterator_count.fetch_add(1, Ordering::Relaxed);
        }

        /// Record iterator next calls.
        pub fn record_iterator_next(&self, count: u64) {
            self.iterator_next_count.fetch_add(count, Ordering::Relaxed);
        }

        /// Record a prune operation with its latency.
        pub fn record_prune(&self, latency_ns: u64) {
            self.prune_count.fetch_add(1, Ordering::Relaxed);
            self.prune_latency_ns_total.fetch_add(latency_ns, Ordering::Relaxed);
        }

        /// Record an import operation with its latency.
        pub fn record_import(&self, latency_ns: u64) {
            self.import_count.fetch_add(1, Ordering::Relaxed);
            self.import_latency_ns_total.fetch_add(latency_ns, Ordering::Relaxed);
        }

        /// Record a batch write with its latency and byte size.
        pub fn record_batch_write(&self, latency_ns: u64, size: u64) {
            self.batch_write_count.fetch_add(1, Ordering::Relaxed);
            self.batch_write_latency_ns_total.fetch_add(latency_ns, Ordering::Relaxed);
            self.batch_size_total.fetch_add(size, Ordering::Relaxed);
        }

        /// Snapshot of all counters for periodic reporting.
        pub fn snapshot(&self) -> MetricsSnapshot {
            MetricsSnapshot {
                get_count: self.get_count.load(Ordering::Relaxed),
                get_latency_ns_total: self.get_latency_ns_total.load(Ordering::Relaxed),
                get_miss_count: self.get_miss_count.load(Ordering::Relaxed),
                set_count: self.set_count.load(Ordering::Relaxed),
                changeset_apply_count: self.changeset_apply_count.load(Ordering::Relaxed),
                changeset_apply_latency_ns_total: self
                    .changeset_apply_latency_ns_total
                    .load(Ordering::Relaxed),
                iterator_count: self.iterator_count.load(Ordering::Relaxed),
                iterator_next_count: self.iterator_next_count.load(Ordering::Relaxed),
                prune_count: self.prune_count.load(Ordering::Relaxed),
                prune_latency_ns_total: self.prune_latency_ns_total.load(Ordering::Relaxed),
                import_count: self.import_count.load(Ordering::Relaxed),
                import_latency_ns_total: self.import_latency_ns_total.load(Ordering::Relaxed),
                batch_write_count: self.batch_write_count.load(Ordering::Relaxed),
                batch_write_latency_ns_total: self
                    .batch_write_latency_ns_total
                    .load(Ordering::Relaxed),
                batch_size_total: self.batch_size_total.load(Ordering::Relaxed),
            }
        }
    }

    /// Point-in-time copy of every counter in [`DbMetrics`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct MetricsSnapshot {
        pub get_count: u64,
        pub get_latency_ns_total: u64,
        pub get_miss_count: u64,
        pub set_count: u64,
        pub changeset_apply_count: u64,
        pub changeset_apply_latency_ns_total: u64,
        pub iterator_count: u64,
        pub iterator_next_count: u64,
        pub prune_count: u64,
        pub prune_latency_ns_total: u64,
        pub import_count: u64,
        pub import_latency_ns_total: u64,
        pub batch_write_count: u64,
        pub batch_write_latency_ns_total: u64,
        pub batch_size_total: u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_timer_basic() {
        let mut timer = PhaseTimer::new("block_exec");
        assert_eq!(timer.name(), "block_exec");

        timer.set_phase("decode");
        // Simulate some work
        std::thread::sleep(std::time::Duration::from_millis(5));

        timer.set_phase("execute");
        // Simulate some work
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Finish the last phase by switching to a dummy or resetting
        timer.set_phase("done");

        let durations = timer.durations();
        assert!(durations.contains_key("decode"), "missing 'decode' phase");
        assert!(durations.contains_key("execute"), "missing 'execute' phase");
        assert!(durations["decode"].as_nanos() > 0, "decode duration should be > 0");
        assert!(durations["execute"].as_nanos() > 0, "execute duration should be > 0");
    }

    #[test]
    fn test_phase_timer_reset() {
        let mut timer = PhaseTimer::new("commit");

        timer.set_phase("write");
        std::thread::sleep(std::time::Duration::from_millis(5));

        timer.set_phase("flush");
        std::thread::sleep(std::time::Duration::from_millis(5));

        timer.reset();

        assert!(timer.durations().is_empty(), "durations should be cleared after reset");
        assert!(timer.current_phase.is_none(), "current_phase should be None after reset");
    }

    // -- DbMetrics tests --

    use db_metrics::DbMetrics;

    #[test]
    fn test_db_metrics_get() {
        let m = DbMetrics::new();

        m.record_get(1_000, true);
        m.record_get(2_000, true);
        m.record_get(500, false);

        let snap = m.snapshot();
        assert_eq!(snap.get_count, 3);
        assert_eq!(snap.get_latency_ns_total, 3_500);
        assert_eq!(snap.get_miss_count, 1);
    }

    #[test]
    fn test_db_metrics_changeset_apply() {
        let m = DbMetrics::new();

        m.record_changeset_apply(10_000);
        m.record_changeset_apply(20_000);

        let snap = m.snapshot();
        assert_eq!(snap.changeset_apply_count, 2);
        assert_eq!(snap.changeset_apply_latency_ns_total, 30_000);
    }

    #[test]
    fn test_db_metrics_snapshot() {
        let m = DbMetrics::new();

        m.record_get(100, true);
        m.record_get(200, false);
        m.record_set();
        m.record_changeset_apply(300);
        m.record_iterator();
        m.record_iterator_next(5);
        m.record_prune(400);
        m.record_import(500);
        m.record_batch_write(600, 1024);

        let snap = m.snapshot();
        assert_eq!(snap.get_count, 2);
        assert_eq!(snap.get_latency_ns_total, 300);
        assert_eq!(snap.get_miss_count, 1);
        assert_eq!(snap.set_count, 1);
        assert_eq!(snap.changeset_apply_count, 1);
        assert_eq!(snap.changeset_apply_latency_ns_total, 300);
        assert_eq!(snap.iterator_count, 1);
        assert_eq!(snap.iterator_next_count, 5);
        assert_eq!(snap.prune_count, 1);
        assert_eq!(snap.prune_latency_ns_total, 400);
        assert_eq!(snap.import_count, 1);
        assert_eq!(snap.import_latency_ns_total, 500);
        assert_eq!(snap.batch_write_count, 1);
        assert_eq!(snap.batch_write_latency_ns_total, 600);
        assert_eq!(snap.batch_size_total, 1024);
    }

    #[test]
    fn test_db_metrics_thread_safe() {
        use std::sync::Arc;

        let m = DbMetrics::new();
        let num_threads = 8;
        let ops_per_thread = 1_000;

        let mut handles = Vec::new();
        for _ in 0..num_threads {
            let m = Arc::clone(&m);
            handles.push(std::thread::spawn(move || {
                for i in 0..ops_per_thread {
                    m.record_get(10, i % 3 == 0);
                    m.record_batch_write(5, 64);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let snap = m.snapshot();
        let total_ops = (num_threads * ops_per_thread) as u64;
        assert_eq!(snap.get_count, total_ops);
        assert_eq!(snap.get_latency_ns_total, total_ops * 10);
        assert_eq!(snap.batch_write_count, total_ops);
        assert_eq!(snap.batch_write_latency_ns_total, total_ops * 5);
        assert_eq!(snap.batch_size_total, total_ops * 64);
        // found=true when i%3==0, so misses are when i%3!=0
        // i%3==0 count per 1000 iterations = 334 (0,3,6,...,999), misses = 666
        let hits_per_thread = (ops_per_thread as u64 + 2) / 3; // ceil division
        let misses_per_thread = ops_per_thread as u64 - hits_per_thread;
        let expected_misses = num_threads as u64 * misses_per_thread;
        assert_eq!(snap.get_miss_count, expected_misses);
    }
}
