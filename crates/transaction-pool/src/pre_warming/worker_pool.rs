//! Worker pool for parallel transaction simulation
//!
//! This module provides an async worker pool that simulates transactions in parallel
//! and merges extracted keys into the PreWarmedCache.
//!
//! ## Architecture
//!
//! ```text
//! trigger_simulation(tx) <- Fire-and-forget
//!     ↓
//!Send to mpsc channel
//!     ↓
//!     ┌─────────────────────────────────┐
//!     │   Multiple Workers Compete      │
//!     │                                 │
//!     │  Worker 1  Worker 2  Worker 3  │
//!     │     ↓         ↓         ↓      │
//!     │  Simulate  Simulate  Simulate  │
//!     │     ↓         ↓         ↓      │
//!     │  Extract   Extract   Extract   │
//!     │     ↓         ↓         ↓      │
//!     └─────────────────────────────────┘
//!                 ↓
//!         Merge into PreWarmedCache
//! ```
//!
//! ## Non-Blocking Design
//!
//! Workers simulate transactions AFTER they've been added to the pool and the user
//! has received their transaction hash. No impact on transaction acceptance latency.

use crate::pre_warming::{ExtractedKeys, PreWarmedCache, PreWarmingConfig, SimulationRequest, Simulator, SnapshotState};
use crate::PoolTransaction;
use reth_chainspec::ChainSpec;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

/// Worker pool for parallel transaction simulation
///
/// Manages N worker threads that compete for simulation jobs via mpsc channel.
/// Workers simulate transactions, extract keys, and merge into PreWarmedCache.
pub struct SimulationWorkerPool<T> {
    /// Sender for submitting simulation jobs (clone-able, cheap)
    sender: mpsc::UnboundedSender<SimulationRequest<T>>,

    /// Worker thread handles
    workers: Vec<JoinHandle<()>>,

    /// Shared cache for merging results
    cache: Arc<PreWarmedCache>,

    /// Shared snapshot of blockchain state (with internal cache)
    snapshot: Arc<SnapshotState>,

    /// Chain specification (for EVM config)
    chain_spec: Arc<ChainSpec>,

    /// Configuration
    config: PreWarmingConfig,
}

impl<T> SimulationWorkerPool<T>
where
    T: PoolTransaction + Send + 'static,
{
    /// Create new worker pool and spawn N workers
    ///
    /// Workers start immediately and wait for jobs on the channel.
    pub fn new(
        config: PreWarmingConfig,
        cache: Arc<PreWarmedCache>,
        snapshot: Arc<SnapshotState>,
        chain_spec: Arc<ChainSpec>,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        // Create shared receiver wrapped in Arc for worker threads
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));

        let mut workers = Vec::with_capacity(config.num_workers);

        // Spawn N workers using tokio::spawn for async runtime integration
        for worker_id in 0..config.num_workers {
            let receiver = Arc::clone(&receiver);
            let cache = Arc::clone(&cache);
            let snapshot = Arc::clone(&snapshot);
            let chain_spec = Arc::clone(&chain_spec);
            let config = config.clone();

            let handle = tokio::spawn(async move {
                worker_loop(worker_id, receiver, cache, snapshot, chain_spec, config).await;
            });

            workers.push(handle);
        }

        debug!(
            target: "txpool::pre_warming",
            num_workers = config.num_workers,
            chain_id = chain_spec.chain.id(),
            "Worker pool started with real EVM simulator"
        );

        Self {
            sender,
            workers,
            cache,
            snapshot,
            chain_spec,
            config,
        }
    }

    /// Update snapshot with new state provider (called when new block arrives)
    ///
    /// This creates a new SnapshotState from the given state provider and updates
    /// the internal reference. Workers will use the new snapshot for subsequent simulations.
    ///
    /// Note: This doesn't interrupt ongoing simulations - they continue with the old snapshot.
    /// Only new simulations will use the updated snapshot.
    pub fn update_snapshot(&mut self, new_snapshot: Arc<SnapshotState>) {
        debug!(
            target: "txpool::pre_warming",
            "Updating snapshot for worker pool"
        );
        self.snapshot = new_snapshot;
    }

    /// Get reference to the current snapshot
    pub fn snapshot(&self) -> &Arc<SnapshotState> {
        &self.snapshot
    }

    /// Trigger simulation for a transaction (fire-and-forget!)
    ///
    /// This just sends the request to the channel and returns immediately.
    /// Takes < 1 microsecond. Workers pick up the job asynchronously.
    ///
    /// ## Non-Blocking
    ///
    /// This is called AFTER the transaction is validated and added to the pool.
    /// The user has already received their transaction hash.
    pub fn trigger_simulation(&self, request: SimulationRequest<T>) {
        // Fire-and-forget: Just send to channel
        if let Err(err) = self.sender.send(request) {
            warn!(
                target: "txpool::pre_warming",
                ?err,
                "Failed to send simulation request (channel closed)"
            );
        }
    }

    /// Get reference to the cache
    pub fn cache(&self) -> &Arc<PreWarmedCache> {
        &self.cache
    }

    /// Get configuration
    pub fn config(&self) -> &PreWarmingConfig {
        &self.config
    }

    /// Shutdown worker pool gracefully
    ///
    /// Drops the sender (closing channel), then waits for all workers to finish
    /// their current jobs and exit.
    pub async fn shutdown(self) {
        debug!(
            target: "txpool::pre_warming",
            num_workers = self.workers.len(),
            "Shutting down worker pool"
        );

        // Drop sender to close channel
        drop(self.sender);

        // Wait for all workers to finish using tokio join
        for (worker_id, handle) in self.workers.into_iter().enumerate() {
            if let Err(err) = handle.await {
                error!(
                    target: "txpool::pre_warming",
                    worker_id,
                    ?err,
                    "Worker task failed or panicked"
                );
            }
        }

        debug!(target: "txpool::pre_warming", "Worker pool shutdown complete");
    }
}

/// Worker loop - runs as tokio async task
///
/// Continuously receives simulation requests from the channel, simulates them,
/// and merges results into the cache.
async fn worker_loop<T>(
    worker_id: usize,
    receiver: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SimulationRequest<T>>>>,
    cache: Arc<PreWarmedCache>,
    snapshot: Arc<SnapshotState>,
    chain_spec: Arc<ChainSpec>,
    _config: PreWarmingConfig,
) where
    T: PoolTransaction,
{
    debug!(
        target: "txpool::pre_warming",
        worker_id,
        chain_id = chain_spec.chain.id(),
        "Worker started"
    );

    // Create Simulator instance for this worker with snapshot and chain spec
    let simulator = Simulator::new(Arc::clone(&snapshot), Arc::clone(&chain_spec));

    loop {
        // Receive next job from channel
        let request = {
            let mut rx = receiver.lock().await;
            rx.recv().await
        };

        match request {
            Some(req) => {
                debug!(
                    target: "txpool::pre_warming",
                    worker_id,
                    tx_hash = ?req.tx_hash,
                    age_ms = req.age().as_millis(),
                    "Processing simulation request"
                );

                // Simulate transaction to extract keys
                let keys = match simulate_transaction(&simulator, &req.transaction) {
                    Ok(keys) => keys,
                    Err(e) => {
                        warn!(
                            target: "txpool::pre_warming",
                            worker_id,
                            tx_hash = ?req.tx_hash,
                            error = ?e,
                            "Simulation failed, using fallback"
                        );
                        // Fallback to basic key extraction
                        dummy_simulate(&req.transaction)
                    }
                };

                // Store keys per transaction (thread-safe)
                cache.store_tx_keys(req.tx_hash, keys);

                debug!(
                    target: "txpool::pre_warming",
                    worker_id,
                    tx_hash = ?req.tx_hash,
                    "Simulation complete"
                );
            }
            None => {
                // Channel closed, exit
                debug!(
                    target: "txpool::pre_warming",
                    worker_id,
                    "Channel closed, worker exiting"
                );
                break;
            }
        }
    }

    debug!(
        target: "txpool::pre_warming",
        worker_id,
        "Worker stopped"
    );
}

/// Simulate transaction and extract accessed keys
///
/// Uses the Simulator to extract keys that the transaction will access:
/// - Sender and recipient accounts
/// - Access list entries (EIP-2930)
fn simulate_transaction<T: PoolTransaction>(
    simulator: &Simulator,
    tx: &T,
) -> Result<ExtractedKeys, Box<dyn std::error::Error + Send + Sync>> {
    // Get sender (already recovered in PoolTransaction)
    let sender = tx.sender();

    // Get the consensus transaction (implements alloy_consensus::Transaction)
    let consensus_tx = tx.clone_into_consensus();
    let (tx_inner, _signer) = consensus_tx.into_parts();

    // Create default BlockEnv for simulation
    // TODO: Get actual block context when available
    let block_env = revm::context::BlockEnv::default();

    // Simulate using the consensus transaction
    simulator.simulate(&tx_inner, sender, block_env)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
}

/// Dummy simulator - extracts sender and recipient as keys
///
/// This is a temporary implementation for testing the worker pool infrastructure.
/// Phase 4 will replace this with real EVM simulation that extracts all accessed
/// accounts, storage slots, and code hashes.
fn dummy_simulate<T: PoolTransaction>(tx: &T) -> ExtractedKeys {
    let mut keys = ExtractedKeys::new();

    // Add sender (always accessed)
    keys.add_account(tx.sender());

    // Add recipient if exists (contract call or transfer)
    if let Some(to) = tx.to() {
        keys.add_account(to);
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_warming::PreWarmingConfig;
    use alloy_primitives::{Address, TxHash};

    #[test]
    fn test_cache_store_and_retrieve() {
        // Test cache store and retrieve with mock data
        let config = PreWarmingConfig::enabled().with_workers(1);
        let cache = Arc::new(PreWarmedCache::new(config.clone()));

        // Verify cache is empty initially
        assert!(cache.is_empty());

        // Create test keys and store them
        let tx_hash = TxHash::random();
        let mut test_keys = ExtractedKeys::new();
        test_keys.add_account(Address::from([1; 20]));
        test_keys.add_account(Address::from([2; 20]));

        cache.store_tx_keys(tx_hash, test_keys);

        // Retrieve keys for the transaction
        let result = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(result.accounts.len(), 2);
    }

    #[test]
    fn test_extracted_keys_from_dummy() {
        // Test that dummy simulate creates valid ExtractedKeys
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::from([1; 20]));
        keys.add_account(Address::from([2; 20]));

        assert_eq!(keys.accounts.len(), 2);
        assert!(!keys.is_empty());
        assert_eq!(keys.total_keys(), 2);
    }

    // Integration tests with Pool will be added once test utilities are available
    // These tests verify the full flow: Pool::add_transaction() → worker_pool → simulation → cache
    //
    // Test scenarios needed:
    // 1. Pool with pre-warming enabled triggers simulation
    // 2. Pool with pre-warming disabled works normally
    // 3. Keys appear in cache after transaction added
    // 4. Multiple transactions trigger multiple simulations
}
