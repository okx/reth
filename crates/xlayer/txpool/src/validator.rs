//! XLayer transaction validator.

use reth_optimism_txpool::OpTransactionValidator;
use reth_rpc_eth_api::helpers::pricer::L2GasPricer;
use reth_transaction_pool::{
    PoolTransaction, TransactionOrigin, TransactionValidationOutcome, TransactionValidator,
};
use reth_primitives_traits::{Block, SealedBlock};
use parking_lot::RwLock;
use std::fmt::Debug;
use std::sync::Arc;
use tracing::debug;

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

impl<Client, Tx> TransactionValidator for XLayerTransactionValidator<Client, Tx>
where
    Client: Debug + Send + Sync,
    Tx: Debug + Send + Sync + PoolTransaction,
    OpTransactionValidator<Client, Tx>: TransactionValidator<Transaction = Tx>,
{
    type Transaction = Tx;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        // TODO: Add XLayer-specific transaction filtering logic here
        // For now, just delegate to the inner validator
        debug!(target: "reth::cli", "XLayer: Validating transaction");
        self.inner.validate_transaction(origin, transaction).await
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        // TODO: Add XLayer-specific transaction filtering logic here
        // For now, just delegate to the inner validator
        debug!(target: "reth::cli", "XLayer: Validating transactions");
        self.inner.validate_transactions(transactions).await
    }

    async fn validate_transactions_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = Self::Transaction> + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        // TODO: Add XLayer-specific transaction filtering logic here
        // For now, just delegate to the inner validator
        debug!(target: "reth::cli", "XLayer: Validating transactions with origin");
        self.inner.validate_transactions_with_origin(origin, transactions).await
    }

    fn on_new_head_block<B>(&self, new_tip_block: &SealedBlock<B>)
    where
        B: Block,
    {
        debug!(target: "reth::cli", "XLayer: On new head block");
        self.inner.on_new_head_block(new_tip_block);
    }
}

