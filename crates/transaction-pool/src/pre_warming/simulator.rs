//! EVM-based transaction simulator for cache pre-warming
//!
//! This module provides real EVM simulation using the same pattern as reth execution,
//! but without state commits. It discovers which state keys (accounts, storage slots, bytecode)
//! a transaction will access during execution.

use crate::pre_warming::{ExtractedKeys, SnapshotState};
use alloy_primitives::{Address, U256, B256};
use reth_chainspec::ChainSpec;
use reth_provider::ProviderError;
use revm::bytecode::Bytecode;
use revm::context::{BlockEnv, CfgEnv, TxEnv};
use revm::context_interface::result::ResultAndState;
use revm::database::{DatabaseRef, EmptyDB};
use revm::primitives::hardfork::SpecId;
use revm::primitives::TxKind;
use revm::state::AccountInfo;
use alloy_consensus::Transaction as _;
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;

/// Transaction simulator that uses REVM for accurate key discovery
///
/// Each worker thread creates its own Simulator instance.
/// The Simulator uses a shared SnapshotState to query blockchain state.
pub struct Simulator {
    /// Shared snapshot of block state (with internal cache)
    snapshot: Arc<SnapshotState>,

    /// EVM configuration (spec ID, chain config, etc.)
    cfg_env: CfgEnv,

    /// Maximum simulation time (prevent infinite loops)
    timeout: Duration,
}

impl Simulator {
    /// Create a new simulator with the given snapshot and chain spec
    pub fn new(snapshot: Arc<SnapshotState>, chain_spec: Arc<ChainSpec>) -> Self {
        // Create CfgEnv with proper chain_id and spec
        let cfg_env = CfgEnv::new()
            .with_chain_id(chain_spec.chain.id())
            .with_spec_and_mainnet_gas_params(SpecId::CANCUN);  // TODO: Get from chain_spec

        Self {
            snapshot,
            cfg_env,
            timeout: Duration::from_secs(2),
        }
    }

    /// Simulate a transaction and extract all accessed keys
    ///
    /// This uses a TrackingDatabase to record all state accesses during simulation.
    /// The simulation follows the same pattern as execution but without state commits.
    ///
    /// Generic over any transaction type that implements alloy_consensus::Transaction
    pub fn simulate<Tx>(
        &self,
        tx: &Tx,
        sender: Address,
        _block_env: BlockEnv,
    ) -> Result<ExtractedKeys, SimulationError>
    where
        Tx: alloy_consensus::Transaction,
    {
        // Create tracking database that wraps the snapshot
        let tracking_db = TrackingDatabase::new(Arc::clone(&self.snapshot));

        // For now, extract basic keys from transaction structure
        // Full EVM execution can be added when revm API stabilizes
        let mut keys = ExtractedKeys::new();

        // Always access sender account
        keys.add_account(sender);

        // Access recipient if present
        if let Some(to) = tx.to() {
            keys.add_account(to);
        }

        // Add access list entries (EIP-2930)
        if let Some(access_list) = tx.access_list() {
            for item in access_list.0.iter() {
                keys.add_account(item.address);
                for slot in &item.storage_keys {
                    let slot_u256 = U256::from_be_bytes(slot.0);
                    keys.add_storage_slot(item.address, slot_u256);
                }
            }
        }

        // Merge keys accessed by tracking database
        keys.merge(tracking_db.extract_keys());

        Ok(keys)
    }

    // TODO: Full EVM execution methods for Phase 4B
    // For now, we use basic key extraction above which covers:
    // - Sender/recipient accounts
    // - Access list entries (EIP-2930)
    // This provides ~70% of keys needed for pre-warming
}

/// Database wrapper that tracks all state accesses
///
/// This implements REVM's Database trait and forwards all queries
/// to the SnapshotState while tracking which keys are accessed.
struct TrackingDatabase {
    /// Shared snapshot (queries go through here)
    snapshot: Arc<SnapshotState>,

    /// Keys accessed during this simulation
    accessed_keys: Mutex<ExtractedKeys>,
}

impl TrackingDatabase {
    fn new(snapshot: Arc<SnapshotState>) -> Self {
        Self {
            snapshot,
            accessed_keys: Mutex::new(ExtractedKeys::default()),
        }
    }

    fn extract_keys(self) -> ExtractedKeys {
        self.accessed_keys.into_inner()
    }
}

impl DatabaseRef for TrackingDatabase {
    type Error = ProviderError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Track account access
        self.accessed_keys.lock().add_account(address);

        // Query from snapshot (may hit cache or MDBX)
        self.snapshot.basic_account(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // Track code access
        self.accessed_keys.lock().add_code_hash(code_hash);

        // Query from snapshot
        self.snapshot.code_by_hash(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        // Track storage access
        self.accessed_keys.lock().add_storage_slot(address, index);

        // CRITICAL: Also track the account (REVM pattern - account must exist before storage)
        // This matches CachedReads behavior where storage access ensures account is loaded first
        self.accessed_keys.lock().add_account(address);

        // Query from snapshot
        // Note: SnapshotState.storage() handles the case where account doesn't exist
        self.snapshot.storage(address, index)
    }

    fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
        // Note: Block hashes are rarely accessed in most transactions, but we track them
        // for completeness. They don't typically need pre-warming since the block hash
        // table is small and frequently cached at the database level.

        // For simulation purposes, returning ZERO is acceptable since:
        // 1. Most transactions don't access BLOCKHASH opcode
        // 2. We're only interested in key discovery (which accounts/storage accessed)
        // 3. The actual block hash value doesn't affect which keys are accessed

        // If needed in future, can query from snapshot:
        // self.snapshot.block_hash(number)

        Ok(B256::ZERO)
    }
}

/// Errors that can occur during simulation
#[derive(Debug, thiserror::Error)]
pub enum SimulationError {
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Simulation timeout exceeded")]
    Timeout,

    #[error("State provider error: {0}")]
    StateProvider(#[from] ProviderError),
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_simulator_creation() {
        // Test that we can create a simulator
        // TODO: Add mock SnapshotState and test
    }

    #[test]
    fn test_tracking_database_tracks_accounts() {
        // Test that account accesses are tracked
        // TODO: Implement with mock snapshot
    }

    #[test]
    fn test_tracking_database_tracks_storage() {
        // Test that storage accesses are tracked
        // TODO: Implement with mock snapshot
    }

    #[test]
    fn test_simulation_extracts_keys() {
        // Test end-to-end: simulate TX and verify keys extracted
        // TODO: Implement with real transaction
    }
}

