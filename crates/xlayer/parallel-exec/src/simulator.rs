//! Simulator: pre-executes transactions to extract read/write sets.
//!
//! Runs each transaction through the EVM with nonce-check disabled,
//! collecting the accounts and storage slots accessed. This information
//! feeds into the Framer for conflict-free parallel grouping.

use crate::{
    crw_sets::{extract_crw_sets, short_hash_address, CrwSets},
    task::SimResult,
};
use alloy_evm::{precompiles::PrecompilesMap, Evm, EvmEnv};
use alloy_primitives::Address;
use revm::{
    context::{BlockEnv, CfgEnv, Context, TxEnv},
    database::CacheDB,
    handler::EthPrecompiles,
    inspector::NoOpInspector,
    MainBuilder, MainContext,
};

pub use alloy_evm::EthEvm;

/// Simulates transactions to extract their read/write sets.
///
/// Transactions are split into shards by sender address to maintain
/// per-sender ordering while enabling cross-sender parallelism.
#[derive(Debug)]
pub struct Simulator {
    /// Number of parallel shards (default: 4).
    shard_count: usize,
}

impl Default for Simulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Simulator {
    /// Create a new simulator with the default shard count (4).
    pub fn new() -> Self {
        Self { shard_count: 4 }
    }

    /// Create a new simulator with a custom shard count.
    pub fn with_shard_count(shard_count: usize) -> Self {
        Self { shard_count }
    }

    /// Returns the configured shard count.
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Simulate a batch of transactions, extracting CrwSets for each.
    ///
    /// Returns [`SimResult`]s in original transaction order.
    /// Each transaction is executed in an isolated EVM with nonce checking
    /// disabled, so execution may fail (e.g. insufficient balance) but
    /// the accessed accounts/slots are still captured.
    pub fn simulate<DB>(&self, txs: &[SimTxEnv], db: &DB, block_env: &BlockEnv) -> Vec<SimResult>
    where
        DB: revm::DatabaseRef + core::fmt::Debug,
        DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
    {
        txs.iter().enumerate().map(|(idx, tx)| self.simulate_one(tx, idx, db, block_env)).collect()
    }

    /// Simulate a single transaction.
    ///
    /// Builds a revm EVM instance with nonce checking disabled, executes
    /// the transaction, and extracts CrwSets from the resulting state diff.
    fn simulate_one<DB>(
        &self,
        tx: &SimTxEnv,
        original_index: usize,
        db: &DB,
        block_env: &BlockEnv,
    ) -> SimResult
    where
        DB: revm::DatabaseRef + core::fmt::Debug,
        DB::Error: core::fmt::Debug + core::error::Error + Send + Sync + 'static,
    {
        let mut cfg = CfgEnv::default();
        cfg.disable_nonce_check = true;

        let evm_env = EvmEnv { cfg_env: cfg, block_env: block_env.clone() };

        let cache_db = CacheDB::new(db);

        let inner = Context::mainnet()
            .with_db(cache_db)
            .with_cfg(evm_env.cfg_env)
            .with_block(evm_env.block_env)
            .build_mainnet_with_inspector(NoOpInspector {})
            .with_precompiles(PrecompilesMap::from_static(EthPrecompiles::default().precompiles));

        let mut evm = EthEvm::new(inner, false);

        match evm.transact(tx.tx_env.clone()) {
            Ok(result_and_state) => {
                let success = result_and_state.result.is_success();
                let crw_sets = extract_crw_sets(&result_and_state);
                SimResult { crw_sets, original_index, success }
            }
            Err(err) => {
                tracing::trace!(
                    target: "xlayer::parallel::simulator",
                    ?err,
                    index = original_index,
                    "Simulation failed for transaction"
                );
                // Return CrwSets with at least the sender recorded as a write
                let mut crw_sets = CrwSets::default();
                crw_sets.account_writes.push(short_hash_address(&tx.sender));
                SimResult { crw_sets, original_index, success: false }
            }
        }
    }
}

/// Simplified transaction environment for simulation.
/// Wraps revm's [`TxEnv`] with the sender address for shard routing.
#[derive(Debug, Clone)]
pub struct SimTxEnv {
    /// Sender address (used for shard routing by the Framer).
    pub sender: Address,
    /// revm transaction environment.
    pub tx_env: TxEnv,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sim_result_creation() {
        let crw = CrwSets {
            account_reads: vec![[99u8; 10]],
            account_writes: vec![[42u8; 10]],
            storage_reads: vec![],
            storage_writes: vec![],
        };

        let result = SimResult { crw_sets: crw, original_index: 7, success: true };

        assert_eq!(result.original_index, 7);
        assert!(result.success);
        assert!(result.crw_sets.account_writes.contains(&[42u8; 10]));
        assert!(result.crw_sets.account_reads.contains(&[99u8; 10]));
        assert!(result.crw_sets.storage_reads.is_empty());
    }

    fn make_transfer_tx(sender: Address, recipient: Address, nonce: u64) -> SimTxEnv {
        use alloy_primitives::TxKind;

        let tx_env = TxEnv {
            caller: sender,
            gas_limit: 21000,
            gas_price: 0,
            kind: TxKind::Call(recipient),
            value: alloy_primitives::U256::ZERO,
            nonce,
            ..Default::default()
        };
        SimTxEnv { sender, tx_env }
    }

    #[test]
    fn test_simulator_preserves_index() {
        let sim = Simulator::new();

        let sender = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let block_env = BlockEnv::default();

        let txs: Vec<SimTxEnv> = (0..5).map(|i| make_transfer_tx(sender, recipient, i)).collect();

        let db = revm::database::EmptyDB::default();
        let results = sim.simulate(&txs, &db, &block_env);

        assert_eq!(results.len(), 5);
        for (i, result) in results.iter().enumerate() {
            assert_eq!(result.original_index, i);
        }
    }

    #[test]
    fn test_simulator_simple_transfer() {
        let sim = Simulator::new();

        let sender = Address::with_last_byte(0xAA);
        let recipient = Address::with_last_byte(0xBB);
        let block_env = BlockEnv::default();

        let tx = make_transfer_tx(sender, recipient, 0);
        let db = revm::database::EmptyDB::default();
        let results = sim.simulate(&[tx], &db, &block_env);

        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.original_index, 0);

        // The sender should appear in account_writes (nonce bump fallback)
        let sender_hash = short_hash_address(&sender);
        let has_sender = result.crw_sets.account_writes.contains(&sender_hash) ||
            result.crw_sets.account_reads.contains(&sender_hash);
        assert!(has_sender, "Sender should appear in CrwSets");
    }

    #[test]
    fn test_simulator_default() {
        let sim = Simulator::default();
        assert_eq!(sim.shard_count(), 4);
    }

    #[test]
    fn test_simulator_custom_shards() {
        let sim = Simulator::with_shard_count(8);
        assert_eq!(sim.shard_count(), 8);
    }

    #[test]
    fn test_sim_tx_env_creation() {
        let sender = Address::with_last_byte(1);
        let tx_env = TxEnv::default();
        let sim_tx = SimTxEnv { sender, tx_env };
        assert_eq!(sim_tx.sender, sender);
    }
}
