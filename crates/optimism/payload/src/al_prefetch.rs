use alloy_consensus::Transaction as _;
use alloy_primitives::{map::HashSet, Address, U256};
use reth_payload_util::PayloadTransactions;
use std::sync::OnceLock;
use tracing::debug;

static ENABLED: OnceLock<bool> = OnceLock::new();

pub fn is_enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("TXPOOL_AL_PREFETCH_ONLY")
            .map(|v| matches!(v.as_str(), "1" | "true" | "True" | "TRUE"))
            .unwrap_or(false)
    })
}

/// Pre-loads EIP-2930 access list keys from MDBX into the EVM's cached DB
/// for the already-selected best transactions, before execution begins.
/// Reads go through `builder.evm_mut().db_mut()` so State/CachedReads is
/// populated automatically — the EVM then hits cache instead of MDBX.
pub fn prefetch_from_best_txs<Txs, DB>(mut txs: Txs, db: &mut DB)
where
    Txs: PayloadTransactions,
    Txs::Transaction: alloy_consensus::Transaction,
    DB: revm::Database,
{
    let start = std::time::Instant::now();
    let mut tx_count = 0usize;
    
    let mut accounts: HashSet<Address> = HashSet::default();
    let mut slots: HashSet<(Address, U256)> = HashSet::default();

    while let Some(tx) = txs.next(()) {
        let Some(al) = tx.access_list() else { continue };
        if al.0.is_empty() {
            continue;
        }
        tx_count += 1;
        for item in &al.0 {
            accounts.insert(item.address);
            for key in &item.storage_keys {
                slots.insert((item.address, U256::from_be_bytes(key.0)));
            }
        }
    }

    // Pass 2: one MDBX read per unique key — populates State/CachedReads.
    for &addr in &accounts {
        let _ = db.basic(addr);
    }
    for &(addr, slot) in &slots {
        let _ = db.storage(addr, slot);
    }

    let key_count = accounts.len() + slots.len();

    let elapsed_us = start.elapsed().as_micros() as u64;

    metrics::counter!("reth_al_prefetch_tx_with_access_list_total").increment(tx_count as u64);
    metrics::counter!("reth_al_prefetch_keys_extracted_total").increment(key_count as u64);
    metrics::histogram!("reth_al_prefetch_duration_seconds")
        .record(elapsed_us as f64 / 1_000_000.0);

    debug!(
        target: "payload_builder::al_prefetch",
        tx_count,
        key_count,
        elapsed_us,
        "AL prefetch completed"
    );
}
