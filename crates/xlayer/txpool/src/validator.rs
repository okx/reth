//! XLayer transaction validator.

use alloy_consensus::BlockHeader;
use alloy_primitives::U256;
use parking_lot::RwLock;
use reth_chainspec::ChainSpecProvider;
use reth_optimism_txpool::OpTransactionValidator;
use reth_primitives_traits::{Block, SealedBlock};
use reth_rpc_eth_api::helpers::pricer::L2GasPricer;
use reth_storage_api::BlockReaderIdExt;
use reth_transaction_pool::{
    error::InvalidPoolTransactionError, PoolTransaction, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator,
};
use std::fmt::Debug;
use std::sync::Arc;
use tracing::{debug, trace};

/// Helper type for storing the pricer in a thread-safe, optional way.
type PricerStorage = Arc<RwLock<Option<Arc<dyn L2GasPricer>>>>;

/// Validator for XLayer transactions.
///
/// This validator wraps [`OpTransactionValidator`] and adds XLayer-specific functionality,
/// including the ability to hold and manage an L2 gas price pricer.
#[derive(Clone)]
pub struct XLayerTransactionValidator<Client, Tx> {
    /// The inner Optimism transaction validator.
    inner: Arc<OpTransactionValidator<Client, Tx>>,
    /// Optional L2 gas price pricer for XLayer-specific gas price calculations.
    pricer: PricerStorage,
}

impl<Client, Tx> Debug for XLayerTransactionValidator<Client, Tx>
where
    Client: Debug,
    Tx: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XLayerTransactionValidator")
            .field("inner", &self.inner)
            .field("pricer", &"<L2GasPricer>")
            .finish()
    }
}

impl<Client, Tx> XLayerTransactionValidator<Client, Tx> {
    /// Create a new [`XLayerTransactionValidator`] wrapping the given [`OpTransactionValidator`].
    pub fn new(inner: OpTransactionValidator<Client, Tx>) -> Self {
        Self {
            inner: Arc::new(inner),
            pricer: Arc::new(RwLock::new(None)),
        }
    }

    /// Sets the L2 gas price pricer.
    ///
    /// This allows setting the pricer at runtime, without requiring it
    /// to be set during initialization.
    pub fn set_pricer(&self, pricer: Arc<dyn L2GasPricer>) {
        *self.pricer.write() = Some(pricer);
    }

    /// Gets the current L2 gas price pricer, if set.
    pub fn get_pricer(&self) -> Option<Arc<dyn L2GasPricer>> {
        self.pricer.read().clone()
    }

    /// Returns a reference to the inner [`OpTransactionValidator`].
    pub fn inner(&self) -> &OpTransactionValidator<Client, Tx> {
        &self.inner
    }

    /// Returns a mutable reference to the inner [`OpTransactionValidator`].
    /// Note: This is only available if you have an owned instance.
    pub fn into_inner(self) -> OpTransactionValidator<Client, Tx>
    where
        Client: Clone,
        Tx: Clone,
    {
        Arc::try_unwrap(self.inner).unwrap_or_else(|arc| (*arc).clone())
    }
}

impl<Client, Tx> XLayerTransactionValidator<Client, Tx>
where
    Client: ChainSpecProvider + BlockReaderIdExt,
    Tx: PoolTransaction,
{
    /// Gets the current base fee from the latest block header.
    ///
    /// This should be called once for batch validation and reused across transactions.
    fn get_base_fee(&self) -> u64 {
        self.inner
            .client()
            .latest_header()
            .ok()
            .flatten()
            .and_then(|header| header.header().base_fee_per_gas())
            .unwrap_or(0)
    }

    /// Gets the minimum gas price from the pricer cache.
    ///
    /// Returns `None` if pricer is not configured or minimum gas price is unavailable.
    /// This should be called once for batch validation and reused across transactions.
    fn get_min_gas_price(&self) -> Option<U256> {
        let pricer = self.get_pricer()?;
        let min_price = pricer.get_gas_cache().get_latest();
        
        if min_price.is_zero() {
            None
        } else {
            Some(min_price)
        }
    }

    /// Filters a transaction based on XLayer gas price requirements.
    ///
    /// Returns `Some(error)` if the transaction should be rejected, `None` if it passes.
    fn filter_transaction(&self, transaction: &Tx) -> Option<InvalidPoolTransactionError> {
        let base_fee = self.get_base_fee();
        let min_price = match self.get_min_gas_price() {
            Some(price) => price,
            None => {
                debug!(target: "reth::cli", "XLayer: No pricer configured or min price unavailable, delegating to inner validator");
                return None;
            }
        };
        
        self.filter_transaction_with_context(transaction, base_fee, min_price)
    }

    /// Filters a transaction based on XLayer gas price requirements with pre-fetched context.
    ///
    /// This is more efficient for batch validation as it avoids fetching the base fee and 
    /// minimum gas price for each transaction.
    ///
    /// Returns `Some(error)` if the transaction should be rejected, `None` if it passes.
    fn filter_transaction_with_context(
        &self,
        transaction: &Tx,
        base_fee: u64,
        min_price: U256,
    ) -> Option<InvalidPoolTransactionError> {
        // Calculate the transaction's effective gas price
        let tx_gas_price = self.calculate_effective_gas_price(transaction, base_fee);

        // Check if transaction meets minimum gas price requirement
        if tx_gas_price < min_price {
            debug!(
                target: "reth::cli",
                "XLayer: Transaction rejected due to insufficient gas price, tx_gas_price={}, min_gas_price={}",
                tx_gas_price,
                min_price
            );
            return Some(InvalidPoolTransactionError::Underpriced);
        }

        trace!(
            target: "reth::cli",
            "XLayer: Transaction accepted, tx_gas_price={}, min_gas_price={}",
            tx_gas_price,
            min_price
        );

        None
    }

    /// Calculates the effective gas price for a transaction with a given base fee.
    ///
    /// For legacy transactions (type 0 and 1), this is simply the gas price.
    /// For EIP-1559 transactions (type 2), this is min(baseFee + tip, feeCap).
    fn calculate_effective_gas_price(&self, transaction: &Tx, base_fee: u64) -> U256 {
        // For dynamic fee transactions (EIP-1559), calculate effective gas price
        if transaction.is_dynamic_fee() {
            // Calculate effective gas price: min(tip + baseFee, feeCap)
            let tip = transaction.max_priority_fee_per_gas().unwrap_or(0);
            let fee_cap = transaction.max_fee_per_gas();
            let tip_plus_base_fee = U256::from(tip).saturating_add(U256::from(base_fee));
            let fee_cap_u256 = U256::from(fee_cap);

            let effective_price = if tip_plus_base_fee < fee_cap_u256 {
                tip_plus_base_fee
            } else {
                fee_cap_u256
            };

            trace!(
                target: "reth::cli",
                "XLayer: Transaction effective gas price, effective_gas_price={}, base_fee={}, tip={}, fee_cap={}",
                effective_price,
                base_fee,
                tip,
                fee_cap
            );

            effective_price
        } else {
            // For legacy transactions, use the gas price directly
            U256::from(transaction.gas_price().unwrap_or(transaction.max_fee_per_gas()))
        }
    }

    /// Filters a batch of validation results from inner validator based on XLayer gas price requirements.
    ///
    /// This method takes validation results from the inner validator and applies XLayer-specific
    /// gas price filtering. Valid transactions that don't meet the minimum gas price are converted
    /// to Invalid outcomes.
    ///
    /// # Arguments
    /// * `inner_results` - Validation results from inner validator
    ///
    /// # Returns
    /// A new vector with XLayer filtering applied, or the original results if filtering is skipped
    fn filter_transactions(
        &self,
        inner_results: Vec<TransactionValidationOutcome<Tx>>,
    ) -> Vec<TransactionValidationOutcome<Tx>> {
        // Get base fee and min price once for all transactions to avoid repeated calls
        let base_fee = self.get_base_fee();
        let min_price = match self.get_min_gas_price() {
            Some(price) => price,
            None => {
                debug!(target: "reth::cli", "XLayer: No pricer configured or min price unavailable, skipping XLayer filtering");
                // No XLayer filtering, return original results
                return inner_results;
            }
        };
        
        // Apply XLayer gas price filtering to each result
        inner_results
            .into_iter()
            .map(|outcome| {
                match outcome {
                    TransactionValidationOutcome::Valid { transaction, balance, state_nonce, bytecode_hash, propagate, authorities } => {
                        // Check if transaction passes XLayer gas price requirements
                        if let Some(err) = self.filter_transaction_with_context(transaction.transaction(), base_fee, min_price) {
                            // Transaction failed XLayer check, convert to Invalid
                            TransactionValidationOutcome::Invalid(transaction.into_transaction(), err)
                        } else {
                            // Transaction passed, keep as Valid
                            TransactionValidationOutcome::Valid {
                                transaction,
                                balance,
                                state_nonce,
                                bytecode_hash,
                                propagate,
                                authorities,
                            }
                        }
                    }
                    // Keep Invalid and Error outcomes as-is
                    other => other,
                }
            })
            .collect()
    }
}

impl<Client, Tx> TransactionValidator for XLayerTransactionValidator<Client, Tx>
where
    Client: Debug + Send + Sync + ChainSpecProvider + BlockReaderIdExt,
    Tx: Debug + Send + Sync + PoolTransaction,
    OpTransactionValidator<Client, Tx>: TransactionValidator<Transaction = Tx>,
{
    type Transaction = Tx;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        trace!(target: "reth::cli", "XLayer: Validating transaction");
        
        // Apply XLayer gas price filtering before delegating to inner validator
        if let Some(err) = self.filter_transaction(&transaction) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }
        
        self.inner.validate_transaction(origin, transaction).await
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        debug!(target: "reth::cli", "XLayer: Validating {} transactions", transactions.len());
        
        // First, delegate to inner validator for basic validation
        let inner_results = self.inner.validate_transactions(transactions).await;
        
        // Apply XLayer gas price filtering to the results
        self.filter_transactions(inner_results)
    }

    async fn validate_transactions_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = Self::Transaction> + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        trace!(target: "reth::cli", "XLayer: Validating transactions with origin");
        
        // First, delegate to inner validator for basic validation
        let inner_results = self.inner.validate_transactions_with_origin(origin, transactions).await;
        
        // Apply XLayer gas price filtering to the results
        self.filter_transactions(inner_results)
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: Block,
    {
        trace!(target: "reth::cli", "XLayer: On new head block");
        self.inner.on_new_head_block(new_tip_block);
    }
}

