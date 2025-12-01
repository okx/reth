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
    use reth_trie::{StateRootTrieDb, TrieExtDatabase};
    use alloy_primitives::{Address, U256, keccak256, B256};
    use reth_primitives_traits::{Account, StorageEntry};
    use reth_trie::{
        StateRoot as StateRootComputer, StateRootProgress,
    };
    use reth_storage_api::TrieWriter;
    use crate::init::compute_state_root;
    use rand::Rng;

    fn generate_random_accounts_and_storage(
        num_accounts: usize,
        storage_per_account: usize,
        rng: &mut impl Rng,
    ) -> (Vec<(Address, Account)>, Vec<(Address, Vec<StorageEntry>)>) {
        let mut accounts = Vec::new();
        let mut storage_entries = Vec::new();

        for _ in 0..num_accounts {
            let mut address_bytes = [0u8; 20];
            rng.fill(&mut address_bytes);
            let address = Address::from_slice(&address_bytes);

            let account = Account {
                nonce: rng.gen_range(0..=u64::MAX),
                balance: U256::from(rng.gen_range(0u128..=u128::MAX)),
                bytecode_hash:  {
                    let mut hash_bytes = [0u8; 32];
                    rng.fill(&mut hash_bytes);
                    Some(B256::from(hash_bytes))
                }
            };
            accounts.push((address, account));

            let mut storage_vec = Vec::new();
            for _ in 0..storage_per_account {
                let mut storage_key_bytes = [0u8; 32];
                rng.fill(&mut storage_key_bytes);
                let storage_key = B256::from(storage_key_bytes);
                
                let mut storage_value_bytes = [0u8; 32];
                rng.fill(&mut storage_value_bytes);
                let storage_value = U256::from_be_slice(&storage_value_bytes);
                
                storage_vec.push(StorageEntry {
                    key: storage_key,
                    value: storage_value,
                });
            }
            storage_entries.push((address, storage_vec));
        }

        (accounts, storage_entries)
    }

    #[test]
    pub fn test_triedb_state_root() {
        let mut rng = rand::thread_rng();
        let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());

        let mut provider_rw = provider_factory.database_provider_rw().unwrap();

        let (dummy_accounts, storage_entries) = generate_random_accounts_and_storage(
            100, // num_accounts
            5,   // storage_per_account
            &mut rng,
        );

        let accounts_for_hashing = dummy_accounts
            .iter()
            .map(|(address, account)| (*address, Some(*account)));

        provider_rw.insert_account_for_hashing(accounts_for_hashing).unwrap();
        provider_rw.insert_storage_for_hashing(storage_entries).unwrap();
        provider_rw.commit().unwrap();

        let traditional_root = {
            let provider_rw = provider_factory.database_provider_rw().unwrap();
            compute_state_root(&provider_rw, None).unwrap()
        };

        let triedb_root = {
            let provider_ro = provider_factory.database_provider_ro().unwrap();
            let tx = provider_ro.tx_ref();
            let hashed_cursor_factory = DatabaseHashedCursorFactory::new(tx);
            let tmp_dir = TempDir::new("test_triedb").unwrap();
            let file_path = tmp_dir.path().join("test.db");
            let trie_ext_db = TrieExtDatabase::new(file_path);
            let state_root_ext = StateRootTrieDb::new(hashed_cursor_factory, trie_ext_db);
            state_root_ext.calculate_commit().unwrap()
        };

        assert_eq!(triedb_root, traditional_root, "State roots should match");
    }
}
