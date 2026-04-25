//! OKX-only timing for the block builder `finish` phase.
//!
//! Mirrors the per-block phase breakdown used in the `benchmark-v2.0.0`
//! reports for the two stages of `BlockBuilder::finish`:
//!
//! * `state_root` — `hashed_post_state` + `state_root_with_updates`
//! * `assemble`   — `assemble_block` + `RecoveredBlock::new_unhashed`
//!
//! Both `tracing::info!` logs and Prometheus histograms are emitted whenever
//! the `std` feature of this crate is enabled (which is the case in every
//! reth binary build). Under no-std, this whole module is not compiled and
//! the upstream call site is gated out.

#![cfg(feature = "std")]

use core::time::Duration;
use tracing::info;

#[cfg(feature = "metrics")]
use reth_metrics::{metrics::Histogram, Metrics};
#[cfg(feature = "metrics")]
use std::sync::LazyLock;

#[cfg(feature = "metrics")]
#[derive(Metrics)]
#[metrics(scope = "okx.block_builder")]
struct BlockBuilderFinishMetrics {
    /// Time spent computing the state root.
    state_root_seconds: Histogram,
    /// Time spent assembling the block (sealing + tx parts split).
    assemble_seconds: Histogram,
}

#[cfg(feature = "metrics")]
static METRICS: LazyLock<BlockBuilderFinishMetrics> =
    LazyLock::new(BlockBuilderFinishMetrics::default);

/// Record one `BlockBuilder::finish` invocation.
///
/// Both stages are reported as Prometheus histograms (`okx.block_builder.*`)
/// and as a single `tracing::info!` line on the `okx::block_builder::timing`
/// target. `block_number` is included for log correlation under concurrent
/// payload-build tasks.
pub fn record_finish_timing(block_number: u64, state_root: Duration, assemble: Duration) {
    #[cfg(feature = "metrics")]
    {
        METRICS.state_root_seconds.record(state_root.as_secs_f64());
        METRICS.assemble_seconds.record(assemble.as_secs_f64());
    }

    info!(
        target: "okx::block_builder::timing",
        block_number,
        state_root_ms = state_root.as_secs_f64() * 1000.0,
        assemble_ms = assemble.as_secs_f64() * 1000.0,
        "okx block builder finish timing"
    );
}
