//! QMDB-backed state provider for reth.
//!
//! This crate provides a state storage backend using QMDB (Quick Merkle Database)
//! for performance evaluation. QMDB handles account state, storage state, bytecodes,
//! and computes SHA-256 state roots internally.
//!
//! # Architecture
//!
//! - [`QmdbStore`]: Typed wrapper over QMDB's low-level ADS API
//! - [`QmdbStateProvider`]: Implements reth's `StateProvider` trait using `QmdbStore`
//! - [`QmdbStateWriter`]: Writes execution results to QMDB

pub mod provider;
pub mod store;
pub mod writer;

pub use provider::QmdbStateProvider;
pub use store::QmdbStore;
pub use writer::QmdbStateWriter;
