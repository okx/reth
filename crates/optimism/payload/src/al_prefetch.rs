//! Access-list-driven prefetch into [`CachedReads`].
//!
//! Called between transaction selection (3.1) and execution (3.2) inside
//! `OpBuilder::build()`. At that point the best transactions are already
//! selected with the correct base-fee filter — we simply extract their
//! EIP-2930 access list keys and pre-load the declared accounts/slots from
//! MDBX into `CachedReads` before the EVM touches them.
//!
//! ## Flow
//!
//! ```text
//! build()
//!   ├── 3.1  best_txs = best_transactions_with_attrs(basefee)   ← selection
//!   ├── 3.1.5 prefetch_from_best_txs(best_txs_copy, db)         ← THIS MODULE
//!   │           Pass 1 — key extraction (no I/O):
//!   │             for each TX with EIP-2930 access list:
//!   │               unique_accounts.insert(address)
//!   │               unique_slots.insert((address, slot))
//!   │           Pass 2 — MDBX reads (deduplicated):
//!   │             db.basic(address)         → MDBX → cached in CachedReads
//!   │             db.storage(address, slot) → MDBX → cached in CachedReads
//!   └── 3.2  execute_best_transactions(best_txs)                ← execution
//!               EVM reads: near-100% cache hits
//! ```
//!
//! ## Design decisions
//!
//! - **Correct transaction set** — operates on `best_transactions_with_attrs`
//!   (base-fee filtered, priority-ordered), not all pending transactions.
//! - **Zero background workers** — all work is synchronous on the build thread.
//! - **Deduplication before reads** — hot contracts (USDC, WETH) appear in
//!   thousands of TXs; a `HashSet` pass first issues exactly one MDBX read
//!   per unique key regardless of how many TXs declare it.
//! - **Reads through `State` DB** — `builder.evm_mut().db_mut()` is
//!   `&mut State<CachedReadsDbMut<...>>`. Calling `basic()`/`storage()` on it
//!   falls through State journal → CachedReadsDbMut → MDBX, populating
//!   CachedReads automatically. No separate cache structure needed.
//! - **Best-effort** — individual MDBX errors are silently ignored; the EVM
//!   falls back to live reads for any missed entry.
//! - **Enabled via env var** — `TXPOOL_AL_PREFETCH_ONLY=1`; checked once at
//!   first call and cached in a [`OnceLock`].

use alloy_consensus::Transaction as _;
use alloy_primitives::{map::HashSet, Address, U256};
use reth_payload_util::PayloadTransactions;
use std::sync::OnceLock;
use tracing::debug;

const METRIC_TX_COUNT: &str = "reth_al_prefetch_tx_with_access_list_total";
const METRIC_KEYS: &str = "reth_al_prefetch_keys_extracted_total";
const METRIC_DURATION: &str = "reth_al_prefetch_duration_seconds";

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Returns `true` when AL prefetch is enabled (`TXPOOL_AL_PREFETCH_ONLY=1`).
#[inline]
pub fn is_enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("TXPOOL_AL_PREFETCH_ONLY")
            .map(|v| matches!(v.as_str(), "1" | "true" | "True" | "TRUE"))
            .unwrap_or(false)
    })
}

/// Statistics from one prefetch call.
#[derive(Debug, Default)]
pub struct AlPrefetchStats {
    pub tx_with_al: usize,
    pub accounts_loaded: usize,
    pub slots_loaded: usize,
    pub elapsed_us: u64,
}

/// Pre-populate the EVM's database cache from EIP-2930 access lists of the
/// already-selected best transactions.
///
/// `txs` is a **second** iterator over the same selection used for execution
/// (possible because `best` is now `Fn`, not `FnOnce`). `db` is
/// `builder.evm_mut().db_mut()` — `State<CachedReadsDbMut<...>>`.
///
/// ## Two-pass approach
///
/// **Pass 1** (no I/O): collect unique `Address` and `(Address, U256)` slot
/// pairs from all access lists into `HashSet`s.
///
/// **Pass 2** (MDBX reads): call `db.basic(addr)` / `db.storage(addr, slot)`
/// for each unique key. Each call falls through `State` → `CachedReadsDbMut`
/// → MDBX and stores the result in `CachedReads`. The EVM's subsequent reads
/// for those keys are in-memory hits.
pub fn prefetch_from_best_txs<Txs, DB>(mut txs: Txs, db: &mut DB) -> AlPrefetchStats
where
    Txs: PayloadTransactions,
    Txs::Transaction: alloy_consensus::Transaction,
    DB: revm::Database,
{
    let start = std::time::Instant::now();

    // ── Pass 1: key extraction (no I/O) ────────────────────────────────────
    let mut tx_with_al = 0usize;
    let mut unique_accounts: HashSet<Address> = HashSet::default();
    let mut unique_slots: HashSet<(Address, U256)> = HashSet::default();

    while let Some(tx) = txs.next(()) {
        let Some(al) = tx.access_list() else { continue };
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

    // ── Pass 2: MDBX reads on deduplicated keys ─────────────────────────────
    for &addr in &unique_accounts {
        let _ = db.basic(addr);
    }
    for &(addr, slot) in &unique_slots {
        let _ = db.storage(addr, slot);
    }

    let accounts_loaded = unique_accounts.len();
    let slots_loaded = unique_slots.len();
    let elapsed_us = start.elapsed().as_micros() as u64;

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
