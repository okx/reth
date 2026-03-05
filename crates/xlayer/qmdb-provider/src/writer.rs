//! QMDB-backed state writer.
//!
//! Writes block execution results (BundleState) to QMDB.
//! Reverts, hashed state, and changeset operations are no-ops since
//! QMDB handles history via height and doesn't use MPT hashed keys.

use crate::store::QmdbStore;
use alloy_primitives::BlockNumber;
use reth_execution_types::ExecutionOutcome;
use reth_storage_api::{StateWriteConfig, StateWriter, WriteStateInput};
use reth_storage_errors::provider::ProviderResult;
use reth_trie_common::HashedPostStateSorted;
use revm_database::{
    states::{PlainStateReverts, StateChangeset},
    OriginalValuesKnown,
};
use std::sync::Arc;

/// A `StateWriter` that commits execution results to QMDB.
pub struct QmdbStateWriter<R> {
    store: Arc<QmdbStore>,
    _phantom: std::marker::PhantomData<R>,
}

impl<R> std::fmt::Debug for QmdbStateWriter<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QmdbStateWriter").field("store", &self.store).finish_non_exhaustive()
    }
}

impl<R> QmdbStateWriter<R> {
    /// Create a new `QmdbStateWriter` wrapping the given store.
    pub fn new(store: Arc<QmdbStore>) -> Self {
        Self { store, _phantom: std::marker::PhantomData }
    }
}

impl<R: Send + Sync + 'static> StateWriter for QmdbStateWriter<R> {
    type Receipt = R;

    fn write_state<'a>(
        &self,
        execution_outcome: impl Into<WriteStateInput<'a, Self::Receipt>>,
        _is_value_known: OriginalValuesKnown,
        _config: StateWriteConfig,
    ) -> ProviderResult<()> {
        let input: WriteStateInput<'a, R> = execution_outcome.into();
        self.store.commit_bundle(input.state());
        Ok(())
    }

    fn write_state_reverts(
        &self,
        _reverts: PlainStateReverts,
        _first_block: BlockNumber,
        _config: StateWriteConfig,
    ) -> ProviderResult<()> {
        // QMDB has built-in history via height — no explicit revert storage needed.
        Ok(())
    }

    fn write_state_changes(&self, changes: StateChangeset) -> ProviderResult<()> {
        // StateChangeset contains individual account/storage changes.
        // For benchmark purposes, we rely on write_state() with the BundleState.
        let _ = changes;
        Ok(())
    }

    fn write_hashed_state(&self, _hashed_state: &HashedPostStateSorted) -> ProviderResult<()> {
        // QMDB doesn't use hashed keys — no-op.
        Ok(())
    }

    fn remove_state_above(&self, _block: BlockNumber) -> ProviderResult<()> {
        // Not needed for benchmark — QMDB has no built-in block pruning API.
        Ok(())
    }

    fn take_state_above(
        &self,
        _block: BlockNumber,
    ) -> ProviderResult<ExecutionOutcome<Self::Receipt>> {
        // Not supported — return empty outcome.
        Ok(ExecutionOutcome::default())
    }
}
