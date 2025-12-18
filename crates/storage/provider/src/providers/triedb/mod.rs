use std::{path::Path, sync::Arc};
use alloy_primitives::{Address, B256, U256};
use alloy_trie::EMPTY_ROOT_HASH;
use alloy_consensus::constants::KECCAK_EMPTY;
use reth_primitives_traits::Account;
use triedb::{Database as TrieDbDatabase, path::{AddressPath, StoragePath},    account::Account as TrieDBAccount,
             transaction::TransactionError, Database};
#[derive(Debug, Clone)]
pub struct TriedbProvider {
    pub inner: Arc<TrieDbDatabase>
}

impl TriedbProvider {
    pub fn new(db_path: impl AsRef<Path>) -> Self {
        let db_path = db_path.as_ref();
        let db = if db_path.exists() {
            TrieDbDatabase::open(db_path).unwrap()
        } else {
            TrieDbDatabase::create_new(db_path).unwrap()
        };
        Self {
            inner: Arc::new(db),
        }
    }

    pub fn set_account(
        &self,
        address: Address,
        account: Account,
        storage_root: Option<B256>,
    ) -> Result<(), TransactionError> {
        let mut tx = self.inner.begin_rw()?;
        
        let address_path = AddressPath::for_address(address);
        let storage_root = storage_root.unwrap_or(EMPTY_ROOT_HASH);
        let trie_account = TrieDBAccount::new(
            account.nonce,
            account.balance,
            storage_root,
            account.bytecode_hash.unwrap_or(KECCAK_EMPTY),
        );
        
        tx.set_account(address_path, Some(trie_account))?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_account(&self, address: Address) -> Result<Option<Account>, TransactionError> {
        let mut tx = self.inner.begin_ro()?;
        let address_path = AddressPath::for_address(address);

        let trie_account_opt = tx.get_account(&address_path)?;

        let account_opt = trie_account_opt.map(|trie_account| {
            Account {
                nonce: trie_account.nonce,
                balance: trie_account.balance,
                bytecode_hash: if trie_account.code_hash == KECCAK_EMPTY {
                    None
                } else {
                    Some(trie_account.code_hash)
                },
            }
        });

        Ok(account_opt)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn test_triedb_provider_new_set_get() {
        let dir = TempDir::new("triedb_test").unwrap();
        let db_path = dir.path().join("triedb");
        let provider = TriedbProvider::new(&db_path);

        let address = Address::with_last_byte(1);
        let account = Account {
            nonce: 42,
            balance: U256::from(1000),
            bytecode_hash: None,
        };

        provider.set_account(address, account, None).unwrap();

        let provider2 = TriedbProvider::new(&db_path);

        let retrieved_account = provider2.get_account(address).unwrap();

        assert!(retrieved_account.is_some(), "Account should exist");
        let acc = retrieved_account.unwrap();
        assert_eq!(acc.nonce, 42, "Nonce should match");
        assert_eq!(acc.balance, U256::from(1000), "Balance should match");
        assert_eq!(acc.bytecode_hash, None, "Bytecode hash should be None for EOA");
    }

    #[test]
    fn test_triedb_provider_with_contract() {
        let dir = TempDir::new("triedb_test_contract").unwrap();
        let db_path = dir.path().join("triedb");

        let provider = TriedbProvider::new(&db_path);

        let address = Address::with_last_byte(2);
        let code_hash = B256::with_last_byte(0xFF);
        let account = Account {
            nonce: 1,
            balance: U256::from(5000),
            bytecode_hash: Some(code_hash),
        };

        provider.set_account(address, account, None).unwrap();

        let provider2 = TriedbProvider::new(&db_path);
        let retrieved_account = provider2.get_account(address).unwrap();

        assert!(retrieved_account.is_some(), "Contract account should exist");
        let acc = retrieved_account.unwrap();
        assert_eq!(acc.nonce, 1, "Nonce should match");
        assert_eq!(acc.balance, U256::from(5000), "Balance should match");
        assert_eq!(acc.bytecode_hash, Some(code_hash), "Code hash should match");
    }

    #[test]
    fn test_triedb_provider_nonexistent_account() {
        let dir = TempDir::new("triedb_test_nonexistent").unwrap();
        let db_path = dir.path().join("triedb");

        let provider = TriedbProvider::new(&db_path);

        let nonexistent_address = Address::with_last_byte(99);
        let result = provider.get_account(nonexistent_address).unwrap();

        assert!(result.is_none(), "Nonexistent account should return None");
    }
}