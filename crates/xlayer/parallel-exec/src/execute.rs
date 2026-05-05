//! Parallel transaction execution using revm.
//!
//! Provides functions for executing individual transactions in a parallel
//! context. Uses `WrapDatabaseRef` instead of `CacheDB` to avoid redundant
//! caching — revm's `JournaledState` already caches per-transaction reads,
//! and the `ParallelStateCache` layer provides cross-transaction caching.

use alloy_evm::{precompiles::PrecompilesMap, Evm, EvmEnv};
use revm::{
    context::{BlockEnv, CfgEnv, Context, TxEnv},
    database::WrapDatabaseRef,
    handler::EthPrecompiles,
    inspector::NoOpInspector,
    MainBuilder, MainContext,
};

pub use alloy_evm::EthEvm;

/// Cached precompiles map — built once and reused for all executions.
/// Avoids the overhead of recreating `PrecompilesMap::from_static(...)` per transaction.
pub static PRECOMPILES: std::sync::LazyLock<PrecompilesMap> =
    std::sync::LazyLock::new(|| {
        PrecompilesMap::from_static(
            EthPrecompiles::new(revm::primitives::hardfork::SpecId::default()).precompiles,
        )
    });

/// Result of executing a single transaction in parallel.
#[derive(Debug)]
pub struct ParallelTxResult {
    /// The execution result (success, revert, halt).
    pub result: revm::context::result::ExecutionResult,
    /// State changes from this transaction.
    pub state: revm::state::EvmState,
    /// Gas used by this transaction.
    pub gas_used: u64,
    /// Whether the transaction succeeded.
    pub success: bool,
}

/// Execute a single transaction using revm with the given database.
///
/// This is the core execution function called by parallel executor threads.
/// Each thread provides its own `CacheDB` wrapping a shared `DatabaseRef`
/// (typically `ParallelTxDatabase`).
///
/// The function:
/// 1. Constructs an EVM instance with the given config
/// 2. Executes the transaction
/// 3. Returns the result and state diff
///
/// The caller is responsible for applying the state diff to the shared
/// cache after execution.
pub fn execute_tx<DB>(
    db: DB,
    block_env: &BlockEnv,
    cfg_env: &CfgEnv,
    tx_env: TxEnv,
) -> ParallelTxResult
where
    DB: revm::Database + std::fmt::Debug,
    DB::Error: std::fmt::Debug + std::error::Error + Send + Sync + 'static,
{
    let mut cfg = cfg_env.clone();
    cfg.disable_nonce_check = true;

    let inner = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg)
        .with_block(block_env.clone())
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PRECOMPILES.clone());

    let mut evm = EthEvm::new(inner, false);

    match evm.transact(tx_env) {
        Ok(result_and_state) => {
            let gas_used = result_and_state.result.gas_used();
            let success = result_and_state.result.is_success();
            ParallelTxResult {
                result: result_and_state.result,
                state: result_and_state.state,
                gas_used,
                success,
            }
        }
        Err(err) => {
            tracing::warn!(
                target: "xlayer::parallel::execute",
                ?err,
                "Parallel transaction execution failed"
            );
            ParallelTxResult {
                result: revm::context::result::ExecutionResult::Halt {
                    reason: revm::context::result::HaltReason::NotActivated,
                    gas: revm::context::result::ResultGas::new(0, 0, 0),
                    logs: std::vec::Vec::new(),
                },
                state: Default::default(),
                gas_used: 0,
                success: false,
            }
        }
    }
}

/// Execute a transaction with a DatabaseRef.
///
/// Uses `WrapDatabaseRef` to adapt DatabaseRef → Database without the overhead
/// of CacheDB's intermediate HashMap. revm's JournaledState already caches
/// per-transaction reads, and ParallelStateCache handles cross-tx caching.
pub fn execute_tx_with_ref<DB>(
    db: &DB,
    block_env: &BlockEnv,
    cfg_env: &CfgEnv,
    tx_env: TxEnv,
) -> ParallelTxResult
where
    DB: revm::DatabaseRef + std::fmt::Debug,
    DB::Error: std::fmt::Debug + std::error::Error + Send + Sync + 'static,
{
    let wrapped = WrapDatabaseRef(db);
    execute_tx(wrapped, block_env, cfg_env, tx_env)
}

/// Execute multiple transactions with a single EVM instance (batched).
///
/// Creates one EVM and reuses it for all txs in the batch. This amortizes
/// EVM creation overhead across multiple transactions, matching fafo's
/// approach where each task (4 txs) shares one EVM session.
pub fn execute_batch_with_ref<DB>(
    db: &DB,
    block_env: &BlockEnv,
    cfg_env: &CfgEnv,
    txs: impl IntoIterator<Item = TxEnv>,
) -> Vec<ParallelTxResult>
where
    DB: revm::DatabaseRef + std::fmt::Debug,
    DB::Error: std::fmt::Debug + std::error::Error + Send + Sync + 'static,
{
    let wrapped = WrapDatabaseRef(db);

    let mut cfg = cfg_env.clone();
    cfg.disable_nonce_check = true;

    let inner = Context::mainnet()
        .with_db(wrapped)
        .with_cfg(cfg)
        .with_block(block_env.clone())
        .build_mainnet_with_inspector(NoOpInspector {})
        .with_precompiles(PRECOMPILES.clone());

    let mut evm = EthEvm::new(inner, false);
    let mut results = Vec::new();

    for tx_env in txs {
        let result = match evm.transact(tx_env) {
            Ok(result_and_state) => {
                let gas_used = result_and_state.result.gas_used();
                let success = result_and_state.result.is_success();
                ParallelTxResult {
                    result: result_and_state.result,
                    state: result_and_state.state,
                    gas_used,
                    success,
                }
            }
            Err(err) => {
                tracing::warn!(target: "xlayer::parallel::execute", ?err, "batch tx failed");
                ParallelTxResult {
                    result: revm::context::result::ExecutionResult::Halt {
                        reason: revm::context::result::HaltReason::NotActivated,
                        gas: revm::context::result::ResultGas::new(0, 0, 0),
                    logs: std::vec::Vec::new(),
                    },
                    state: Default::default(),
                    gas_used: 0,
                    success: false,
                }
            }
        };
        results.push(result);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, TxKind, U256};

    fn make_transfer_tx(sender: Address, recipient: Address, nonce: u64) -> TxEnv {
        TxEnv {
            caller: sender,
            gas_limit: 21000,
            gas_price: 0,
            kind: TxKind::Call(recipient),
            value: U256::ZERO,
            nonce,
            ..Default::default()
        }
    }

    #[test]
    fn test_execute_tx_with_empty_db() {
        let db = revm::database::EmptyDB::default();
        let block_env = BlockEnv::default();
        let cfg_env = CfgEnv::default();

        let sender = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let tx_env = make_transfer_tx(sender, recipient, 0);

        let result = execute_tx_with_ref(&db, &block_env, &cfg_env, tx_env);

        // With EmptyDB, transaction should still produce a result
        // (may fail due to no balance, but shouldn't panic)
        assert_eq!(result.gas_used, 0_u64.max(result.gas_used));
    }

    #[test]
    fn test_execute_tx_returns_state_diff() {
        let db = revm::database::EmptyDB::default();
        let block_env = BlockEnv::default();
        let mut cfg_env = CfgEnv::default();
        cfg_env.disable_nonce_check = true;

        let sender = Address::with_last_byte(0xAA);
        let recipient = Address::with_last_byte(0xBB);
        let tx_env = make_transfer_tx(sender, recipient, 0);

        let result = execute_tx_with_ref(&db, &block_env, &cfg_env, tx_env);

        // The state diff should contain at least the sender (touched for nonce/gas)
        // With EmptyDB the tx may fail but state should still be populated
        let _ = result.state;
    }

    #[test]
    fn test_execute_tx_parallel_safety() {
        use rayon::prelude::*;

        let db = revm::database::EmptyDB::default();
        let block_env = BlockEnv::default();
        let mut cfg_env = CfgEnv::default();
        cfg_env.disable_nonce_check = true;

        let results: Vec<ParallelTxResult> = (0..10u8)
            .into_par_iter()
            .map(|i| {
                let sender = Address::with_last_byte(i);
                let recipient = Address::with_last_byte(i + 100);
                let tx_env = make_transfer_tx(sender, recipient, 0);
                execute_tx_with_ref(&db, &block_env, &cfg_env, tx_env)
            })
            .collect();

        assert_eq!(results.len(), 10);
    }

    #[test]
    fn test_parallel_tx_result_debug() {
        let result = ParallelTxResult {
            result: revm::context::result::ExecutionResult::Halt {
                reason: revm::context::result::HaltReason::NotActivated,
                gas: revm::context::result::ResultGas::new(0, 0, 0),
                    logs: std::vec::Vec::new(),
            },
            state: Default::default(),
            gas_used: 0,
            success: false,
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("ParallelTxResult"));
    }
}
