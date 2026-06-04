use crate::{eth::RpcNodeCore, OpEthApi, OpEthApiError};
use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::B256;
use op_revm::transaction::OpTxTr;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor, HaltReasonFor, TxEnvFor};
use reth_optimism_evm::OpTxEnv;
use reth_primitives_traits::Recovered;
use reth_revm::db::bal::EvmDatabaseError;
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, Call, EthCall},
    FromEvmError, RpcConvert,
};
use reth_storage_api::{errors::ProviderError, ProviderTx};
use revm::{context_interface::result::ResultAndState, Database, DatabaseCommit};

impl<N, Rpc> EthCall for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError, Evm = N::Evm>,
    TxEnvFor<N::Evm>: OpTxTr + OpTxEnv,
{
}

impl<N, Rpc> EstimateCall for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError, Evm = N::Evm>,
    TxEnvFor<N::Evm>: OpTxTr + OpTxEnv,
{
}

impl<N, Rpc> Call for OpEthApi<N, Rpc>
where
    N: RpcNodeCore,
    OpEthApiError: FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = OpEthApiError, Evm = N::Evm>,
    TxEnvFor<N::Evm>: OpTxTr + OpTxEnv,
{
    #[inline]
    fn call_gas_limit(&self) -> u64 {
        self.inner.eth_api.gas_cap()
    }

    #[inline]
    fn max_simulate_blocks(&self) -> u64 {
        self.inner.eth_api.max_simulate_blocks()
    }

    #[inline]
    fn evm_memory_limit(&self) -> u64 {
        self.inner.eth_api.evm_memory_limit()
    }

    /// Gasless-aware override of the default [`Call::transact`].
    ///
    /// Builds a fresh EVM per call, so (like `inspect`) it detects gasless on this same plain EVM
    /// and, when gasless, relaxes `disable_base_fee` on the local `evm_env` clone before building
    /// the EVM and marks the `tx_env` gasless. There is no inspector here, so detection cannot
    /// pollute a trace. The non-gasless path is byte-for-byte the default.
    fn transact<DB>(
        &self,
        mut db: DB,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut tx_env: TxEnvFor<Self::Evm>,
    ) -> Result<ResultAndState<HaltReasonFor<Self::Evm>>, Self::Error>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>> + core::fmt::Debug,
    {
        // `&mut DB: Database`, so detection borrows the db and leaves it for the real execution.
        if self.detect_gasless(&mut db, evm_env.clone(), &tx_env)? {
            evm_env.cfg_env.disable_base_fee = true;
            tx_env.set_gasless(true);
        }

        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        evm.transact(tx_env).map_err(Self::Error::from_evm_err)
    }

    /// Gasless-aware override of the default [`Call::replay_transactions_until`].
    ///
    /// Replays the block's txs preceding the target to rebuild state. A zero-priced gasless tx in
    /// the block would be rejected here by the base-fee check — it only executed during block
    /// production because the gasless hook relaxed that check — so this disables the check for the
    /// whole replay. Doing so is a no-op for normal txs (the check only rejects underpriced txs,
    /// and every tx in the block already passed block execution), and a gasless tx moves zero fees
    /// regardless of the gasless flag (`effective_gas_price == 0`), so the rebuilt state is
    /// identical to block execution. Otherwise byte-for-byte the default.
    fn replay_transactions_until<'a, DB, I>(
        &self,
        db: &mut DB,
        mut evm_env: EvmEnvFor<Self::Evm>,
        transactions: I,
        target_tx_hash: B256,
    ) -> Result<usize, Self::Error>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>> + DatabaseCommit + core::fmt::Debug,
        I: IntoIterator<Item = Recovered<&'a ProviderTx<Self::Provider>>>,
    {
        evm_env.cfg_env.disable_base_fee = true;

        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        let mut index = 0;
        for tx in transactions {
            if *tx.tx_hash() == target_tx_hash {
                // reached the target transaction
                break
            }

            let tx_env = self.evm_config().tx_env(tx);
            evm.transact_commit(tx_env).map_err(Self::Error::from_evm_err)?;
            index += 1;
        }
        Ok(index)
    }
}
