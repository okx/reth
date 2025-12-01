use std::path::Path;
use alloy_primitives::B256;
use alloy_trie::{HashBuilder, EMPTY_ROOT_HASH};
use tracing::{debug, trace};
use reth_execution_errors::StateRootError;
use reth_trie_common::{prefix_set::TriePrefixSets};
use crate::{IntermediateStateRootState, StateRoot, StateRootProgress, StorageRoot};
use crate::hashed_cursor::{HashedCursor, HashedCursorFactory};
use crate::node_iter::{TrieElement, TrieNodeIter};
use crate::stats::TrieTracker;
use crate::trie::StateRootContext;
use crate::trie_cursor::TrieCursorFactory;
use crate::walker::TrieWalker;
use triedb::{Database as TrieDbDatabase, path::{AddressPath, StoragePath}, };
use nybbles::Nibbles;
use triedb::account::Account as TrieDbAccount;
use alloy_consensus::constants::KECCAK_EMPTY;
#[derive(Debug)]
pub struct TrieExtDatabase {
    pub inner: TrieDbDatabase,
}

impl TrieExtDatabase {
    pub fn new(db_path: impl AsRef<Path>) -> Self {
        let db_path = db_path.as_ref();
        let db = TrieDbDatabase::create_new(db_path).unwrap();
        Self {
            inner: db,
        }
    }
}

/// `StateRoot` is used to compute the root node of a state trie.
#[derive(Debug)]
pub struct StateRootTrieDb<H> {
    /// The factory for hashed cursors.
    pub hashed_cursor_factory: H,
    pub db: TrieExtDatabase
}

impl<H> StateRootTrieDb<H> {
    /// Creates [`StateRootTrieDb`] with
    pub fn new(hashed_cursor_factory: H, db: TrieExtDatabase) -> Self {
        Self {
            hashed_cursor_factory,
            db
        }
    }
}
impl<H> StateRootTrieDb<H>
where
    H: HashedCursorFactory + Clone,
{
    pub fn calculate_commit(self) -> Result<B256, StateRootError> {
        trace!(target: "trie::state_root", "calculating state root");

        let mut acct_cursor = self.hashed_cursor_factory.hashed_account_cursor()?;

        let mut tx = self.db.inner.begin_rw().unwrap();

        // Start from the beginning by seeking to the first account (B256::ZERO)
        let mut account_entry = acct_cursor.next().unwrap();
        while let Some((hashed_address, account)) = account_entry {

            let nibbles = Nibbles::unpack(hashed_address);
            let address_path = AddressPath::new(nibbles);

            // Get storage cursor for this account first
            let mut storage_cursor = self.hashed_cursor_factory.hashed_storage_cursor(hashed_address)?;

            // Iterate through all storage entries for this account to compute storage root
            // For now, we'll use EMPTY_ROOT_HASH if no storage entries exist
            // TODO: Compute actual storage root from storage entries
            let mut storage_entry = storage_cursor.seek(B256::ZERO)?;
            let storage_root = if storage_entry.is_some() {
                // If there are storage entries, we need to compute the storage root
                // For now, use EMPTY_ROOT_HASH as placeholder
                // In a full implementation, you'd build the storage trie and get its root
                EMPTY_ROOT_HASH
            } else {
                EMPTY_ROOT_HASH
            };

            // Convert reth_primitives_traits::Account to triedb::account::Account
            let triedb_account = TrieDbAccount {
                nonce: account.nonce,
                balance: account.balance,
                code_hash: account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
                storage_root,
            };

            tx.set_account(address_path.clone(), Some(triedb_account)).unwrap();

            // Now set storage slots in TrieDB
            while let Some((hashed_storage_key, storage_value)) = storage_entry {
                let storage_path = StoragePath::for_address_path_and_slot_hash(address_path.clone(), Nibbles::unpack(hashed_storage_key));
                tx.set_storage_slot(storage_path, Some(storage_value)).unwrap();

                storage_entry = storage_cursor.next()?;
            }

            account_entry = acct_cursor.next()?;
        }

        tx.commit().unwrap();
        Ok(self.db.inner.state_root())
        // Ok(EMPTY_ROOT_HASH)
    }
}

#[cfg(test)]
mod tests {
    use tempdir::TempDir;
    use super::{TrieExtDatabase};
    use crate::hashed_cursor::{HashedCursor, HashedCursorFactory};
    use reth_provider::{
        test_utils::{create_test_provider_factory_with_chain_spec, MockNodeTypesWithDB},
        ProviderFactory, HashingWriter, DBProvider
    };
    use reth_chainspec::{Chain, ChainSpec, HOLESKY, MAINNET, SEPOLIA};
    use reth_provider::DatabaseProviderFactory;
    use reth_trie_db::DatabaseHashedCursorFactory;
    use alloy_primitives::{Address, U256, keccak256, B256};
    use reth_primitives_traits::Account;

    #[test]
    pub fn test_triedb() {
        let tmp_dir = TempDir::new("test_triedb").unwrap();
        let file_path = tmp_dir.path().join("test.db");
        let trie_db = TrieExtDatabase::new(file_path);

        let provider_factory = create_test_provider_factory_with_chain_spec(MAINNET.clone());

        let mut provider_rw = provider_factory.database_provider_rw().unwrap();

        // Generate dummy accounts
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

        // Insert accounts into the database
        let accounts_for_hashing = dummy_accounts
            .iter()
            .map(|(address, account)| (*address, Some(*account)));
        
        provider_rw.insert_account_for_hashing(accounts_for_hashing).unwrap();
        
        // Commit the transaction (this consumes provider_rw)
        provider_rw.commit().unwrap();

        // Get a new provider to read the committed data
        let provider_rw = provider_factory.database_provider_rw().unwrap();
        let tx = provider_rw.tx_ref();
        let hashed_cursor_factory = DatabaseHashedCursorFactory::new(tx);
        println!("hashed cursor factory: {:?}", hashed_cursor_factory);
        let mut account_cursor = hashed_cursor_factory.hashed_account_cursor().unwrap();
        //
        // // Start from the beginning (seek to B256::ZERO to get the first account)
        // let mut account_entry = account_cursor.seek(B256::ZERO).unwrap();
        //
        // let mut iterated_accounts = Vec::new();
        //
        // // Iterate through all accounts
        // while let Some((hashed_address, account)) = account_entry {
        //     iterated_accounts.push((hashed_address, account));
        //
        //     // Move to next account
        //     account_entry = account_cursor.next().unwrap();
        // }
        //
        // // Verify we got all the accounts we inserted
        // assert_eq!(iterated_accounts.len(), dummy_accounts.len());
        //
        // // Verify the accounts match (by checking hashed addresses)
        // let inserted_hashed_addresses: Vec<B256> = dummy_accounts
        //     .iter()
        //     .map(|(address, _)| keccak256(address))
        //     .collect();
        //
        // let iterated_hashed_addresses: Vec<B256> = iterated_accounts
        //     .iter()
        //     .map(|(hashed_address, _)| *hashed_address)
        //     .collect();
        //
        // // Sort both for comparison
        // let mut inserted_sorted = inserted_hashed_addresses.clone();
        // inserted_sorted.sort();
        // let mut iterated_sorted = iterated_hashed_addresses.clone();
        // iterated_sorted.sort();
        //
        // assert_eq!(inserted_sorted, iterated_sorted);
        //
        // // Verify account data matches
        // for (hashed_address, account) in &iterated_accounts {
        //     let original_account = dummy_accounts
        //         .iter()
        //         .find(|(addr, _)| keccak256(addr) == *hashed_address)
        //         .unwrap();
        //
        //     assert_eq!(account.nonce, original_account.1.nonce);
        //     assert_eq!(account.balance, original_account.1.balance);
        //     assert_eq!(account.bytecode_hash, original_account.1.bytecode_hash);
        // }
    }
}