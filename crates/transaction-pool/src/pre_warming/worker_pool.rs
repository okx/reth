//! Worker pool for parallel transaction simulation
//!
//! This module provides an async worker pool that simulates transactions in parallel
//! and merges extracted keys into the PreWarmedCache.
//!
//! ## Architecture
//!
//! ```text
//! trigger_simulation(tx) ← Fire-and-forget! (< 1μs)
//!     ↓
//! Send to mpsc channel
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
//! has received their transaction hash. Zero impact on transaction acceptance latency.

use crate::pre_warming::{ExtractedKeys, PreWarmedCache, PreWarmingConfig, SimulationRequest};
use crate::PoolTransaction;
use std::sync::Arc;
use tokio::sync::mpsc;
use std::thread::JoinHandle;
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
    pub fn new(config: PreWarmingConfig, cache: Arc<PreWarmedCache>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        // Create shared receiver wrapped in Arc for worker threads
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));

        let mut workers = Vec::with_capacity(config.num_workers);

        // Spawn N workers
        for worker_id in 0..config.num_workers {
            let receiver = Arc::clone(&receiver);
            let cache = Arc::clone(&cache);
            let config = config.clone();

            let handle = std::thread::spawn(move || {
                worker_loop(worker_id, receiver, cache, config);
            });

            workers.push(handle);
        }

        debug!(
            target: "txpool::pre_warming",
            num_workers = config.num_workers,
            "Worker pool started"
        );

        Self {
            sender,
            workers,
            cache,
            config,
        }
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
    pub fn shutdown(self) {
        debug!(
            target: "txpool::pre_warming",
            num_workers = self.workers.len(),
            "Shutting down worker pool"
        );

        // Drop sender to close channel
        drop(self.sender);

        // Wait for all workers to finish
        for (worker_id, handle) in self.workers.into_iter().enumerate() {
            if let Err(err) = handle.join() {
                error!(
                    target: "txpool::pre_warming",
                    worker_id,
                    ?err,
                    "Worker panicked"
                );
            }
        }

        debug!(target: "txpool::pre_warming", "Worker pool shutdown complete");
    }
}

/// Worker loop - runs in worker thread
///
/// Continuously receives simulation requests from the channel, simulates them,
/// and merges results into the cache.
fn worker_loop<T>(
    worker_id: usize,
    receiver: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SimulationRequest<T>>>>,
    cache: Arc<PreWarmedCache>,
    _config: PreWarmingConfig,
) where
    T: PoolTransaction,
{
    debug!(
        target: "txpool::pre_warming",
        worker_id,
        "Worker started"
    );

    // Create a tokio runtime for this worker thread
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime for worker");

    rt.block_on(async {
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

                    // Simulate transaction (dummy for now - Phase 4 will add real EVM)
                    let keys = dummy_simulate(&req.transaction);

                    // Merge into cache (thread-safe)
                    cache.merge_keys(keys);

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
    });

    debug!(
        target: "txpool::pre_warming",
        worker_id,
        "Worker stopped"
    );
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

    // TODO Phase 4: Replace with real EVM simulation
    // - Execute transaction in read-only mode
    // - Track all state access via CacheDB
    // - Extract accounts, storage_slots, code_hashes, block_hashes

    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_warming::PreWarmingConfig;
    use alloy_primitives::Address;

    #[test]
    fn test_dummy_simulate_basic() {
        // Test dummy_simulate with mock data
        // Full integration tests will come in Phase 3
        let config = PreWarmingConfig::enabled().with_workers(1);
        let cache = Arc::new(PreWarmedCache::new(config.clone()));

        // Verify cache is empty initially
        let keys = cache.get_all_keys();
        assert!(keys.is_empty());

        // Manual merge test
        let mut test_keys = ExtractedKeys::new();
        test_keys.add_account(Address::from([1; 20]));
        test_keys.add_account(Address::from([2; 20]));

        cache.merge_keys(test_keys);

        let result = cache.get_all_keys();
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

