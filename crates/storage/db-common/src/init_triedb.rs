use reth_provider::{
    DBProvider, ProviderError, TrieWriter,
};
use reth_trie::{
    prefix_set::TriePrefixSets,
    IntermediateStateRootState, StateRoot as StateRootComputer, StateRootProgress,
};
use reth_trie_db::DatabaseHashedCursorFactory;
use reth_trie::{StateRootTrieDb, TrieExtDatabase};
use alloy_primitives::B256;
use tracing::{info, trace};
use std::path::Path;

/// Calculate state root using TrieDB and commit trie updates.
///
/// This function:
/// 1. Uses `StateRootTrieDb` with `DatabaseHashedCursorFactory` to read from the database
/// 2. Calculates state root using TrieDB
/// 3. Returns the computed state root
///
/// # Arguments
///
/// * `provider` - Database provider that implements `DBProvider` and `TrieWriter`
/// * `trie_db_path` - Path where the TrieDB database should be created
/// * `prefix_sets` - Optional prefix sets for incremental state root calculation (currently unused)
///
/// # Returns
///
/// * `Ok(B256)` - The computed state root hash
/// * `Err(ProviderError)` - If state root calculation fails
pub fn calculate_state_root_with_triedb<Provider>(
    provider: &Provider,
    trie_db_path: impl AsRef<Path>,
    _prefix_sets: Option<TriePrefixSets>,
) -> Result<B256, ProviderError>
where
    Provider: DBProvider<Tx: reth_db_api::transaction::DbTxMut> + TrieWriter,
{
    trace!(target: "reth::state_root", "Calculating state root using TrieDB");
    let tx = provider.tx_ref();
    let hashed_cursor_factory = DatabaseHashedCursorFactory::new(tx);
    let trie_ext_db = TrieExtDatabase::new(trie_db_path);
    let state_root_ext = StateRootTrieDb::new(hashed_cursor_factory, trie_ext_db);
    let ret = state_root_ext.calculate_commit();
    match ret {
        Ok(root) => Ok(root),
        Err(error) => Err(ProviderError::TrieWitnessError("".to_string())),
    }
}
