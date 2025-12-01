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

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;
    use reth_provider::{
        test_utils::{create_test_provider_factory_with_chain_spec, MockNodeTypesWithDB},
        ProviderFactory, HashingWriter, DBProvider,
    };
    use reth_chainspec::MAINNET;
    use reth_provider::DatabaseProviderFactory;
    use reth_trie_db::DatabaseHashedCursorFactory;
    use reth_trie::{hashed_cursor::{HashedCursorFactory, HashedCursor}, StateRootTrieDb, TrieExtDatabase};
    use alloy_primitives::{Address, U256, keccak256, B256};
    use reth_primitives_traits::{Account, StorageEntry};
    use reth_trie::{
        prefix_set::{TriePrefixSets, TriePrefixSetsMut},
        IntermediateStateRootState, Nibbles, StateRoot as StateRootComputer, StateRootProgress,
    };
    use reth_storage_api::TrieWriter;
    use reth_trie_db::{DatabaseStateRoot};
    use crate::init::compute_state_root;

    #[test]
    pub fn test_triedb_state_root() {
        let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());

        let mut provider_rw = provider_factory.database_provider_rw().unwrap();

        let dummy_accounts: Vec<(Address, Account)> = vec![
            (
                Address::with_last_byte(1),
                Account {
                    nonce: 10,
                    balance: U256::from(1000),
                    bytecode_hash: None,
                },
            ),
            (
                Address::with_last_byte(2),
                Account {
                    nonce: 20,
                    balance: U256::from(2000),
                    bytecode_hash: None,
                },
            ),
            (
                Address::with_last_byte(3),
                Account {
                    nonce: 30,
                    balance: U256::from(3000),
                    bytecode_hash: None,
                },
            ),
        ];

        let accounts_for_hashing = dummy_accounts
            .iter()
            .map(|(address, account)| (*address, Some(*account)));

        provider_rw.insert_account_for_hashing(accounts_for_hashing).unwrap();

        // Generate two random storage entries for each account
        let storage_entries: Vec<(Address, Vec<StorageEntry>)> = dummy_accounts
            .iter()
            .map(|(address, _)| {
                // Generate two random storage entries per account
                // Using deterministic but varied keys based on address and index
                let mut storage_vec = Vec::new();
                for i in 0..2 {
                    // Create a deterministic but unique storage key for each account and slot
                    let mut key_bytes = [0u8; 32];
                    key_bytes[0..20].copy_from_slice(address.as_slice());
                    key_bytes[20] = i as u8;
                    key_bytes[21] = 0xFF;
                    let storage_key = B256::from(key_bytes);
                    
                    // Generate a random value (using address and index for determinism)
                    let hash = keccak256([address.as_slice(), &[i as u8]].concat());
                    let storage_value = U256::from_be_slice(hash.as_slice());
                    
                    storage_vec.push(StorageEntry {
                        key: storage_key,
                        value: storage_value,
                    });
                }
                (*address, storage_vec)
            })
            .collect();

        // Insert storage entries for hashing
        provider_rw.insert_storage_for_hashing(storage_entries).unwrap();

        provider_rw.commit().unwrap();

        let origin_rot = {
            let provider_rw = provider_factory.database_provider_rw().unwrap();
            let root = compute_state_root(&provider_rw, None).unwrap();
            root
        };

        let trie_db_ext_root = {
            let provider_ro = provider_factory.database_provider_ro().unwrap();
            let tx = provider_ro.tx_ref();
            let hashed_cursor_factory = DatabaseHashedCursorFactory::new(tx);
            let tmp_dir = TempDir::new("test_triedb").unwrap();
            let file_path = tmp_dir.path().join("test.db");
            let trie_ext_db = TrieExtDatabase::new(file_path);
            let state_root_ext = StateRootTrieDb::new(hashed_cursor_factory, trie_ext_db);
            let root = state_root_ext.calculate_commit().unwrap();
            root
        };

        assert_eq!(trie_db_ext_root, origin_rot);
    }
}
