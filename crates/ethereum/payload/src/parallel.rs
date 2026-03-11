//! Background parallel simulation for the Ethereum payload builder.
//!
//! When parallel execution is enabled, transactions are pre-simulated in a
//! background thread (using rayon for per-tx parallelism) to extract CrwSets,
//! while the main thread executes them sequentially at full speed.
//!
//! Phase 1: background CrwSets collection (no execution speedup yet)
//! Phase 2 (future): use CrwSets with Framer for conflict-free parallel dispatch

use alloy_consensus::Transaction;
use alloy_primitives::{Address, B256, U256};
use reth_ethereum_primitives::TransactionSigned;
use reth_storage_api::StateProvider;
use revm::context::TxEnv;
use xlayer_parallel_exec::simulator::SimTxEnv;

/// A [`DatabaseRef`](revm::DatabaseRef) adapter for simulation.
///
/// Reads from the base `StateProvider` (pre-block state). For L1, there are no
/// sequencer transactions, so no account overrides are needed.
pub(crate) struct SimDatabaseRef<'a> {
    pub(crate) provider: &'a dyn StateProvider,
}

/// SAFETY: `SimDatabaseRef` is read-only during parallel simulation.
/// The `StateProvider` is backed by QMDB (lock-free reads) or MDBX (thread-safe reads).
/// The `StateProviderBox` type lacks `Sync` in its trait object bound,
/// but all concrete implementations used in reth are thread-safe for reads.
unsafe impl Sync for SimDatabaseRef<'_> {}

impl core::fmt::Debug for SimDatabaseRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SimDatabaseRef").finish_non_exhaustive()
    }
}

impl revm::DatabaseRef for SimDatabaseRef<'_> {
    type Error = reth_storage_api::errors::ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        Ok(self.provider.basic_account(&address)?.map(Into::into))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
        Ok(self.provider.bytecode_by_hash(&code_hash)?.unwrap_or_default().0)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        Ok(self.provider.storage(address, B256::new(index.to_be_bytes()))?.unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        Ok(reth_storage_api::BlockHashReader::block_hash(self.provider, number)?
            .unwrap_or_default())
    }
}

/// Convert a `Recovered<TransactionSigned>` into a [`SimTxEnv`] for pre-simulation.
pub(crate) fn tx_to_sim_env(
    tx: &alloy_consensus::transaction::Recovered<TransactionSigned>,
) -> SimTxEnv {
    let signer = tx.signer();
    let tx_env = TxEnv {
        caller: signer,
        gas_limit: tx.gas_limit(),
        gas_price: tx.gas_price().unwrap_or_default(),
        kind: tx.to().into(),
        value: tx.value(),
        data: tx.input().clone(),
        nonce: tx.nonce(),
        ..Default::default()
    };
    SimTxEnv { sender: signer, tx_env, pre_crw_sets: None }
}
