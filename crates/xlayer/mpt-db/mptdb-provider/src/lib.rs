//! reth `StateProvider` adapter for mpt-db.
//!
//! This crate bridges mptdb-sc (SC layer) and mptdb-ss (SS layer) to reth's
//! `StateProvider` / `StateWriter` / `StateProviderFactory` traits.
//!
//! ## Architecture
//!
//! ```text
//! reth engine
//!   ├─ StateProvider reads  → SS (flat KV, O(1))
//!   ├─ StateRootProvider    → SC (MPT dry-run via apply_hashed_state_overlay)
//!   └─ StateWriter          → SC commit (sync) then SS async write
//! ```

mod factory;
mod provider;
mod writer;

pub use factory::MptDbStateProviderFactory;
pub use provider::MptDbStateProvider;
pub use writer::MptDbStateWriter;

#[cfg(test)]
mod tests;
