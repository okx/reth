//! Parallel transaction execution framework for reth.
//!
//! Inspired by fafo's architecture (Simulator → Framer → Dispatcher),
//! this crate implements parallel transaction execution while reusing
//! reth's native infrastructure (EVM, StateProvider, BundleState).

pub mod block_context;
pub mod builder;
pub mod crw_sets;
pub mod dashboard;
pub mod dispatch_task;
pub mod dispatcher;
pub mod dispatcher_new;
pub mod execute;
pub mod framer;
pub mod para_bloom;
pub mod parallel_state_cache;
pub mod pipeline;
pub mod result_collector;
pub mod simulator;
pub mod state_cache;
pub mod task;
pub mod tasks_manager;
pub mod tx_database;
