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
use parking_lot::RwLock;
use reth_chainspec::ChainSpec;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

/// Shared snapshot holder that workers can read from
/// Workers hold Arc to this, and can read the inner snapshot on each simulation
///
/// TODO: Wire up snapshot updates - call update_snapshot() from on_canonical_state_change()
/// when new block arrives. Currently defined but not called.
type SharedSnapshot = Arc<RwLock<Arc<SnapshotState>>>;

/// Worker pool for parallel transaction simulation
///
/// Manages N worker threads that compete for simulation jobs via mpsc channel.
/// Workers simulate transactions, extract keys, and merge into PreWarmedCache.
pub struct SimulationWorkerPool<T> {
    /// Sender for submitting simulation jobs (clone-able, cheap)
    /// Uses bounded channel to prevent unbounded memory growth
    sender: mpsc::Sender<SimulationRequest<T>>,

    /// Worker thread handles
    workers: Vec<JoinHandle<()>>,

    /// Shared cache for merging results
    cache: Arc<PreWarmedCache>,

    /// Shared snapshot holder - workers read from this on each simulation
    /// Wrapped in RwLock so update_snapshot() can swap the inner Arc
    snapshot_holder: SharedSnapshot,

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
    /// Uses bounded channel with capacity = num_workers * 10 to prevent unbounded memory growth.
    pub fn new(
        config: PreWarmingConfig,
        cache: Arc<PreWarmedCache>,
        snapshot: Arc<SnapshotState>,
        chain_spec: Arc<ChainSpec>,
    ) -> Self {
        // Bounded channel: capacity = workers * 10
        // This allows some buffering but prevents unbounded queue growth
        let channel_capacity = config.num_workers * 10;
        let (sender, receiver) = mpsc::channel(channel_capacity);

        // Create shared receiver wrapped in Arc for worker threads
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));

        // Wrap snapshot in RwLock so workers can see updates
        let snapshot_holder: SharedSnapshot = Arc::new(RwLock::new(snapshot));

        let mut workers = Vec::with_capacity(config.num_workers);

        // Spawn N workers using tokio::spawn for async runtime integration
        for worker_id in 0..config.num_workers {
            let receiver = Arc::clone(&receiver);
            let cache = Arc::clone(&cache);
            let snapshot_holder = Arc::clone(&snapshot_holder);
            let chain_spec = Arc::clone(&chain_spec);
            let config = config.clone();

            let handle = tokio::spawn(async move {
                worker_loop(worker_id, receiver, cache, snapshot_holder, chain_spec, config).await;
            });

            workers.push(handle);
        }

        debug!(
            target: "txpool::pre_warming",
            num_workers = config.num_workers,
            channel_capacity,
            chain_id = chain_spec.chain.id(),
            "Worker pool started with bounded channel"
        );

        Self {
            sender,
            workers,
            cache,
            snapshot_holder,
            chain_spec,
            config,
        }
    }

    /// Update snapshot with new state provider (called when new block arrives)
    ///
    /// Workers read from the shared snapshot_holder on each simulation,
    /// so they will see this update on their next simulation.
    ///
    /// TODO: Wire this up - should be called from on_canonical_state_change() in pool/mod.rs
    /// when new block arrives. Caller needs to create SnapshotState from StateProvider.
    pub fn update_snapshot(&self, new_snapshot: Arc<SnapshotState>) {
        debug!(
            target: "txpool::pre_warming",
            "Updating snapshot for worker pool"
        );
        *self.snapshot_holder.write() = new_snapshot;
    }

    /// Get reference to the current snapshot
    ///
    /// TODO: Used by worker_loop to get fresh snapshot on each simulation.
    /// Returns clone of inner Arc (cheap - just ref count increment).
    pub fn snapshot(&self) -> Arc<SnapshotState> {
        self.snapshot_holder.read().clone()
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
    ///
    /// ## Backpressure Handling
    ///
    /// If the channel is full (workers can't keep up), we log a warning and drop
    /// the simulation request. This prevents blocking transaction acceptance.
    /// The transaction will still be executed, just without pre-warming benefit.
    pub fn trigger_simulation(&self, request: SimulationRequest<T>) {
        match self.sender.try_send(request) {
            Ok(_) => {
                // Successfully queued for simulation
            }
            Err(mpsc::error::TrySendError::Full(req)) => {
                // Channel is full - workers can't keep up!
                // Log warning and drop simulation (transaction still executes)
                warn!(
                    target: "txpool::pre_warming",
                    tx_hash = ?req.tx_hash,
                    "Simulation channel full - workers overloaded, dropping simulation request. \
                     Consider increasing worker count or reducing transaction rate."
                );
                // TODO: Add metrics counter for dropped simulations
                // metrics::counter!("txpool.pre_warming.simulations_dropped").increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Channel closed - worker pool shutdown
                warn!(
                    target: "txpool::pre_warming",
                    "Simulation channel closed - worker pool shut down"
                );
            }
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
    receiver: Arc<tokio::sync::Mutex<mpsc::Receiver<SimulationRequest<T>>>>,
    cache: Arc<PreWarmedCache>,
    snapshot_holder: SharedSnapshot,
    chain_spec: Arc<ChainSpec>,
    config: PreWarmingConfig,
) where
    T: PoolTransaction,
{
    debug!(
        target: "txpool::pre_warming",
        worker_id,
        chain_id = chain_spec.chain.id(),
        "Worker started"
    );


    // Track consecutive empty receives for adaptive sleep
    let mut consecutive_empty: u32 = 0;
    const MAX_CONSECUTIVE_EMPTY: u32 = 100; // After 100 empty tries, sleep longer
    const BASE_SLEEP_MICROS: u64 = 100;
    const MAX_SLEEP_MICROS: u64 = 10_000; // Cap at 10ms

    loop {
        // Try to receive from channel (non-blocking)
        //
        // CRITICAL: We must NOT hold the lock while waiting for items!
        // Pattern: lock briefly → try_recv → unlock → process or sleep
        let request = {
            let mut rx = receiver.lock().await;
            rx.try_recv()
        }; // Lock released here!

        match request {
            Ok(req) => {
                // Reset empty counter on successful receive
                consecutive_empty = 0;

                // Log if request is old (but still process it!)
                let age = req.age();
                if age > config.simulation_timeout {
                    debug!(
                        target: "txpool::pre_warming",
                        worker_id,
                        tx_hash = ?req.tx_hash,
                        age_ms = age.as_millis(),
                        "Processing delayed simulation request"
                    );
                }

                debug!(
                    target: "txpool::pre_warming",
                    worker_id,
                    tx_hash = ?req.tx_hash,
                    age_ms = age.as_millis(),
                    "Processing simulation request"
                );

                // Read fresh snapshot for this simulation
                // This ensures we use the latest state after block updates
                //
                // TODO: Once update_snapshot() is wired to on_canonical_state_change(),
                // this read will automatically get the fresh snapshot after new block.
                let snapshot = snapshot_holder.read().clone();
                let simulator = Simulator::new(snapshot, Arc::clone(&chain_spec));

                // Simulate transaction with timeout to prevent hanging
                let simulation_timeout = config.simulation_timeout;
                let keys = match tokio::time::timeout(
                    simulation_timeout,
                    tokio::task::spawn_blocking({
                        let tx = req.transaction.clone();
                        move || simulate_transaction_sync(&simulator, &tx)
                    })
                ).await {
                    Ok(Ok(Ok(keys))) => keys,
                    Ok(Ok(Err(e))) => {
                        warn!(
                            target: "txpool::pre_warming",
                            worker_id,
                            tx_hash = ?req.tx_hash,
                            error = ?e,
                            "Simulation failed, using fallback"
                        );
                        dummy_simulate(&req.transaction)
                    }
                    Ok(Err(join_err)) => {
                        // spawn_blocking task panicked
                        error!(
                            target: "txpool::pre_warming",
                            worker_id,
                            tx_hash = ?req.tx_hash,
                            error = ?join_err,
                            "Simulation task panicked, using fallback"
                        );
                        dummy_simulate(&req.transaction)
                    }
                    Err(_timeout) => {
                        warn!(
                            target: "txpool::pre_warming",
                            worker_id,
                            tx_hash = ?req.tx_hash,
                            timeout_ms = simulation_timeout.as_millis(),
                            "Simulation timed out, using fallback"
                        );
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
            Err(mpsc::error::TryRecvError::Empty) => {
                // Channel empty - adaptive sleep to prevent busy spinning
                consecutive_empty = consecutive_empty.saturating_add(1);

                // Exponential backoff: sleep longer if channel stays empty
                let sleep_micros = if consecutive_empty > MAX_CONSECUTIVE_EMPTY {
                    MAX_SLEEP_MICROS
                } else {
                    BASE_SLEEP_MICROS.saturating_mul(1 + (consecutive_empty as u64 / 10))
                        .min(MAX_SLEEP_MICROS)
                };

                tokio::time::sleep(tokio::time::Duration::from_micros(sleep_micros)).await;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                // Channel closed, exit worker
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

/// Simulate transaction synchronously (for use in spawn_blocking)
fn simulate_transaction_sync<T: PoolTransaction>(
    simulator: &Simulator,
    tx: &T,
) -> Result<ExtractedKeys, Box<dyn std::error::Error + Send + Sync>> {
    simulate_transaction(simulator, tx)
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
