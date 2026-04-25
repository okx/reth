//! OKX-only block timing for the local miner cycle.
//!
//! Mirrors the per-cycle phase breakdown used in the `benchmark-v2.0.0` reports:
//! `idle`, `payload_build`, `new_payload`, `fcu + commit` and the `total` block
//! interval. Both `tracing::info!` logs and Prometheus histogram metrics are
//! emitted for every cycle so the Markdown / dashboard tooling can be reused.
//!
//! This module is intentionally kept self-contained so upstream `miner.rs`
//! changes are minimal — callers create a [`MinerCycleTimer`], record the
//! per-stage durations and then call [`MinerCycleTimer::finish`].

use alloy_primitives::B256;
use reth_metrics::{metrics::Histogram, Metrics};
use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};
use tracing::info;

/// Prometheus metrics for the local miner block cycle.
#[derive(Metrics)]
#[metrics(scope = "okx.miner")]
struct MinerCycleMetrics {
    /// Time spent idle waiting for the next mining trigger.
    idle_seconds: Histogram,
    /// Time spent in the payload builder (resolve_kind) for one cycle.
    payload_build_seconds: Histogram,
    /// Time spent in `new_payload` validating the produced block.
    new_payload_seconds: Histogram,
    /// `fork_choice_updated` plus post-`new_payload` bookkeeping (overhead).
    fcu_commit_seconds: Histogram,
    /// Total block interval = idle + advance.
    block_interval_seconds: Histogram,
}

static METRICS: LazyLock<MinerCycleMetrics> = LazyLock::new(MinerCycleMetrics::default);

/// Tracker collecting per-phase durations for one local-miner cycle.
#[derive(Debug)]
pub(crate) struct MinerCycleTimer {
    /// Instant the previous cycle finished — used to derive idle time.
    last_block_end: Instant,
    /// Instant `advance()` was entered.
    advance_start: Instant,
    /// `fork_choice_updated` duration.
    fcu: Duration,
    /// Payload build (resolve_kind) duration.
    payload_build: Duration,
    /// `new_payload` duration.
    new_payload: Duration,
}

impl MinerCycleTimer {
    /// Create a tracker; `last_block_end` is the instant the previous cycle
    /// finished.
    pub(crate) fn start(last_block_end: Instant) -> Self {
        Self {
            last_block_end,
            advance_start: Instant::now(),
            fcu: Duration::ZERO,
            payload_build: Duration::ZERO,
            new_payload: Duration::ZERO,
        }
    }

    pub(crate) fn record_fcu(&mut self, d: Duration) {
        self.fcu = d;
    }

    pub(crate) fn record_payload_build(&mut self, d: Duration) {
        self.payload_build = d;
    }

    pub(crate) fn record_new_payload(&mut self, d: Duration) {
        self.new_payload = d;
    }

    /// Finish the cycle, emit log + Prometheus metrics, and return the new
    /// `last_block_end` instant for the next cycle.
    pub(crate) fn finish(self, block_number: u64, block_hash: B256) -> Instant {
        let now = Instant::now();
        let idle = self.advance_start.saturating_duration_since(self.last_block_end);
        let advance = now.saturating_duration_since(self.advance_start);
        let block_interval = idle + advance;
        // "fcu + commit" = advance - payload_build - new_payload (i.e. fcu plus
        // all bookkeeping outside the two main phases).
        let fcu_commit =
            advance.saturating_sub(self.payload_build).saturating_sub(self.new_payload);

        METRICS.idle_seconds.record(idle.as_secs_f64());
        METRICS.payload_build_seconds.record(self.payload_build.as_secs_f64());
        METRICS.new_payload_seconds.record(self.new_payload.as_secs_f64());
        METRICS.fcu_commit_seconds.record(fcu_commit.as_secs_f64());
        METRICS.block_interval_seconds.record(block_interval.as_secs_f64());

        info!(
            target: "okx::miner::timing",
            block_number,
            block_hash = ?block_hash,
            block_interval_ms = ms(block_interval),
            idle_ms = ms(idle),
            payload_build_ms = ms(self.payload_build),
            new_payload_ms = ms(self.new_payload),
            fcu_commit_ms = ms(fcu_commit),
            fcu_ms = ms(self.fcu),
            "okx miner cycle timing"
        );

        now
    }
}

#[inline]
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
