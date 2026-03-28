//! `StateProviderFactory` for mpt-db.

use crate::provider::MptDbStateProvider;
use alloy_eips::{BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{BlockHash, BlockNumber, B256};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
use parking_lot::Mutex;
use reth_chainspec::ChainInfo;
use reth_storage_api::{
    errors::provider::{ProviderError, ProviderResult},
    BlockHashReader, BlockIdReader, BlockNumReader, StateProvider, StateProviderBox,
    StateProviderFactory,
};
use std::sync::Arc;

pub struct MptDbStateProviderFactory {
    pub sc: Arc<Mutex<MptCommitStore>>,
    /// Fallback provider for EVM reads and non-state data (bytecode, block hashes).
    /// In production this is unused at factory level (reads come via StateProviderOverride).
    /// In benchmark contexts, this is a MDBX-backed provider or bytecode-only stub.
    pub fallback: Arc<dyn StateProvider + Send + Sync>,
    pub block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    /// Optional factory for version-specific historical providers.
    pub historical_fallback_factory: Option<Arc<dyn StateProviderFactory + Send + Sync>>,
}

impl MptDbStateProviderFactory {
    pub fn new(
        sc: Arc<Mutex<MptCommitStore>>,
        fallback: Arc<dyn StateProvider + Send + Sync>,
        block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    ) -> Self {
        Self { sc, fallback, block_id_reader, historical_fallback_factory: None }
    }

    /// Configure a MDBX-backed factory for version-specific historical providers.
    pub fn with_historical_fallback(
        mut self,
        factory: Arc<dyn StateProviderFactory + Send + Sync>,
    ) -> Self {
        self.historical_fallback_factory = Some(factory);
        self
    }

    /// Create a `MptDbStateProvider` at `version`.
    ///
    /// For `latest()` (version == latest SC version): uses `self.fallback`
    /// directly (the `StateProviderOverride` default_provider in production).
    ///
    /// For historical versions (version < latest SC version):
    /// - If `historical_fallback_factory` is set: calls `history_by_block_number` to obtain a
    ///   version-specific fallback.  Errors are propagated.
    /// - If `historical_fallback_factory` is NOT set: returns an error rather than silently using
    ///   `self.fallback` (current-state data), which would produce wrong reads for historical
    ///   queries without any indication.
    fn make_provider(&self, version: i64) -> ProviderResult<MptDbStateProvider> {
        use crate::provider::SyncProvider;

        let latest = self.sc.lock().version().max(0);
        let fallback: Arc<dyn StateProvider + Send + Sync> = if version == latest {
            // Latest version: self.fallback is correct (current-state provider).
            Arc::clone(&self.fallback)
        } else if let Some(factory) = &self.historical_fallback_factory {
            let block = (version - 1).max(0) as u64;
            SyncProvider::new(factory.history_by_block_number(block)?)
        } else {
            // No historical factory configured: cannot provide version-specific
            // reads.  Return an error instead of silently using self.fallback
            // (current-state), which would return wrong data for historical queries.
            return Err(ProviderError::UnsupportedProvider);
        };

        Ok(MptDbStateProvider::new(
            Arc::clone(&self.sc),
            version,
            fallback,
            Arc::clone(&self.block_id_reader),
        ))
    }

    fn latest_version(&self) -> i64 {
        // SC version = block_number + 1 (version 0 = fresh DB, no blocks).
        self.sc.lock().version().max(0)
    }
}

impl BlockNumReader for MptDbStateProviderFactory {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        self.block_id_reader.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        // latest_version() = SC version = block_number + 1.
        Ok(self.latest_version().saturating_sub(1).max(0) as u64)
    }

    fn last_block_number(&self) -> ProviderResult<BlockNumber> {
        self.block_id_reader.last_block_number()
    }

    fn block_number(&self, hash: B256) -> ProviderResult<Option<BlockNumber>> {
        self.block_id_reader.block_number(hash)
    }
}

impl BlockHashReader for MptDbStateProviderFactory {
    fn block_hash(&self, number: BlockNumber) -> ProviderResult<Option<B256>> {
        self.fallback.block_hash(number)
    }

    fn canonical_hashes_range(
        &self,
        start: BlockNumber,
        end: BlockNumber,
    ) -> ProviderResult<Vec<B256>> {
        self.fallback.canonical_hashes_range(start, end)
    }
}

impl BlockIdReader for MptDbStateProviderFactory {
    fn pending_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.block_id_reader.pending_block_num_hash()
    }

    fn safe_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.block_id_reader.safe_block_num_hash()
    }

    fn finalized_block_num_hash(&self) -> ProviderResult<Option<BlockNumHash>> {
        self.block_id_reader.finalized_block_num_hash()
    }
}

impl StateProviderFactory for MptDbStateProviderFactory {
    fn latest(&self) -> ProviderResult<StateProviderBox> {
        // Build latest provider in one shot to avoid TOCTOU races:
        // if SC version advances between `latest_version()` and `make_provider`,
        // `make_provider` can misclassify "latest" as historical and return
        // UnsupportedProvider when no historical factory is configured.
        let version = self.latest_version();
        Ok(Box::new(MptDbStateProvider::new(
            Arc::clone(&self.sc),
            version,
            Arc::clone(&self.fallback),
            Arc::clone(&self.block_id_reader),
        )))
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Latest => self.latest(),
            BlockNumberOrTag::Safe => {
                let nh = self
                    .block_id_reader
                    .safe_block_num_hash()?
                    .ok_or(ProviderError::SafeBlockNotFound)?;
                self.history_by_block_number(nh.number)
            }
            BlockNumberOrTag::Finalized => {
                let nh = self
                    .block_id_reader
                    .finalized_block_num_hash()?
                    .ok_or(ProviderError::FinalizedBlockNotFound)?;
                self.history_by_block_number(nh.number)
            }
            BlockNumberOrTag::Pending => self.pending(),
            BlockNumberOrTag::Number(n) => Ok(Box::new(self.make_provider(n as i64 + 1)?)),
            BlockNumberOrTag::Earliest => {
                // Version 1 = block 0 (genesis).  If SC was initialised from
                // a non-genesis checkpoint or has pruned early versions, this
                // may not correspond to the true earliest available state.
                // TODO: expose SC earliest_version to derive a correct mapping.
                Ok(Box::new(self.make_provider(1)?))
            }
        }
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        Ok(Box::new(self.make_provider(block as i64 + 1)?))
    }

    fn state_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        let number = self
            .block_id_reader
            .block_number(block)?
            .ok_or_else(|| ProviderError::BlockHashNotFound(block))?;
        Ok(Box::new(self.make_provider(number as i64 + 1)?))
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        self.state_by_block_hash(block)
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        self.latest()
    }

    fn pending_state_by_hash(&self, block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        match self.block_id_reader.block_number(block_hash)? {
            Some(n) => Ok(Some(Box::new(self.make_provider(n as i64 + 1)?))),
            None => Ok(None),
        }
    }

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        Ok(None)
    }
}
