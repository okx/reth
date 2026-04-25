//! OKX-only timing for the Ethereum payload builder.
//!
//! Mirrors the per-payload phase breakdown used in the `benchmark-v2.0.0`
//! reports: `txpool_next` (time spent pulling the next transaction from the
//! pool), `tx_execute` (time spent inside `execute_transaction`) and the
//! overall `payload_build_total`. Both `tracing::info!` logs and Prometheus
//! histogram metrics are emitted once per build attempt.
//!
//! All timing concerns live in this module; the upstream payload builder only
//! creates a [`PayloadBuildTimer`] after `apply_pre_execution_changes`,
//! updates its fields inline and calls [`PayloadBuildTimer::finish`] on each
//! known exit path. If a timer is dropped without `finish` being called
//! (unannotated early-return, panic unwind where possible), the [`Drop`]
//! impl emits a fallback record with `outcome = dropped` so the histograms
//! still see the attempt.
//!
//! Timing boundary: `total_seconds` starts right before the
//! transaction-selection loop (after `apply_pre_execution_changes`) and ends
//! when `finish` is called. It is narrower than the miner-level
//! `okx.miner.payload_build_seconds` (which wraps the entire `resolve_kind`
//! call, including EVM/state-provider setup and any queuing).

use alloy_rpc_types_engine::PayloadId;
use reth_metrics::{metrics::Histogram, Metrics};
use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};
use tracing::info;

/// Prometheus metrics for the Ethereum payload builder.
#[derive(Metrics)]
#[metrics(scope = "okx.payload_builder")]
struct PayloadBuildMetrics {
    /// Time spent fetching the next transaction from the pool iterator.
    txpool_next_seconds: Histogram,
    /// Time spent inside `execute_transaction` (including failures).
    tx_execute_seconds: Histogram,
    /// Total wall-clock time spent in `default_ethereum_payload`.
    total_seconds: Histogram,
    /// Number of transactions iterated from the pool.
    txs_considered: Histogram,
    /// Number of transactions that were successfully executed and included.
    txs_executed: Histogram,
}

static METRICS: LazyLock<PayloadBuildMetrics> = LazyLock::new(PayloadBuildMetrics::default);

/// Outcome of a single payload-build attempt — used as a label in the log
/// line so a downstream parser can distinguish completed builds from aborted
/// / cancelled / errored ones.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BuildOutcomeLabel {
    Better,
    Aborted,
    Cancelled,
    Error,
    /// Fallback emitted by the Drop guard when no explicit outcome was set —
    /// typically an unusual early return via `?` that the call-site did not
    /// annotate.
    Dropped,
}

impl BuildOutcomeLabel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::Aborted => "aborted",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
            Self::Dropped => "dropped",
        }
    }
}

/// Tracker for one payload-build invocation. Fields are updated inline by the
/// upstream loop; on completion call [`Self::finish`] to emit the log line and
/// Prometheus metrics. If dropped without `finish`, a fallback record is
/// emitted with `outcome = dropped`.
#[derive(Debug)]
pub(crate) struct PayloadBuildTimer {
    payload_id: PayloadId,
    start: Instant,
    /// Aggregate time spent on `best_txs.next()` calls.
    pub(crate) txpool_next_total: Duration,
    /// Aggregate time spent on `execute_transaction` calls.
    pub(crate) tx_execute_total: Duration,
    /// Number of transactions iterated from the pool.
    pub(crate) txs_considered: u64,
    /// Number of transactions that were successfully executed and included.
    pub(crate) txs_executed: u64,
    /// Set to `true` once `finish` has emitted metrics, so `Drop` does not
    /// double-emit.
    emitted: bool,
}

impl PayloadBuildTimer {
    pub(crate) fn start(payload_id: PayloadId) -> Self {
        Self {
            payload_id,
            start: Instant::now(),
            txpool_next_total: Duration::ZERO,
            tx_execute_total: Duration::ZERO,
            txs_considered: 0,
            txs_executed: 0,
            emitted: false,
        }
    }

    pub(crate) fn finish(mut self, outcome: BuildOutcomeLabel) {
        self.emit(outcome);
    }

    fn emit(&mut self, outcome: BuildOutcomeLabel) {
        if self.emitted {
            return;
        }
        self.emitted = true;

        let total = self.start.elapsed();

        METRICS.txpool_next_seconds.record(self.txpool_next_total.as_secs_f64());
        METRICS.tx_execute_seconds.record(self.tx_execute_total.as_secs_f64());
        METRICS.total_seconds.record(total.as_secs_f64());
        METRICS.txs_considered.record(self.txs_considered as f64);
        METRICS.txs_executed.record(self.txs_executed as f64);

        info!(
            target: "okx::payload_builder::timing",
            id = %self.payload_id,
            outcome = outcome.as_str(),
            txs_considered = self.txs_considered,
            txs_executed = self.txs_executed,
            txpool_next_ms = ms(self.txpool_next_total),
            tx_execute_ms = ms(self.tx_execute_total),
            payload_build_total_ms = ms(total),
            "okx payload build stage timing"
        );
    }
}

impl Drop for PayloadBuildTimer {
    fn drop(&mut self) {
        // Avoid re-entering the metrics / tracing subscribers while the thread
        // is already unwinding from a panic — a panic inside `emit` during
        // unwind would abort the process.
        if !self.emitted && !std::thread::panicking() {
            self.emit(BuildOutcomeLabel::Dropped);
        }
    }
}

#[inline]
fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
