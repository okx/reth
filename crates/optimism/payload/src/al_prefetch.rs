//! Access-list-driven prefetch into [`CachedReads`].
//!
//! At block build time, iterates all pending pool transactions, extracts every
//! EIP-2930 access list entry, and pre-loads the referenced accounts and storage
//! slots into [`CachedReads`] via sequential MDBX reads.
//!
//! When enabled (`TXPOOL_AL_PREFETCH_ONLY=true`), this fires inside
//! `build_payload()` before the EVM is handed the state database, so every
//! account / slot declared in an access list is already in memory when the EVM
//! executes.
//!
//! ## Flow
//!
//! ```text
//! build_payload()
//!   ├── state_provider = StateProviderDatabase::new(...)   (existing)
//!   ├── prefetch_from_pool(pool, cached_reads, state)      ← THIS MODULE
//!   │     for each pending TX with EIP-2930 access list:
//!   │       preload.basic(address)           → MDBX read, cached
//!   │       preload.storage(address, slot)   → MDBX read, cached
//!   └── builder.build(cached_reads.as_db_mut(state), ...)  (existing)
//!         EVM reads: near-100% cache hits
//! ```
//!
//! ## Design decisions
//!
//! - **Zero background workers** — all work is sequential on the block-build thread.
//! - **Reuses `CachedReadsDbMut`** — standard reth cache layer; no new data structures.
//! - **Best-effort** — errors from individual MDBX reads are silently ignored; the
//!   EVM falls back to live MDBX reads for any missed entry.
//! - **Enabled via env var** — `TXPOOL_AL_PREFETCH_ONLY=1` or `true`; checked once
//!   at first call and cached in a [`OnceLock`].

use alloy_consensus::Transaction as _;
use alloy_primitives::U256;
use reth_revm::{cached::CachedReads, database::StateProviderDatabase};
use reth_storage_api::StateProvider;
use reth_transaction_pool::TransactionPool;
use revm::Database;
use std::sync::OnceLock;
use tracing::debug;

// Prometheus metric names — match the naming convention of other reth metrics.
const METRIC_TX_COUNT: &str = "reth_al_prefetch_tx_with_access_list_total";
const METRIC_KEYS: &str = "reth_al_prefetch_keys_extracted_total";
const METRIC_DURATION: &str = "reth_al_prefetch_duration_seconds";

/// Cached result of the `TXPOOL_AL_PREFETCH_ONLY` env-var check.
/// Evaluated exactly once on first call, never changes.
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Returns `true` when AL prefetch is enabled.
///
/// Reads `TXPOOL_AL_PREFETCH_ONLY` from the environment on first call.
/// Accepts `"1"`, `"true"`, `"True"`, or `"TRUE"`.
#[inline]
pub fn is_enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("TXPOOL_AL_PREFETCH_ONLY")
            .map(|v| matches!(v.as_str(), "1" | "true" | "True" | "TRUE"))
            .unwrap_or(false)
    })
}

/// Statistics from one prefetch call — logged at `debug` level and emitted as
/// Prometheus counters / histogram.
#[derive(Debug, Default)]
pub struct AlPrefetchStats {
    /// Number of pending TXs that had a non-empty EIP-2930 access list.
    pub tx_with_al: usize,
    /// Total account entries loaded (one per unique address in all access lists).
    pub accounts_loaded: usize,
    /// Total storage slots loaded.
    pub slots_loaded: usize,
    /// Wall-clock time for the entire prefetch call (key extraction + MDBX reads).
    pub elapsed_us: u64,
}

/// Pre-populate `cached_reads` from the EIP-2930 access lists of all pending
/// pool transactions.
///
/// For every pending transaction that carries a non-empty access list:
/// - `basic(address)` is called for each declared address → loads account info
/// - `storage(address, slot)` is called for each declared slot → loads value
///
/// The [`CachedReadsDbMut`] wrapper caches each MDBX result in `cached_reads`
/// automatically. When the EVM later calls the same addresses/slots it gets
/// in-memory hits instead of MDBX round-trips.
///
/// # Errors
///
/// Individual MDBX errors are silently ignored — prefetch is best-effort.
/// The EVM will fall back to live reads for any missed entry.
pub fn prefetch_from_pool<Pool, SP>(
    pool: &Pool,
    cached_reads: &mut CachedReads,
    state_provider: &SP,
) -> AlPrefetchStats
where
    Pool: TransactionPool,
    SP: StateProvider,
{
    let start = std::time::Instant::now();

    // Wrap state_provider in a CachedReadsDbMut.
    // Every MDBX read inside this block is automatically stored in cached_reads.
    // We drop `preload` before returning so that cached_reads is usable again
    // by the main block build path.
    let db = StateProviderDatabase::new(state_provider);
    let mut preload = cached_reads.as_db_mut(db);

    let mut tx_with_al = 0usize;
    let mut accounts_loaded = 0usize;
    let mut slots_loaded = 0usize;

    for valid_tx in pool.pending_transactions() {
        // Deref Arc → ValidPoolTransaction → .transaction (Pool::Transaction: PoolTransaction)
        let Some(al) = valid_tx.transaction.access_list() else { continue };
        if al.0.is_empty() {
            continue;
        }

        tx_with_al += 1;

        for item in &al.0 {
            // Load account (nonce, balance, code hash) — miss triggers MDBX read
            // and populates cached_reads.accounts entry.
            let _ = preload.basic(item.address);
            accounts_loaded += 1;

            // Load each declared storage slot.
            // CachedReadsDbMut::storage() finds the already-loaded account and
            // issues a targeted MDBX read only for the slot value.
            for key in &item.storage_keys {
                let _ = preload.storage(item.address, U256::from_be_bytes(key.0));
                slots_loaded += 1;
            }
        }
    }

    // Drop the CachedReadsDbMut borrow — cached_reads is free to use again.
    drop(preload);

    let elapsed_us = start.elapsed().as_micros() as u64;

    // Emit Prometheus metrics.
    metrics::counter!(METRIC_TX_COUNT).increment(tx_with_al as u64);
    metrics::counter!(METRIC_KEYS).increment((accounts_loaded + slots_loaded) as u64);
    metrics::histogram!(METRIC_DURATION).record(elapsed_us as f64 / 1_000_000.0);

    debug!(
        target: "payload_builder::al_prefetch",
        tx_with_al,
        accounts_loaded,
        slots_loaded,
        elapsed_us,
        "AL prefetch completed"
    );

    AlPrefetchStats { tx_with_al, accounts_loaded, slots_loaded, elapsed_us }
}
