//! reth `StateProvider` adapter for mpt-db (Plan C architecture).
//!
//! ## Architecture (Plan C)
//!
//! ```text
//! reth engine
//!   ├─ EVM reads (basic_account/storage)
//!   │    └─ MptDbStateProvider.fallback → reth PlainState (MDBX)
//!   │         via StateProviderOverride default_provider
//!   ├─ StateRootProvider / proof
//!   │    └─ SC (MptCommitStore, always-resident MPT)
//!   └─ StateWriter
//!        └─ SC commit only (apply_bundle_state + commit)
//!             reth writes PlainState to MDBX as part of its own flow
//! ```
//!
//! SS (mptdb-ss) is no longer in the read/write hot path.

mod factory;
pub mod provider;
mod writer;

pub use factory::MptDbStateProviderFactory;
pub use provider::{MptDbStateProvider, ScPrewarmDispatcher, SyncProvider};
pub use writer::MptDbStateWriter;

#[cfg(test)]
mod tests;
