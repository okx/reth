//! Parallel-safe state cache for concurrent EVM execution.
//!
//! Provides a three-layer read path:
//! 1. ParallelStateCache (DashMap, current block's hot data)
//! 2. reth CanonicalInMemoryState (cross-block cache)
//! 3. QMDB + MDBX (persistent storage)
