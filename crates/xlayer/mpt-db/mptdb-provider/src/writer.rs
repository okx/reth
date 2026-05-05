//! `StateWriter` for mpt-db.
//!
//! EVM reads are served by reth's PlainState (MDBX) via `StateProviderOverride`.
//! This writer only commits to SC (MPT state root) — SS writes are removed.

use alloy_primitives::{map::AddressMap, Address, B256};
use mptdb_common::error::MptDbError;
use mptdb_sc::mpt::{MptCommitStore, MptCommitter};
use parking_lot::Mutex;
use reth_execution_types::ExecutionOutcome;
use reth_storage_api::{
    errors::provider::{ProviderError, ProviderResult},
    StateWriteConfig, StateWriter, WriteStateInput,
};
use reth_trie_common::HashedPostStateSorted;
use revm_database::{
    states::{PlainStateReverts, StateChangeset},
    OriginalValuesKnown,
};
use std::sync::Arc;

const PREPOP_CHUNK_SIZE: usize = 10_000;

fn map_err(e: MptDbError) -> ProviderError {
    ProviderError::Database(reth_storage_api::errors::db::DatabaseError::Other(e.to_string()))
}

/// `StateWriter` that commits blocks to mpt-db SC only.
///
/// EVM reads are delegated to reth's PlainState (MDBX) via `StateProviderOverride`.
/// SC provides state root computation and proof generation.
pub struct MptDbStateWriter<R> {
    pub sc: Arc<Mutex<MptCommitStore>>,
    _phantom: std::marker::PhantomData<R>,
}

impl<R> MptDbStateWriter<R> {
    /// Pre-populate SC with genesis/initial state from a `BundleState`.
    /// Must be called once before the first `write_state` call.
    pub fn pre_populate(
        &self,
        bundle: &revm_database::BundleState,
        _block_number: u64,
    ) -> ProviderResult<()> {
        let mut ordered_accounts: Vec<(Address, &revm_database::BundleAccount)> =
            bundle.state.iter().map(|(addr, account)| (*addr, account)).collect();
        ordered_accounts.sort_unstable_by_key(|(addr, _)| *addr);

        let mut sc = self.sc.lock();
        for chunk in ordered_accounts.chunks(PREPOP_CHUNK_SIZE) {
            let mut state: AddressMap<revm_database::BundleAccount> = AddressMap::default();
            for (addr, account) in chunk {
                state.insert(*addr, (*account).clone());
            }
            let chunk_bundle = revm_database::BundleState {
                state,
                contracts: Default::default(),
                reverts: Default::default(),
                state_size: 0,
                reverts_size: 0,
            };
            sc.apply_bundle_state(&chunk_bundle).map_err(map_err)?;
            sc.commit().map_err(map_err)?;
        }
        Ok(())
    }
}

impl<R> MptDbStateWriter<R> {
    pub fn new(sc: Arc<Mutex<MptCommitStore>>) -> Self {
        Self { sc, _phantom: std::marker::PhantomData }
    }

    /// Apply execution outcome to SC without committing.
    pub fn apply_execution_outcome<'a>(
        &self,
        execution_outcome: impl Into<WriteStateInput<'a, R>>,
    ) -> ProviderResult<()>
    where
        R: 'a,
    {
        let input: WriteStateInput<'_, R> = execution_outcome.into();
        let bundle = input.state();
        let mut sc = self.sc.lock();
        sc.apply_bundle_state(bundle).map_err(map_err)?;
        Ok(())
    }

    /// Commit previously applied state using an externally computed state root.
    pub fn commit_with_external_root(&self, state_root: B256) -> ProviderResult<(i64, B256)> {
        self.sc.lock().commit_with_external_root(state_root).map_err(map_err)
    }

    /// Apply + commit with external root.
    pub fn write_state_with_external_root<'a>(
        &self,
        execution_outcome: impl Into<WriteStateInput<'a, R>>,
        state_root: B256,
    ) -> ProviderResult<()>
    where
        R: 'a,
    {
        self.apply_execution_outcome(execution_outcome)?;
        self.commit_with_external_root(state_root)?;
        Ok(())
    }
}

impl<R: reth_primitives_traits::Receipt + 'static> StateWriter for MptDbStateWriter<R> {
    type Receipt = R;

    fn write_state<'a>(
        &self,
        execution_outcome: impl Into<WriteStateInput<'a, Self::Receipt>>,
        _is_value_known: OriginalValuesKnown,
        _config: StateWriteConfig,
    ) -> ProviderResult<()> {
        self.apply_execution_outcome(execution_outcome)?;
        self.sc.lock().commit().map_err(map_err)?;
        Ok(())
    }

    fn write_state_reverts(
        &self,
        _reverts: PlainStateReverts,
        _first_block: alloy_primitives::BlockNumber,
        _config: StateWriteConfig,
    ) -> ProviderResult<()> {
        Ok(()) // mpt-db does not maintain changeset history
    }

    fn write_state_changes(&self, _changes: StateChangeset) -> ProviderResult<()> {
        Ok(()) // no-op: all writes go through write_state()
    }

    fn write_hashed_state(&self, _hashed_state: &HashedPostStateSorted) -> ProviderResult<()> {
        Ok(()) // no-op: mptdb-sc manages its own MPT
    }

    fn remove_state_above(&self, block: alloy_primitives::BlockNumber) -> ProviderResult<()> {
        // SC version = block_number + 1; rollback to the version after `block`.
        let target_version = block as i64 + 1;
        self.sc.lock().rollback(target_version).map_err(map_err)
    }

    fn take_state_above(
        &self,
        _block: alloy_primitives::BlockNumber,
    ) -> ProviderResult<ExecutionOutcome<Self::Receipt>> {
        // Called by the execution stage unwind path
        // (stages/stages/src/stages/execution.rs).  SC does not store
        // execution outcomes per block; returning the outcome for replay
        // is not supported.  Use remove_state_above for the rollback side;
        // the execution outcome must be obtained from reth's MDBX change sets.
        Err(ProviderError::Database(reth_storage_api::errors::db::DatabaseError::Other(
            "mpt-db: take_state_above not supported — SC stores no per-block execution \
             outcomes; retrieve from reth MDBX change sets instead"
                .into(),
        )))
    }
}
