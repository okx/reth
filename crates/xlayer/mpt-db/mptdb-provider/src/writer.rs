//! `StateWriter` for mpt-db.

use mptdb_common::error::MptDbError;
use mptdb_sc::mpt::{ss_changeset::bundle_to_ss_changeset, MptCommitStore, MptCommitter};
use mptdb_ss::evm::store::EVMStateStore;
use mptdb_traits::ss::StateStore as _;
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

fn map_err(e: MptDbError) -> ProviderError {
    ProviderError::Database(reth_storage_api::errors::db::DatabaseError::Other(e.to_string()))
}

/// `StateWriter` that commits blocks to mpt-db (SC + SS).
pub struct MptDbStateWriter<R> {
    pub ss: Arc<EVMStateStore>,
    pub sc: Arc<Mutex<MptCommitStore>>,
    _phantom: std::marker::PhantomData<R>,
}

impl<R> MptDbStateWriter<R> {
    pub fn new(ss: Arc<EVMStateStore>, sc: Arc<Mutex<MptCommitStore>>) -> Self {
        Self { ss, sc, _phantom: std::marker::PhantomData }
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
        let input: WriteStateInput<'_, R> = execution_outcome.into();
        let bundle = input.state();
        let first_block = input.first_block();

        // 1. SC: apply_bundle_state + commit (synchronous)
        {
            let mut sc = self.sc.lock();
            sc.apply_bundle_state(bundle).map_err(map_err)?;
            sc.commit().map_err(map_err)?;
        }

        // 2. SS: write changeset (WAL sync on calling thread, RocksDB async)
        // SS version = first_block + 1 to match SC's version semantics.
        // MVCC skips version 0 (maps to 1), so we use block_number + 1 to
        // guarantee each block occupies a distinct SS version.
        let ss_version = first_block as i64 + 1;
        let ss_changeset = bundle_to_ss_changeset(bundle);
        self.ss.apply_changeset_async(ss_version, &ss_changeset).map_err(map_err)?;

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

    fn remove_state_above(&self, _block: alloy_primitives::BlockNumber) -> ProviderResult<()> {
        Err(ProviderError::Database(reth_storage_api::errors::db::DatabaseError::Other(
            "mpt-db: remove_state_above (reorg) not yet implemented".into(),
        )))
    }

    fn take_state_above(
        &self,
        _block: alloy_primitives::BlockNumber,
    ) -> ProviderResult<ExecutionOutcome<Self::Receipt>> {
        Err(ProviderError::Database(reth_storage_api::errors::db::DatabaseError::Other(
            "mpt-db: take_state_above (reorg) not yet implemented".into(),
        )))
    }
}
