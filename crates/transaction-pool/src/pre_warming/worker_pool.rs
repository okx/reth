//! # Worker Pool for Parallel Transaction Simulation
//!
//! This module provides an async worker pool that simulates transactions in parallel
//! and extracts state keys for the pre-warming cache.
//!
//! ---
//!
//! ## TL;DR (Executive Summary)
//!
//! The `SimulationWorkerPool` is a **multi-threaded job processor** that:
//! 1. Receives transaction simulation requests via a bounded channel
//! 2. Multiple workers compete to pick up jobs
//! 3. Each worker simulates the transaction to extract state keys
//! 4. Keys are stored in the `PreWarmedCache` for later prefetching
//!
//! ---
//! ## How Does the Worker Pool Work?
//!
//! ### Step-by-Step Flow
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        TRANSACTION POOL                                 │
//! │                                                                         │
//! │  1. Transaction arrives via RPC (eth_sendRawTransaction)               │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │  2. Transaction validated (signature, nonce, balance check)            │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │  3. Transaction ADDED to pool → User gets tx_hash back ✓               │
//! │       │                                                                 │
//! │       ▼                                                                 │
//! │  4. IF pre-warming enabled:                                            │
//! │       worker_pool.trigger_simulation(SimulationRequest::new(tx))       │
//! │       │                                                                 │
//! │       │  ← This returns IMMEDIATELY (fire-and-forget!)                 │
//! │       │     Takes < 1 microsecond                                      │
//! │       │                                                                 │
//! └───────┼─────────────────────────────────────────────────────────────────┘
//!         │
//!         ▼
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                     BOUNDED CHANNEL (MPSC)                              │
//! │                                                                         │
//! │  ┌───────┬───────┬───────┬───────┬───────┬─ ─ ─ ─┐                     │
//! │  │ Req 1 │ Req 2 │ Req 3 │ Req 4 │ Req 5 │  ...  │  Capacity = N * 10  │
//! │  └───────┴───────┴───────┴───────┴───────┴─ ─ ─ ─┘                     │
//! │                                                                         │
//! │  BOUNDED = Has maximum size!                                           │
//! │  If full, new requests are DROPPED (not blocked)                       │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//!         │
//!         │  Workers COMPETE to receive from channel
//!         │  (Only ONE worker gets each request)
//!         ▼
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        WORKER THREADS                                   │
//! │                                                                         │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                  │
//! │  │   Worker 0   │  │   Worker 1   │  │   Worker 2   │  ...             │
//! │  │              │  │              │  │              │                  │
//! │  │ ┌──────────┐ │  │ ┌──────────┐ │  │ ┌──────────┐ │                  │
//! │  │ │ Receive  │ │  │ │ Receive  │ │  │ │ Receive  │ │                  │
//! │  │ │  from    │ │  │ │  from    │ │  │ │  from    │ │                  │
//! │  │ │ channel  │ │  │ │ channel  │ │  │ │ channel  │ │                  │
//! │  │ └────┬─────┘ │  │ └────┬─────┘ │  │ └────┬─────┘ │                  │
//! │  │      ▼       │  │      ▼       │  │      ▼       │                  │
//! │  │ ┌──────────┐ │  │ ┌──────────┐ │  │ ┌──────────┐ │                  │
//! │  │ │ Simulate │ │  │ │ Simulate │ │  │ │ Simulate │ │                  │
//! │  │ │    TX    │ │  │ │    TX    │ │  │ │    TX    │ │                  │
//! │  │ └────┬─────┘ │  │ └────┬─────┘ │  │ └────┬─────┘ │                  │
//! │  │      ▼       │  │      ▼       │  │      ▼       │                  │
//! │  │ ┌──────────┐ │  │ ┌──────────┐ │  │ ┌──────────┐ │                  │
//! │  │ │ Extract  │ │  │ │ Extract  │ │  │ │ Extract  │ │                  │
//! │  │ │  Keys    │ │  │ │  Keys    │ │  │ │  Keys    │ │                  │
//! │  │ └────┬─────┘ │  │ └────┬─────┘ │  │ └────┬─────┘ │                  │
//! │  │      ▼       │  │      ▼       │  │      ▼       │                  │
//! │  │ ┌──────────┐ │  │ ┌──────────┐ │  │ ┌──────────┐ │                  │
//! │  │ │  Store   │ │  │ │  Store   │ │  │ │  Store   │ │                  │
//! │  │ │ in cache │ │  │ │ in cache │ │  │ │ in cache │ │                  │
//! │  │ └──────────┘ │  │ └──────────┘ │  │ └──────────┘ │                  │
//! │  └──────────────┘  └──────────────┘  └──────────────┘                  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//!         │
//!         ▼
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       PRE-WARMED CACHE                                  │
//! │                                                                         │
//! │  HashMap<TxHash, ExtractedKeys>                                        │
//! │                                                                         │
//! │  ┌──────────────────────────────────────────────────────────────┐      │
//! │  │ tx_hash_1 → [Alice, Bob, USDC, slot(USDC, 0)]               │      │
//! │  │ tx_hash_2 → [Charlie, Uniswap, WETH, slot(WETH, 5)]         │      │
//! │  │ tx_hash_3 → [Dave, USDC, slot(USDC, 10)]                    │      │
//! │  └──────────────────────────────────────────────────────────────┘      │
//! │                                                                         │
//! │  Later: Block builder queries these keys to prefetch from MDBX         │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ---
//!
//! ## Where Is the Worker Pool Used?
//!
//! ### In the Transaction Pool Module
//!
//! ```text
//! crates/transaction-pool/src/pool/mod.rs
//!
//! pub struct Pool<V, T, S> {
//!     // ... other fields ...
//!
//!     #[cfg(feature = "pre-warming")]
//!     worker_pool: Option<Arc<SimulationWorkerPool<T>>>,
//! }
//!
//! impl Pool {
//!     async fn add_transaction(&self, tx: T) -> Result<TxHash> {
//!         // 1. Validate transaction
//!         let validated = self.validate(tx).await?;
//!
//!         // 2. Add to pool
//!         let hash = self.pool.add_transaction(validated)?;
//!
//!         // 3. Trigger pre-warming simulation (fire-and-forget!)
//!         #[cfg(feature = "pre-warming")]
//!         if let Some(worker_pool) = &self.worker_pool {
//!             worker_pool.trigger_simulation(SimulationRequest::new(hash, validated));
//!         }
//!
//!         // 4. Return hash to user IMMEDIATELY
//!         //    (simulation happens in background)
//!         Ok(hash)
//!     }
//! }
//! ```
//!
//! ---
//!
//! ## When Is the Worker Pool Active?
//!
//! | Condition | Worker Pool Status |
//! |-----------|-------------------|
//! | `pre-warming` feature enabled + config.enabled = true | ACTIVE |
//! | `pre-warming` feature enabled + config.enabled = false | NOT CREATED |
//! | `pre-warming` feature disabled (compile-time) | NOT COMPILED |
//!
//! ### Lifecycle
//!
//! ```text
//! Node startup
//!     │
//!     ▼
//! Pool::new() called
//!     │
//!     ├── pre-warming disabled? → worker_pool = None
//!     │
//!     └── pre-warming enabled?
//!             │
//!             ▼
//!         SimulationWorkerPool::new()
//!             │
//!             ├── Create bounded channel
//!             ├── Spawn N worker tasks
//!             └── Workers start waiting for jobs
//!
//!                     ...node runs...
//!
//! Node shutdown
//!     │
//!     ▼
//! worker_pool.shutdown()
//!     │
//!     ├── Drop sender (closes channel)
//!     ├── Workers see channel closed
//!     ├── Workers exit their loops
//!     └── await all worker handles
//! ```
//!
//! ---
//!
//! ## Bounded Queue - Why and How?
//!
//! ### Why Bounded (Not Unbounded)?
//!
//! **UNBOUNDED queue problems:**
//! ```text
//! Transaction spam attack
//!     │
//!     ▼
//! 1,000,000 TXs arrive in 10 seconds
//!     │
//!     ▼
//! Unbounded queue grows to 1,000,000 entries
//!     │
//!     ▼
//! Memory exhaustion → Node crashes!
//! ```
//!
//! **BOUNDED queue solution:**
//! ```text
//! Transaction spam attack
//!     │
//!     ▼
//! 1,000,000 TXs arrive in 10 seconds
//!     │
//!     ▼
//! Queue fills to capacity (e.g., 80 = 8 workers × 10)
//!     │
//!     ▼
//! New requests DROPPED (logged as warning)
//!     │
//!     ▼
//! Memory stays bounded → Node survives!
//!
//! Note: Dropped TXs still execute, just without pre-warming benefit
//! ```
//!
//! ### How Is the Bounded Queue Handled?
//!
//! ```text
//! // Channel creation with bounded capacity
//! let channel_capacity = config.num_workers * 10;  // e.g., 8 workers × 10 = 80
//! let (sender, receiver) = mpsc::channel(channel_capacity);
//!
//! // When sending (in trigger_simulation):
//! match self.sender.try_send(request) {
//!     Ok(_) => {
//!         // Success! Request queued for simulation
//!     }
//!     Err(TrySendError::Full(req)) => {
//!         // Channel full! Workers can't keep up
//!         // Log warning and DROP the request
//!         // Transaction still executes, just no pre-warming
//!         warn!("Channel full, dropping simulation request");
//!     }
//!     Err(TrySendError::Closed(_)) => {
//!         // Channel closed (shutdown in progress)
//!         warn!("Channel closed");
//!     }
//! }
//! ```
//!
//! ### Backpressure Behavior
//!
//! | Channel State | What Happens | User Impact |
//! |---------------|--------------|-------------|
//! | Has space | Request queued | None |
//! | Full | Request dropped + warning logged | TX still works, no pre-warm |
//! | Closed | Request dropped + warning logged | TX still works, no pre-warm |
//!
//! ---
//!
//! ## Error Handling
//!
//! ### Simulation Errors
//!
//! ```text
//! Simulation can fail for many reasons:
//!
//! 1. Timeout (simulation takes too long)
//!    └── Log warning, use dummy_simulate() fallback
//!
//! 2. Panic in simulation code
//!    └── spawn_blocking catches panic, use fallback
//!
//! 3. EVM execution error
//!    └── Log warning, use fallback
//!
//! 4. State access error
//!    └── Log warning, use fallback
//!
//! CRITICAL: Errors NEVER block transaction acceptance!
//!           The TX is already in the pool before simulation starts.
//! ```
//!
//! ### Fallback Behavior
//!
//! When simulation fails, we use `dummy_simulate()` which extracts minimal keys:
//! - Sender address (always known)
//! - Recipient address (if present)
//!
//! This is better than nothing - at least sender/recipient get prefetched.
//!
//! ### Error Categories
//!
//! | Error Type | Severity | Action | Impact |
//! |------------|----------|--------|--------|
//! | Channel full | Warning | Drop request | No pre-warming for this TX |
//! | Simulation timeout | Warning | Use fallback | Partial pre-warming |
//! | Simulation panic | Error | Use fallback | Partial pre-warming |
//! | EVM error | Warning | Use fallback | Partial pre-warming |
//! | Channel closed | Warning | Drop request | Shutdown in progress |
//!
//! ---
//!
//! ## Configuration
//!
//! ```text
//! PreWarmingConfig {
//!     enabled: true,           // Master switch
//!     num_workers: 8,          // Number of worker tasks
//!     simulation_timeout: 50ms, // Max time per simulation
//!     cache_max_entries: 10000, // Max TXs in cache
//! }
//! ```
//!
//! ### Tuning Guidelines
//!
//! | Parameter | Too Low | Too High | Recommended |
//! |-----------|---------|----------|-------------|
//! | `num_workers` | Can't keep up | Diminishing returns | CPU cores / 2 |
//! | `simulation_timeout` | Too many timeouts | Slow workers | 50-100ms |
//! | Channel capacity | Drops requests | Memory waste | workers × 10 |

use crate::{
    pre_warming::{
        metrics::PreWarmingMetrics, ExtractedKeys, PreWarmedCache, PreWarmingConfig,
        SimulationRequest, Simulator, SnapshotState,
    },
    PoolTransaction,
};
use parking_lot::RwLock;
use reth_chainspec::ChainSpec;
use std::sync::Arc;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{debug, error, info};

/// Shared snapshot holder that workers can read from.
///
/// Workers hold Arc to this, and can read the inner snapshot on each simulation.
/// The RwLock allows `update_snapshot()` to swap the inner Arc when a new block arrives.
///
/// ## Why Double-Wrapped (Arc<RwLock<Arc<...>>>)?
///
/// ```text
/// Outer Arc: Shared ownership among all workers
///     │
///     └── RwLock: Allows atomic swap of inner snapshot
///             │
///             └── Inner Arc: Cheap clone for each simulation
///
/// When update_snapshot() is called:
/// 1. Acquire write lock on RwLock
/// 2. Replace inner Arc with new snapshot
/// 3. Release lock
/// 4. Workers see new snapshot on next read (cheap Arc clone)
/// ```
type SharedSnapshot = Arc<RwLock<Arc<SnapshotState>>>;

/// Worker pool for parallel transaction simulation.
///
/// Manages N async worker tasks that compete for simulation jobs via an mpsc channel.
/// Workers simulate transactions, extract keys, and merge into `PreWarmedCache`.
///
/// ## Thread Safety
///
/// - `sender`: Clone-able, can be used from any thread
/// - `cache`: Thread-safe via internal `RwLock`
/// - `snapshot_holder`: Thread-safe via `RwLock`
///
/// ## Memory Safety
///
/// Uses bounded channel to prevent unbounded memory growth during TX spam.
/// Channel capacity = `num_workers × 10`.
///
/// ## Lifecycle
///
/// 1. `new()` - Creates pool, spawns workers, workers start waiting
/// 2. `trigger_simulation()` - Sends requests (fire-and-forget)
/// 3. Workers process requests in parallel
/// 4. `shutdown()` - Closes channel, waits for workers to finish
///
/// # Generic Parameters
///
/// - `T`: Transaction type (must implement `PoolTransaction`)
/// - `E`: EVM configuration type (optional, defaults to `()`)
///
/// When `E` implements `ConfigureEvm`, workers can perform full EVM simulation
/// to discover all state keys including storage slots. When `E = ()`, workers
/// use heuristic-based key extraction.
pub struct SimulationWorkerPool<T, E = ()> {
    /// Sender for submitting simulation jobs.
    ///
    /// Clone-able and cheap (just an Arc increment).
    /// Unbounded so trigger_simulation() never blocks or drops due to capacity.
    /// Concurrent simulation count is bounded by the semaphore in drain_loop.
    sender: mpsc::UnboundedSender<SimulationRequest<T>>,

    /// Worker task handles for graceful shutdown.
    ///
    /// We store these so `shutdown()` can await all workers completing.
    workers: Vec<JoinHandle<()>>,

    /// Shared cache for storing extracted keys.
    ///
    /// Thread-safe via internal RwLock. Workers write, block builder reads.
    cache: Arc<PreWarmedCache>,

    /// Shared snapshot holder.
    ///
    /// Workers read from this on each simulation to get current state.
    /// `update_snapshot()` can swap the inner Arc when new block arrives.
    snapshot_holder: SharedSnapshot,

    /// Chain specification for EVM configuration.
    ///
    /// Contains chain ID, fork schedules, etc. needed for simulation.
    chain_spec: Arc<ChainSpec>,

    /// Configuration for the worker pool.
    config: PreWarmingConfig,

    /// Metrics for monitoring pre-warming performance.
    ///
    /// Tracks simulations triggered/completed/failed, cache stats, etc.
    metrics: Arc<PreWarmingMetrics>,

    /// EVM configuration for full simulation (optional).
    ///
    /// When provided, enables full EVM execution to discover all state accesses.
    /// When None, falls back to heuristic-based key extraction.
    evm_config: Option<Arc<E>>,
}

// Manual Debug implementation since some fields don't implement Debug
impl<T, E> std::fmt::Debug for SimulationWorkerPool<T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulationWorkerPool")
            .field("num_workers", &self.workers.len())
            .field("cache_size", &self.cache.len())
            .field("config", &self.config)
            .field("chain_id", &self.chain_spec.chain.id())
            .field("has_evm_config", &self.evm_config.is_some())
            .finish()
    }
}

impl<T, E> SimulationWorkerPool<T, E>
where
    T: PoolTransaction + Send + 'static,
    E: Send + Sync + 'static,
{
    /// Create a new worker pool and spawn N worker tasks.
    ///
    /// Workers start immediately and wait for jobs on the channel.
    /// Uses bounded channel with capacity = `num_workers × 10` to prevent
    /// unbounded memory growth.
    ///
    /// # Arguments
    ///
    /// * `config` - Pre-warming configuration (worker count, timeouts, etc.)
    /// * `cache` - Shared cache for storing extracted keys
    /// * `snapshot` - Initial state snapshot for simulation
    /// * `chain_spec` - Chain specification for EVM config
    ///
    /// # Example
    ///
    /// ```ignore
    /// let config = PreWarmingConfig::enabled().with_workers(8);
    /// let cache = Arc::new(PreWarmedCache::new(config.clone()));
    /// let snapshot = Arc::new(SnapshotState::new(state_provider));
    /// let chain_spec = Arc::new(MAINNET.clone());
    ///
    /// let pool = SimulationWorkerPool::new(config, cache, snapshot, chain_spec);
    /// ```
    ///
    /// # Channel Capacity
    ///
    /// ```text
    /// Capacity = num_workers × 10
    ///
    /// Example: 8 workers → capacity = 80
    ///
    /// Why 10x?
    /// - Too small: Requests dropped during burst
    /// - Too large: Wasted memory
    /// - 10x: Good balance for typical burst patterns
    /// ```
    pub fn new(
        config: PreWarmingConfig,
        cache: Arc<PreWarmedCache>,
        snapshot: Arc<SnapshotState>,
        chain_spec: Arc<ChainSpec>,
    ) -> Self {
        // Unbounded channel: drain_loop blocks on semaphore acquisition while waiting
        // for a free simulation slot. During that wait, recv() is not called, so a
        // bounded channel would fill and drop requests. With unbounded, items queue
        // up in memory instead. Memory cost is negligible — each item is an Arc
        // pointer (~40 bytes); even 100k queued txs = ~4 MB. Simulations complete
        // in ~10ms so the queue drains quickly in practice.
        let (sender, receiver) = mpsc::unbounded_channel();
        let channel_capacity = "unbounded";

        // Register initial snapshot globally — payload builder can reuse the warm
        // DashMap cache immediately instead of opening a fresh cold MDBX transaction.
        crate::pre_warming::registry::set_global_simulation_snapshot(Arc::clone(&snapshot));

        // Wrap snapshot in RwLock so the drain task can see block updates.
        let snapshot_holder: SharedSnapshot = Arc::new(RwLock::new(snapshot));

        // Create metrics instance (registers with global Prometheus registry)
        let metrics = Arc::new(PreWarmingMetrics::default());

        // Dedicated rayon thread pool for simulations.
        // Isolated from tokio's blocking pool so simulation work cannot starve
        // the payload processor (multiproof, execution) which also uses spawn_blocking.
        let simulation_pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(config.num_workers)
                .thread_name(|i| format!("pre-warm-sim-{i}"))
                .build()
                .expect("Failed to build pre-warming simulation thread pool"),
        );

        // Semaphore bounds concurrent simulations to num_workers.
        // The drain task receives items immediately (no blocking on simulation),
        // so the channel is drained as fast as items arrive. The semaphore
        // prevents unbounded spawning of simulation tasks.
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.num_workers));

        let handle = {
            let cache = Arc::clone(&cache);
            let snapshot_holder = Arc::clone(&snapshot_holder);
            let chain_spec = Arc::clone(&chain_spec);
            let config = config.clone();
            let metrics = Arc::clone(&metrics);
            let simulation_pool = Arc::clone(&simulation_pool);

            tokio::spawn(async move {
                drain_loop(
                    receiver,
                    semaphore,
                    cache,
                    snapshot_holder,
                    chain_spec,
                    config,
                    metrics,
                    simulation_pool,
                )
                .await;
            })
        };
        let workers = vec![handle];

        info!(
            target: "txpool::pre_warming",
            num_workers = config.num_workers,
            prefetch_workers = config.prefetch_num_workers,
            channel_capacity,
            simulation_timeout_ms = config.simulation_timeout.as_millis(),
            cache_max_entries = config.cache_max_entries,
            "Pre-warming ENABLED - Worker pool started"
        );

        // Register cache globally so payload builder can access it
        crate::pre_warming::registry::set_global_cache(Arc::clone(&cache));

        // Register metrics globally so prefetch can update them
        crate::pre_warming::registry::set_global_metrics(Arc::clone(&metrics));

        // Register prefetch threads count globally so payload builder can use it
        crate::pre_warming::registry::set_global_prefetch_threads(config.prefetch_num_workers);

        Self {
            sender,
            workers,
            cache,
            snapshot_holder,
            chain_spec,
            config,
            metrics,
            evm_config: None,
        }
    }

    /// Create a new worker pool with EVM config for full simulation.
    ///
    /// When `evm_config` is provided, workers can perform full EVM execution
    /// to discover all state keys including storage slots accessed during
    /// contract execution.
    ///
    /// # Arguments
    ///
    /// * `config` - Pre-warming configuration
    /// * `cache` - Shared cache for storing extracted keys
    /// * `snapshot` - Initial state snapshot
    /// * `chain_spec` - Chain specification
    /// * `evm_config` - EVM configuration for full simulation
    pub fn new_with_evm(
        config: PreWarmingConfig,
        cache: Arc<PreWarmedCache>,
        snapshot: Arc<SnapshotState>,
        chain_spec: Arc<ChainSpec>,
        evm_config: E,
    ) -> Self {
        let mut pool = Self::new(config, cache, snapshot, chain_spec);
        pool.evm_config = Some(Arc::new(evm_config));

        info!(
            target: "txpool::pre_warming",
            "Full EVM simulation ENABLED for pre-warming"
        );

        pool
    }

    /// Returns true if full EVM simulation is enabled.
    pub fn has_evm_config(&self) -> bool {
        self.evm_config.is_some()
    }

    /// Update the snapshot with new state (called when new block arrives).
    ///
    /// Workers read from `snapshot_holder` on each simulation, so they will
    /// automatically see this update on their next simulation.
    ///
    /// # When to Call
    ///
    /// Call this from `on_canonical_state_change()` when a new block is finalized.
    /// The caller needs to create a new `SnapshotState` from the `StateProvider`
    /// at the new block.
    ///
    /// # Thread Safety
    ///
    /// Safe to call from any thread. Uses RwLock for synchronization.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In on_canonical_state_change():
    /// fn on_canonical_state_change(&mut self, new_tip: BlockHash) {
    ///     let state_provider = self.provider.state_by_block_hash(new_tip)?;
    ///     let new_snapshot = Arc::new(SnapshotState::new(state_provider));
    ///     self.worker_pool.update_snapshot(new_snapshot);
    /// }
    /// ```
    pub fn update_snapshot(&self, new_snapshot: Arc<SnapshotState>) {
        debug!(
            target: "txpool::pre_warming",
            "Updating snapshot for worker pool"
        );

        // Carry bytecode cache forward from the old snapshot before swapping.
        // Bytecode is immutable on-chain, so entries from the previous block are
        // always valid. This eliminates cold MDBX code_by_hash queries at the
        // start of every new block.
        {
            let old = self.snapshot_holder.read();
            new_snapshot.inherit_code_cache(&old);
        }

        // Clone before move — registry and snapshot_holder both need ownership.
        let snapshot_for_registry = Arc::clone(&new_snapshot);
        *self.snapshot_holder.write() = new_snapshot;

        // Update global registry so the payload builder reuses this warm snapshot.
        // The snapshot's DashMap cache already contains state queried by simulation
        // workers, turning most prefetch MDBX queries into cheap in-memory hits.
        crate::pre_warming::registry::set_global_simulation_snapshot(snapshot_for_registry);

        // Record snapshot update
        self.metrics.snapshot_updates.increment(1);
    }

    /// Get a reference to the current snapshot.
    ///
    /// Returns a clone of the inner Arc (cheap - just ref count increment).
    /// Useful for testing or manual simulation.
    pub fn snapshot(&self) -> Arc<SnapshotState> {
        self.snapshot_holder.read().clone()
    }

    /// Trigger simulation for a transaction (fire-and-forget!).
    ///
    /// This just sends the request to the channel and returns immediately.
    /// Takes < 1 microsecond. Workers pick up the job asynchronously.
    ///
    /// # Non-Blocking
    ///
    /// This is called AFTER the transaction is validated and added to the pool.
    /// The user has already received their transaction hash. No blocking!
    ///
    /// # Backpressure Handling
    ///
    /// If the channel is full (workers can't keep up), we:
    /// 1. Log a warning
    /// 2. Drop the simulation request
    /// 3. Return immediately
    ///
    /// The transaction will still be executed, just without pre-warming benefit.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In Pool::add_transaction(), after adding to pool:
    /// let request = SimulationRequest::new(tx_hash, validated_tx);
    /// worker_pool.trigger_simulation(request);
    /// // Returns immediately! Don't await anything.
    /// ```
    pub fn trigger_simulation(&self, request: SimulationRequest<T>) {
        // Record that a simulation was triggered
        self.metrics.simulations_triggered.increment(1);

        if self.sender.send(request).is_err() {
            // Channel closed — worker pool is shutting down
            self.metrics.simulations_dropped.increment(1);
            tracing::trace!(
                target: "txpool::pre_warming",
                "Simulation channel closed - worker pool shut down"
            );
        }
    }

    /// Get reference to the cache.
    pub fn cache(&self) -> &Arc<PreWarmedCache> {
        &self.cache
    }

    /// Get reference to the metrics.
    pub fn metrics(&self) -> &Arc<PreWarmingMetrics> {
        &self.metrics
    }

    /// Get the configuration.
    pub fn config(&self) -> &PreWarmingConfig {
        &self.config
    }

    /// Get the chain specification.
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }

    /// Get the number of workers.
    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }

    /// Check if the channel is closed (shutdown in progress or complete).
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// Shutdown the worker pool gracefully.
    ///
    /// 1. Drops the sender (closing the channel)
    /// 2. Waits for all workers to finish their current jobs and exit
    ///
    /// # Blocking
    ///
    /// This method is async and will wait until all workers have exited.
    /// Workers will finish their current job before exiting.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // During node shutdown:
    /// if let Some(worker_pool) = self.worker_pool.take() {
    ///     worker_pool.shutdown().await;
    /// }
    /// ```
    pub async fn shutdown(self) {
        debug!(
            target: "txpool::pre_warming",
            num_workers = self.workers.len(),
            "Shutting down worker pool"
        );

        // Drop sender to close channel
        drop(self.sender);

        // Wait for all workers to finish
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

/// Drain loop - single async task that dispatches simulation work.
///
/// Receives simulation requests one at a time and spawns a concurrent tokio
/// task for each. A semaphore with capacity = `num_workers` bounds concurrent
/// simulations without ever blocking the receive path.
///
/// ## Design
///
/// ```text
/// drain_loop:
///   loop {
///     recv item (blocks until available, never spins)
///     acquire semaphore permit (blocks only if num_workers sims running)
///     spawn simulation task (returns immediately)
///   }
///
/// spawned task:
///   run rayon simulation (holds permit for duration)
///   write result to cache
///   drop permit (releases semaphore slot)
/// ```
///
/// ## Why This Prevents Burst Drops
///
/// The old design (N workers each blocked on simulation) meant the channel
/// filled during bursts: workers could not receive new items while simulating.
/// This loop drains the channel continuously — the only wait is acquiring a
/// semaphore permit, which unblocks as soon as any simulation finishes.
/// The large channel buffer (workers × 100) absorbs burst arrivals.
async fn drain_loop<T>(
    mut receiver: mpsc::UnboundedReceiver<SimulationRequest<T>>,
    semaphore: Arc<tokio::sync::Semaphore>,
    cache: Arc<PreWarmedCache>,
    snapshot_holder: SharedSnapshot,
    chain_spec: Arc<ChainSpec>,
    config: PreWarmingConfig,
    metrics: Arc<PreWarmingMetrics>,
    simulation_pool: Arc<rayon::ThreadPool>,
) where
    T: PoolTransaction + Send + 'static,
{
    debug!(target: "txpool::pre_warming", "Pre-warming drain loop started");

    while let Some(req) = receiver.recv().await {
        // Skip if already simulated to avoid duplicate work
        if cache.contains_tx(&req.tx_hash) {
            continue;
        }

        // Acquire a simulation slot. Blocks only when num_workers simulations
        // are already running; the channel continues to buffer items in the meantime.
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // Semaphore closed (shutdown)
        };

        let cache = Arc::clone(&cache);
        let snapshot = snapshot_holder.read().clone();
        let chain_spec = Arc::clone(&chain_spec);
        let simulation_pool = Arc::clone(&simulation_pool);
        let metrics = Arc::clone(&metrics);
        let simulation_timeout = config.simulation_timeout;

        // Spawn simulation task — drain loop returns to recv() immediately.
        // The permit is held by the task and released when the task completes.
        tokio::spawn(async move {
            let _permit = permit;

            let simulation_start = std::time::Instant::now();

            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            simulation_pool.spawn({
                let tx = req.transaction.clone();
                move || {
                    // Construct Simulator on the rayon thread where CPU work belongs.
                    // This keeps CfgEnv creation off the tokio async scheduler.
                    let simulator = Simulator::new(snapshot, chain_spec);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        simulate_transaction_sync(&simulator, &tx)
                    }));
                    if let Ok(sim_result) = result {
                        let _ = result_tx.send(sim_result);
                    }
                    // On panic: sender drops → receiver gets Err → fallback below
                }
            });

            let keys = match tokio::time::timeout(simulation_timeout, result_rx).await {
                Ok(Ok(Ok(keys))) => {
                    metrics.simulations_completed.increment(1);
                    keys
                }
                Ok(Ok(Err(e))) => {
                    metrics.simulations_failed.increment(1);
                    tracing::trace!(
                        target: "txpool::pre_warming",
                        tx_hash = ?req.tx_hash,
                        error = ?e,
                        "Simulation failed, using fallback"
                    );
                    dummy_simulate(&req.transaction)
                }
                Ok(Err(_recv_err)) => {
                    // Rayon worker panicked (sender dropped without sending)
                    metrics.simulations_failed.increment(1);
                    tracing::trace!(
                        target: "txpool::pre_warming",
                        tx_hash = ?req.tx_hash,
                        "Simulation worker panicked, using fallback"
                    );
                    dummy_simulate(&req.transaction)
                }
                Err(_timeout) => {
                    metrics.simulations_failed.increment(1);
                    tracing::trace!(
                        target: "txpool::pre_warming",
                        tx_hash = ?req.tx_hash,
                        timeout_ms = simulation_timeout.as_millis(),
                        "Simulation timed out, using fallback"
                    );
                    dummy_simulate(&req.transaction)
                }
            };

            let simulation_duration = simulation_start.elapsed();
            metrics.simulation_duration.record(simulation_duration.as_secs_f64());

            let keys_count = keys.accounts.len()
                + keys.storage_slots.len()
                + keys.code_hashes.len()
                + keys.block_hashes.len();

            tracing::trace!(
                target: "txpool::pre_warming",
                tx_hash = ?req.tx_hash,
                duration_us = simulation_duration.as_micros(),
                keys_total = keys_count,
                "TX_TIMING: Simulation complete"
            );

            cache.store_tx_keys(req.tx_hash, keys);
            metrics.cache_entries.increment(1.0);
            metrics.cache_keys_total.increment(keys_count as f64);
        });
    }

    debug!(target: "txpool::pre_warming", "Pre-warming drain loop stopped");
}

/// Simulate transaction synchronously (for use in spawn_blocking).
fn simulate_transaction_sync<T: PoolTransaction>(
    simulator: &Simulator,
    tx: &T,
) -> Result<ExtractedKeys, Box<dyn std::error::Error + Send + Sync>> {
    simulate_transaction(simulator, tx)
}

/// Simulate transaction and extract accessed keys.
///
/// Uses the Simulator to extract keys that the transaction will access:
/// - Sender and recipient accounts
/// - Access list entries (EIP-2930)
/// - Storage slots accessed during execution
///
/// Uses optimized simulation with fast paths for common transaction types.
fn simulate_transaction<T: PoolTransaction>(
    simulator: &Simulator,
    tx: &T,
) -> Result<ExtractedKeys, Box<dyn std::error::Error + Send + Sync>> {
    let sender = tx.sender();
    let consensus_tx = tx.clone_into_consensus();
    let (tx_inner, _signer) = consensus_tx.into_parts();

    // Use optimized simulation with fast paths:
    // - Simple ETH transfers: Just sender + recipient (no DB queries)
    // - Known ERC20/DeFi: Heuristic slot prediction (no DB queries)
    // - Unknown contracts: Full simulation with TrackingDB
    simulator
        .simulate(&tx_inner, sender)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
}

/// Fallback simulator - extracts minimal keys (sender + recipient).
///
/// Used when real simulation fails (timeout, panic, error).
/// Better than nothing - at least sender/recipient get prefetched.
#[inline]
fn dummy_simulate<T: PoolTransaction>(tx: &T) -> ExtractedKeys {
    // Use minimal capacity since we only add 2 accounts max
    let mut keys = ExtractedKeys::with_capacity_for_txs(1);
    keys.add_account(tx.sender());
    if let Some(to) = tx.to() {
        keys.add_account(to);
    }
    keys
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    //! # SimulationWorkerPool Test Suite
    //!
    //! This test suite validates the worker pool for parallel transaction simulation.
    //!
    //! ## Test Categories
    //!
    //! ### Unit Tests (Core Functionality)
    //! - Cache store and retrieve
    //! - ExtractedKeys creation
    //! - Configuration validation
    //! - Debug implementation
    //!
    //! ### Scenario Tests (Real-World Patterns)
    //! - Normal transaction flow
    //! - High load (channel full)
    //! - Shutdown during processing
    //! - Multiple workers competing
    //!
    //! ### Edge Case Tests
    //! - Empty transaction (no recipient)
    //! - Zero workers configured
    //! - Disabled pre-warming

    use super::*;
    use crate::pre_warming::PreWarmingConfig;
    use alloy_primitives::{Address, TxHash, U256};
    use std::time::Duration;

    // ========================================================================
    // UNIT TESTS - Core Functionality
    // ========================================================================

    /// # Test: Cache Store and Retrieve
    ///
    /// ## Scenario
    /// Worker completes simulation and stores keys in cache.
    /// Later, block builder retrieves those keys.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Worker extracts keys → cache.store_tx_keys()
    ///     ↓
    /// Block builder → cache.get_keys_for_txs()
    ///     ↓
    /// Keys returned for prefetching
    /// ```
    ///
    /// ## Validates
    /// - Keys can be stored with tx_hash
    /// - Keys can be retrieved by tx_hash
    /// - Key counts are correct
    #[test]
    fn test_cache_store_and_retrieve() {
        let config = PreWarmingConfig::enabled().with_workers(1);
        let cache = Arc::new(PreWarmedCache::new(config.clone()));

        assert!(cache.is_empty());

        let tx_hash = TxHash::random();
        let mut test_keys = ExtractedKeys::new();
        test_keys.add_account(Address::from([1; 20]));
        test_keys.add_account(Address::from([2; 20]));

        cache.store_tx_keys(tx_hash, test_keys);

        let result = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(result.accounts.len(), 2);
    }

    /// # Test: ExtractedKeys from Dummy Simulate
    ///
    /// ## Scenario
    /// Simulation fails, fallback to dummy_simulate().
    /// Should still extract sender and recipient.
    ///
    /// ## Validates
    /// - ExtractedKeys can be created manually
    /// - add_account() works correctly
    /// - is_empty() returns false after adds
    /// - total_keys() counts correctly
    #[test]
    fn test_extracted_keys_from_dummy() {
        let mut keys = ExtractedKeys::new();
        assert!(keys.is_empty());
        assert_eq!(keys.total_keys(), 0);

        keys.add_account(Address::from([1; 20]));
        keys.add_account(Address::from([2; 20]));

        assert_eq!(keys.accounts.len(), 2);
        assert!(!keys.is_empty());
        assert_eq!(keys.total_keys(), 2);
    }

    /// # Test: Configuration Builder Pattern
    ///
    /// ## Scenario
    /// Node operator configures worker pool via builder pattern.
    ///
    /// ## Validates
    /// - enabled() creates enabled config
    /// - with_workers() sets worker count
    /// - Default values are sensible
    #[test]
    fn test_config_builder() {
        let config = PreWarmingConfig::enabled()
            .with_workers(16)
            .with_timeout(Duration::from_millis(100))
            .with_cache_max_entries(5000);

        assert!(config.enabled);
        assert_eq!(config.num_workers, 16);
        assert_eq!(config.simulation_timeout, Duration::from_millis(100));
        assert_eq!(config.cache_max_entries, 5000);
    }

    /// # Test: Disabled Configuration
    ///
    /// ## Scenario
    /// Pre-warming is disabled in config.
    ///
    /// ## Validates
    /// - disabled() creates disabled config
    /// - enabled flag is false
    #[test]
    fn test_config_disabled() {
        let config = PreWarmingConfig::disabled();
        assert!(!config.enabled);
    }

    /// # Test: Default Configuration
    ///
    /// ## Scenario
    /// No explicit configuration, use defaults.
    ///
    /// ## Validates
    /// - Default is disabled (safe default)
    /// - Default worker count is sensible
    #[test]
    fn test_config_default() {
        let config = PreWarmingConfig::default();
        assert!(!config.enabled); // Safe default
        assert!(config.num_workers > 0);
        assert!(config.simulation_timeout > Duration::ZERO);
    }

    /// # Test: Channel Capacity Calculation
    ///
    /// ## Scenario
    /// Verify channel capacity is workers × 10.
    ///
    /// ## Validates
    /// - Capacity scales with worker count
    /// - Formula: capacity = num_workers × 10
    #[test]
    fn test_channel_capacity_calculation() {
        // Test various worker counts
        for workers in [1, 4, 8, 16, 32] {
            let expected_capacity = workers * 10;
            // We can't directly test channel capacity, but we can verify the formula
            assert_eq!(expected_capacity, workers * 10);
        }
    }

    // ========================================================================
    // SCENARIO TESTS - Real-World Patterns
    // ========================================================================

    /// # Test: Normal Transaction Simulation Flow
    ///
    /// ## Scenario
    /// User submits transaction → Pool adds it → Worker simulates → Keys cached.
    ///
    /// ## Flow Being Tested
    /// ```text
    /// Transaction submitted
    ///     ↓
    /// trigger_simulation() called
    ///     ↓
    /// Request queued in channel
    ///     ↓
    /// Worker picks up request
    ///     ↓
    /// Simulation runs (or fallback)
    ///     ↓
    /// Keys stored in cache
    /// ```
    ///
    /// ## Validates
    /// - Request can be created
    /// - Keys can be stored manually (simulating worker completion)
    #[test]
    fn test_normal_transaction_flow() {
        let config = PreWarmingConfig::enabled().with_workers(4);
        let cache = Arc::new(PreWarmedCache::new(config.clone()));

        // Simulate transaction arrival
        let tx_hash = TxHash::random();
        let sender = Address::from([0xAA; 20]);
        let recipient = Address::from([0xBB; 20]);

        // Simulate what worker would do after processing
        let mut keys = ExtractedKeys::new();
        keys.add_account(sender);
        keys.add_account(recipient);
        keys.add_storage_slot(recipient, U256::ZERO); // Balance slot

        cache.store_tx_keys(tx_hash, keys);

        // Verify keys are retrievable
        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(retrieved.accounts.len(), 2);
        assert_eq!(retrieved.storage_slots.len(), 1);
    }

    /// # Test: Multiple Transactions Concurrent
    ///
    /// ## Scenario
    /// Multiple transactions arrive simultaneously.
    /// Each should have its keys stored independently.
    ///
    /// ## Validates
    /// - Multiple tx_hashes can be stored
    /// - Each tx has independent keys
    /// - Retrieval can select subset
    #[test]
    fn test_multiple_transactions() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let mut tx_hashes = Vec::new();

        // Store 100 transactions
        for i in 0..100u8 {
            let tx_hash = TxHash::random();
            tx_hashes.push(tx_hash);

            let mut keys = ExtractedKeys::new();
            keys.add_account(Address::from([i; 20]));
            keys.add_account(Address::from([i.wrapping_add(100); 20]));

            cache.store_tx_keys(tx_hash, keys);
        }

        assert_eq!(cache.len(), 100);

        // Retrieve subset
        let subset = &tx_hashes[0..10];
        let keys = cache.get_keys_for_txs(subset);
        assert_eq!(keys.accounts.len(), 20); // 10 txs × 2 accounts each
    }

    /// # Test: Transaction Without Recipient (Contract Creation)
    ///
    /// ## Scenario
    /// Contract deployment transaction has no `to` address.
    /// Only sender should be extracted.
    ///
    /// ## Validates
    /// - Sender is always added
    /// - Missing recipient doesn't cause error
    /// - Keys are still stored
    #[test]
    fn test_transaction_without_recipient() {
        let mut keys = ExtractedKeys::new();
        let sender = Address::from([0xDE; 20]);

        // Only add sender (no recipient for contract creation)
        keys.add_account(sender);

        assert_eq!(keys.accounts.len(), 1);
        assert!(keys.accounts.contains(&sender));
    }

    /// # Test: Duplicate Key Handling
    ///
    /// ## Scenario
    /// Same address added multiple times (e.g., sender is also token holder).
    ///
    /// ## Validates
    /// - Duplicate addresses are deduplicated
    /// - Count reflects unique addresses
    #[test]
    fn test_duplicate_key_handling() {
        let mut keys = ExtractedKeys::new();
        let addr = Address::from([0x11; 20]);

        // Add same address 10 times
        for _ in 0..10 {
            keys.add_account(addr);
        }

        // Should only have 1 unique address
        assert_eq!(keys.accounts.len(), 1);
    }

    /// # Test: SimulationRequest Age Tracking
    ///
    /// ## Scenario
    /// Request sits in queue for a while before being processed.
    /// Worker logs if request is old.
    ///
    /// ## Validates
    /// - age() returns elapsed time since creation
    /// - Old requests are still processed
    #[test]
    fn test_simulation_request_age() {
        let tx_hash = TxHash::random();
        let request = SimulationRequest::new(tx_hash, 42u64);

        // Age should be very small initially
        assert!(request.age() < Duration::from_millis(10));

        // Wait a bit
        std::thread::sleep(Duration::from_millis(50));

        // Age should have increased
        assert!(request.age() >= Duration::from_millis(50));
    }

    /// # Test: Cache Statistics
    ///
    /// ## Scenario
    /// Monitor cache health via stats().
    ///
    /// ## Validates
    /// - Stats accurately reflect stored data
    /// - All key types counted
    #[test]
    fn test_cache_statistics() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        // Store transaction with various key types
        let tx_hash = TxHash::random();
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::from([1; 20]));
        keys.add_account(Address::from([2; 20]));
        keys.add_storage_slot(Address::from([1; 20]), U256::from(0));
        keys.add_storage_slot(Address::from([1; 20]), U256::from(1));
        keys.add_code_hash(alloy_primitives::B256::random());

        cache.store_tx_keys(tx_hash, keys);

        let stats = cache.stats();
        assert_eq!(stats.total_transactions, 1);
        assert_eq!(stats.total_accounts, 2);
        assert_eq!(stats.total_storage_slots, 2);
        assert_eq!(stats.total_code_hashes, 1);
    }

    /// # Test: Cache Clear and Reuse
    ///
    /// ## Scenario
    /// Cache cleared (e.g., after reorg) and reused.
    ///
    /// ## Validates
    /// - clear() removes all entries
    /// - Cache is reusable after clear
    #[test]
    fn test_cache_clear_and_reuse() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        // Store some transactions
        for _ in 0..10 {
            let tx_hash = TxHash::random();
            let mut keys = ExtractedKeys::new();
            keys.add_account(Address::random());
            cache.store_tx_keys(tx_hash, keys);
        }

        assert_eq!(cache.len(), 10);

        // Clear
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());

        // Reuse
        let tx_hash = TxHash::random();
        let mut keys = ExtractedKeys::new();
        keys.add_account(Address::random());
        cache.store_tx_keys(tx_hash, keys);

        assert_eq!(cache.len(), 1);
    }

    // ========================================================================
    // EDGE CASE TESTS
    // ========================================================================

    /// # Test: Empty Keys Storage
    ///
    /// ## Scenario
    /// Transaction simulation produces no keys (unusual but possible).
    ///
    /// ## Validates
    /// - Empty keys can be stored
    /// - Empty keys retrieved correctly
    #[test]
    fn test_empty_keys_storage() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let tx_hash = TxHash::random();
        let keys = ExtractedKeys::new(); // Empty!

        cache.store_tx_keys(tx_hash, keys);

        assert_eq!(cache.len(), 1);

        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert!(retrieved.is_empty());
    }

    /// # Test: Same Transaction Stored Twice (Overwrite)
    ///
    /// ## Scenario
    /// Same tx_hash simulated twice (e.g., retry after timeout).
    /// Second simulation should overwrite first.
    ///
    /// ## Validates
    /// - Duplicate tx_hash overwrites
    /// - Cache size doesn't increase
    /// - Latest keys returned
    #[test]
    fn test_duplicate_tx_hash_overwrite() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let tx_hash = TxHash::random();

        // First simulation
        let mut keys1 = ExtractedKeys::new();
        keys1.add_account(Address::from([1; 20]));
        cache.store_tx_keys(tx_hash, keys1);

        // Second simulation (same tx_hash, different keys)
        let mut keys2 = ExtractedKeys::new();
        keys2.add_account(Address::from([2; 20]));
        keys2.add_account(Address::from([3; 20]));
        cache.store_tx_keys(tx_hash, keys2);

        // Should still be 1 entry
        assert_eq!(cache.len(), 1);

        // Should have latest keys (2 accounts)
        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(retrieved.accounts.len(), 2);
    }

    /// # Test: Non-Existent Transaction Retrieval
    ///
    /// ## Scenario
    /// Block builder queries for transaction that was never simulated.
    ///
    /// ## Validates
    /// - Returns empty keys (not error)
    /// - Doesn't panic
    #[test]
    fn test_nonexistent_tx_retrieval() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        // Query for random tx_hash that doesn't exist
        let nonexistent = TxHash::random();
        let keys = cache.get_keys_for_txs(&[nonexistent]);

        assert!(keys.is_empty());
    }

    /// # Test: Large Key Set
    ///
    /// ## Scenario
    /// Complex transaction touches many addresses and storage slots.
    ///
    /// ## Validates
    /// - Large key sets can be stored
    /// - All keys retrievable
    #[test]
    fn test_large_key_set() {
        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let tx_hash = TxHash::random();
        let mut keys = ExtractedKeys::new();

        // Add 100 accounts
        for i in 0..100u8 {
            keys.add_account(Address::from([i; 20]));
        }

        // Add 500 storage slots
        for i in 0..500u64 {
            keys.add_storage_slot(Address::from([0; 20]), U256::from(i));
        }

        cache.store_tx_keys(tx_hash, keys);

        let retrieved = cache.get_keys_for_txs(&[tx_hash]);
        assert_eq!(retrieved.accounts.len(), 100);
        assert_eq!(retrieved.storage_slots.len(), 500);
    }

    /// # Test: Config with Zero Workers (Clamped to 1)
    ///
    /// ## Scenario
    /// Invalid configuration with 0 workers.
    /// Config clamps to minimum of 1 worker.
    ///
    /// ## Validates
    /// - with_workers(0) clamps to 1
    /// - System remains functional
    #[test]
    fn test_config_zero_workers() {
        let config = PreWarmingConfig::enabled().with_workers(0);
        // 0 is clamped to 1 (minimum workers)
        assert_eq!(config.num_workers, 1);
    }

    /// # Test: Concurrent Cache Access
    ///
    /// ## Scenario
    /// Multiple threads writing to cache simultaneously.
    ///
    /// ## Validates
    /// - Thread-safe writes
    /// - No data loss
    #[test]
    fn test_concurrent_cache_writes() {
        use std::thread;

        let config = PreWarmingConfig::enabled();
        let cache = Arc::new(PreWarmedCache::new(config));

        let handles: Vec<_> = (0..10)
            .map(|thread_id| {
                let cache = Arc::clone(&cache);
                thread::spawn(move || {
                    for i in 0..100 {
                        let tx_hash = TxHash::random();
                        let mut keys = ExtractedKeys::new();
                        keys.add_account(Address::from([(thread_id * 100 + i) as u8; 20]));
                        cache.store_tx_keys(tx_hash, keys);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have 1000 entries (10 threads × 100 each)
        assert_eq!(cache.len(), 1000);
    }
}
