//! Parallel transaction execution framework for reth.
//!
//! Inspired by fafo's architecture (Simulator → Framer → Dispatcher),
//! this crate implements parallel transaction execution while reusing
//! reth's native infrastructure (EVM, StateProvider, BundleState).

pub mod crw_sets;
pub mod dispatcher;
pub mod framer;
pub mod para_bloom;
pub mod simulator;
pub mod state_cache;
pub mod task;
