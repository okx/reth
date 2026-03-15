//! Metrics for the payload builder impl

use reth_metrics::{metrics::Counter, Metrics};
use std::sync::OnceLock;

/// Global metrics instance for CachedReads tracking
/// This allows metrics to be incremented from closures without ownership issues
static GLOBAL_CACHED_READS_METRICS: OnceLock<CachedReadsMetrics> = OnceLock::new();

/// Metrics specifically for CachedReads hit/miss tracking
/// Separate from PayloadBuilderMetrics so it can be accessed globally
#[derive(Metrics, Clone)]
#[metrics(scope = "payloads")]
pub struct CachedReadsMetrics {
    /// Total number of cache hits in CachedReads during EVM execution.
    /// This is tracked regardless of pre-warming feature being enabled.
    pub cached_reads_hits: Counter,
    /// Total number of cache misses in CachedReads during EVM execution.
    /// This is tracked regardless of pre-warming feature being enabled.
    pub cached_reads_misses: Counter,
}

impl CachedReadsMetrics {
    /// Get or create the global CachedReads metrics instance.
    /// This is used by both basic and optimism payload builders.
    pub fn global() -> &'static CachedReadsMetrics {
        GLOBAL_CACHED_READS_METRICS.get_or_init(CachedReadsMetrics::default)
    }

    /// Increment cache hits counter
    pub fn inc_hits(&self) {
        self.cached_reads_hits.increment(1);
    }

    /// Increment cache misses counter
    pub fn inc_misses(&self) {
        self.cached_reads_misses.increment(1);
    }
}

/// Payload builder metrics
#[derive(Metrics)]
#[metrics(scope = "payloads")]
pub(crate) struct PayloadBuilderMetrics {
    /// Total number of times an empty payload was returned because a built one was not ready.
    pub(crate) requested_empty_payload: Counter,
    /// Total number of initiated payload build attempts.
    pub(crate) initiated_payload_builds: Counter,
    /// Total number of failed payload build attempts.
    pub(crate) failed_payload_builds: Counter,
}

impl PayloadBuilderMetrics {
    pub(crate) fn inc_requested_empty_payload(&self) {
        self.requested_empty_payload.increment(1);
    }

    pub(crate) fn inc_initiated_payload_builds(&self) {
        self.initiated_payload_builds.increment(1);
    }

    pub(crate) fn inc_failed_payload_builds(&self) {
        self.failed_payload_builds.increment(1);
    }
}
