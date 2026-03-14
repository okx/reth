//! Access-list-driven prefetch into [`CachedReads`].
//!
//! At block build time, iterates all pending pool transactions, extracts every
//! EIP-2930 access list entry, deduplicates the keys, and pre-loads the
//! referenced accounts and storage slots into [`CachedReads`] via sequential
//! MDBX reads.
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
//!   │     Pass 1 — key extraction (no MDBX):
//!   │       for each pending TX with EIP-2930 access list:
//!   │         unique_accounts.insert(address)
//!   │         unique_slots.insert((address, slot))
//!   │     Pass 2 — MDBX reads (deduplicated):
//!   │       for each unique address:
//!   │         preload.basic(address)           → MDBX read, cached
//!   │       for each unique (address, slot):
//!   │         preload.storage(address, slot)   → MDBX read, cached
//!   └── builder.build(cached_reads.as_db_mut(state), ...)  (existing)
//!         EVM reads: near-100% cache hits
//! ```
//!
//! ## Design decisions
//!
//! - **Zero background workers** — all work is on the block-build thread.
//! - **Deduplication before reads** — hot contracts (USDC, WETH) appear in
//!   thousands of TXs; without dedup we'd call `basic(usdc)` thousands of times
//!   even though only the first is an MDBX round-trip. A `HashSet` pass first
//!   eliminates all redundant calls.
//! - **Reuses `CachedReadsDbMut`** — standard reth cache layer; no new data structures.
//! - **Best-effort** — errors from individual MDBX reads are silently ignored; the
//!   EVM falls back to live MDBX reads for any missed entry.
//! - **Enabled via env var** — `TXPOOL_AL_PREFETCH_ONLY=1` or `true`; checked once
//!   at first call and cached in a [`OnceLock`].

use alloy_consensus::Transaction as _;
use alloy_primitives::{
    map::{HashSet},
    Address, U256,
};
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
    /// Unique accounts loaded (one MDBX read per unique address).
    pub accounts_loaded: usize,
    /// Unique storage slots loaded (one MDBX read per unique (address, slot) pair).
    pub slots_loaded: usize,
    /// Wall-clock time for the entire prefetch call (key extraction + MDBX reads).
    pub elapsed_us: u64,
}

/// Pre-populate `cached_reads` from the EIP-2930 access lists of all pending
/// pool transactions.
///
/// ## Two-pass approach
///
/// **Pass 1** (no I/O): iterate all pending TXs and collect unique addresses
/// and `(address, slot)` pairs into `HashSet`s. Hot contracts like USDC/WETH
/// appear in thousands of transactions; deduplicating here means we issue
/// exactly one MDBX read per unique key regardless of mempool size.
///
/// **Pass 2** (MDBX reads): iterate the deduplicated sets and call
/// `preload.basic(address)` / `preload.storage(address, slot)` once each.
/// The [`CachedReadsDbMut`] wrapper stores each result in `cached_reads`
/// automatically. The EVM later gets in-memory hits instead of MDBX round-trips.
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

    // ── Pass 1: key extraction (no MDBX, no allocations beyond the sets) ──

    let mut tx_with_al = 0usize;
    let mut unique_accounts: HashSet<Address> = HashSet::default();
    let mut unique_slots: HashSet<(Address, U256)> = HashSet::default();

    for valid_tx in pool.pending_transactions() {
        let Some(al) = valid_tx.transaction.access_list() else { continue };
        if al.0.is_empty() {
            continue;
        }
        tx_with_al += 1;
        for item in &al.0 {
            unique_accounts.insert(item.address);
            for key in &item.storage_keys {
                unique_slots.insert((item.address, U256::from_be_bytes(key.0)));
            }
        }
    }

    if tx_with_al == 0 {
        return AlPrefetchStats::default();
    }

    // ── Pass 2: MDBX reads on deduplicated keys ──
    //
    // Wrap state_provider in a CachedReadsDbMut so every read is automatically
    // stored in cached_reads. Drop `preload` before returning so the borrow ends.

    let db = StateProviderDatabase::new(state_provider);
    let mut preload = cached_reads.as_db_mut(db);

    for &addr in &unique_accounts {
        let _ = preload.basic(addr);
    }

    for &(addr, slot) in &unique_slots {
        let _ = preload.storage(addr, slot);
    }

    drop(preload);

    let accounts_loaded = unique_accounts.len();
    let slots_loaded = unique_slots.len();
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
