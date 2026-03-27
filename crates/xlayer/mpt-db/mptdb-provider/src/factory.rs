//! `StateProviderFactory` for mpt-db.

use crate::provider::MptDbStateProvider;
use alloy_eips::{BlockNumHash, BlockNumberOrTag};
use alloy_primitives::{BlockHash, BlockNumber, B256};
use mptdb_sc::mpt::{MptCommitStore, MptCommitter as _};
use mptdb_ss::evm::store::EVMStateStore;
use parking_lot::Mutex;
use reth_chainspec::ChainInfo;
use reth_storage_api::{
    errors::provider::{ProviderError, ProviderResult},
    BlockHashReader, BlockIdReader, BlockNumReader, StateProvider, StateProviderBox,
    StateProviderFactory,
};
use std::sync::Arc;

pub struct MptDbStateProviderFactory {
    pub ss: Arc<EVMStateStore>,
    pub sc: Arc<Mutex<MptCommitStore>>,
    pub fallback: Arc<dyn StateProvider + Send + Sync>,
    pub block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
}

impl MptDbStateProviderFactory {
    pub fn new(
        ss: Arc<EVMStateStore>,
        sc: Arc<Mutex<MptCommitStore>>,
        fallback: Arc<dyn StateProvider + Send + Sync>,
        block_id_reader: Arc<dyn BlockIdReader + Send + Sync>,
    ) -> Self {
        Self { ss, sc, fallback, block_id_reader }
    }

    fn make_provider(&self, version: i64) -> MptDbStateProvider {
        MptDbStateProvider::new(
            Arc::clone(&self.ss),
            Arc::clone(&self.sc),
            version,
            Arc::clone(&self.fallback),
            Arc::clone(&self.block_id_reader),
        )
    }

    fn latest_version(&self) -> i64 {
        // SC version = block_number + 1; SS version = block_number + 1.
        // Both SC and SS use the same version number for the same block.
        // SC version 0 = fresh DB (no blocks); SC version 1 = block 0.
        // SS version for block N = N + 1 (MVCC skips version 0).
        // So latest SS version = SC version (they are identical).
        self.sc.lock().version().max(0)
    }
}

impl BlockNumReader for MptDbStateProviderFactory {
    fn chain_info(&self) -> ProviderResult<ChainInfo> {
        self.block_id_reader.chain_info()
    }

    fn best_block_number(&self) -> ProviderResult<BlockNumber> {
        Ok(self.latest_version().max(0) as u64)
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
        Ok(Box::new(self.make_provider(self.latest_version())))
    }

    fn state_by_block_number_or_tag(
        &self,
        number_or_tag: BlockNumberOrTag,
    ) -> ProviderResult<StateProviderBox> {
        match number_or_tag {
            BlockNumberOrTag::Latest | BlockNumberOrTag::Safe | BlockNumberOrTag::Finalized => {
                self.latest()
            }
            BlockNumberOrTag::Pending => self.pending(),
            BlockNumberOrTag::Number(n) => Ok(Box::new(self.make_provider(n as i64))),
            BlockNumberOrTag::Earliest => Ok(Box::new(self.make_provider(0))),
        }
    }

    fn history_by_block_number(&self, block: BlockNumber) -> ProviderResult<StateProviderBox> {
        // SS version = block_number + 1 (see §5.4 of plan).
        Ok(Box::new(self.make_provider(block as i64 + 1)))
    }

    fn state_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        let number = self
            .block_id_reader
            .block_number(block)?
            .ok_or_else(|| ProviderError::BlockHashNotFound(block))?;
        Ok(Box::new(self.make_provider(number as i64)))
    }

    fn history_by_block_hash(&self, block: BlockHash) -> ProviderResult<StateProviderBox> {
        self.state_by_block_hash(block)
    }

    fn pending(&self) -> ProviderResult<StateProviderBox> {
        self.latest()
    }

    fn pending_state_by_hash(&self, block_hash: B256) -> ProviderResult<Option<StateProviderBox>> {
        match self.block_id_reader.block_number(block_hash)? {
            Some(n) => Ok(Some(Box::new(self.make_provider(n as i64)))),
            None => Ok(None),
        }
    }

    fn maybe_pending(&self) -> ProviderResult<Option<StateProviderBox>> {
        Ok(None)
    }
}
