use crate::flatkv::{
    keys::{account_key, address_from_bytes, decode_account_value},
    store::CommitStore,
};
use seidb_common::evm_keys::{parse_evm_key, EvmKeyKind};
use seidb_traits::kv::KvEngine;

impl CommitStore {
    /// Reads a value by its memiavl EVM key.
    ///
    /// Pending writes always take priority over committed DB state.
    /// Returns `(Some(value), true)` if found, `(None, false)` if not.
    pub fn get(&self, key: &[u8]) -> (Option<Vec<u8>>, bool) {
        let (kind, stripped_key) = parse_evm_key(key);

        match kind {
            EvmKeyKind::Empty => (None, false),

            EvmKeyKind::Storage => {
                // Check pending writes first.
                if let Some(pw) = self.storage_writes.get(stripped_key) {
                    if pw.is_delete {
                        return (None, false);
                    }
                    return (Some(pw.value.clone()), true);
                }

                // Fall back to storage DB.
                let db = match self.storage_db.as_ref() {
                    Some(db) => db,
                    None => return (None, false),
                };
                match db.get(stripped_key) {
                    Ok(Some(val)) => (Some(val), true),
                    _ => (None, false),
                }
            }

            EvmKeyKind::Nonce | EvmKeyKind::CodeHash => {
                let addr = match address_from_bytes(stripped_key) {
                    Some(a) => a,
                    None => return (None, false),
                };

                // Check pending account writes first.
                if let Some(paw) = self.account_writes.get(&addr[..]) {
                    if paw.is_delete {
                        return (None, false);
                    }
                    if kind == EvmKeyKind::Nonce {
                        return (Some(paw.value.nonce.to_be_bytes().to_vec()), true);
                    }
                    // CodeHash: all-zero means EOA -> return not found.
                    if paw.value.code_hash == [0u8; 32] {
                        return (None, false);
                    }
                    return (Some(paw.value.code_hash.to_vec()), true);
                }

                // Fall back to account DB.
                let db = match self.account_db.as_ref() {
                    Some(db) => db,
                    None => return (None, false),
                };
                let encoded = match db.get(&account_key(&addr)) {
                    Ok(Some(raw)) => raw,
                    _ => return (None, false),
                };
                let av = match decode_account_value(&encoded) {
                    Ok(av) => av,
                    Err(_) => return (None, false),
                };

                if kind == EvmKeyKind::Nonce {
                    (Some(av.nonce.to_be_bytes().to_vec()), true)
                } else {
                    // CodeHash: all-zero (EOA) -> not found.
                    if av.code_hash == [0u8; 32] {
                        (None, false)
                    } else {
                        (Some(av.code_hash.to_vec()), true)
                    }
                }
            }

            EvmKeyKind::Code => {
                // Check pending writes first.
                if let Some(pw) = self.code_writes.get(stripped_key) {
                    if pw.is_delete {
                        return (None, false);
                    }
                    return (Some(pw.value.clone()), true);
                }

                // Fall back to code DB.
                let db = match self.code_db.as_ref() {
                    Some(db) => db,
                    None => return (None, false),
                };
                match db.get(stripped_key) {
                    Ok(Some(val)) => (Some(val), true),
                    _ => (None, false),
                }
            }

            EvmKeyKind::Legacy => {
                // Check pending writes first.
                if let Some(pw) = self.legacy_writes.get(stripped_key) {
                    if pw.is_delete {
                        return (None, false);
                    }
                    return (Some(pw.value.clone()), true);
                }

                // Fall back to legacy DB.
                let db = match self.legacy_db.as_ref() {
                    Some(db) => db,
                    None => return (None, false),
                };
                match db.get(stripped_key) {
                    Ok(Some(val)) => (Some(val), true),
                    _ => (None, false),
                }
            }
        }
    }

    /// Reports whether the given memiavl key exists (pending or committed).
    pub fn has(&self, key: &[u8]) -> bool {
        self.get(key).1
    }
}

#[cfg(test)]
mod tests {
    use crate::flatkv::{
        keys::{ADDRESS_LEN, CODE_HASH_LEN, NONCE_LEN},
        store::CommitStore,
    };
    use seidb_common::{
        config::FlatKvConfig,
        evm_keys::{CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
    };
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::TempDir;

    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    fn test_addr(seed: u8) -> [u8; ADDRESS_LEN] {
        let mut addr = [0u8; ADDRESS_LEN];
        addr[0] = seed;
        addr[19] = seed;
        addr
    }

    fn test_slot(seed: u8) -> [u8; 32] {
        let mut slot = [0u8; 32];
        slot[0] = seed;
        slot
    }

    fn make_storage_key(addr: &[u8; ADDRESS_LEN], slot: &[u8; 32]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN + 32);
        key.push(STATE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key.extend_from_slice(slot);
        key
    }

    fn make_nonce_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(NONCE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    fn make_codehash_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(CODE_HASH_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    fn make_code_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(CODE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    fn encode_nonce(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    fn evm_cs(pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
        vec![NamedChangeSet { name: "evm".to_string(), changeset: Some(ChangeSet { pairs }) }]
    }

    #[test]
    fn test_get_pending_storage() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(1);
        let slot = test_slot(0xAA);
        let memiavl_key = make_storage_key(&addr, &slot);

        let cs = evm_cs(vec![KvPair {
            key: memiavl_key.clone(),
            value: b"pending_val".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();

        // Pending write should be visible via get.
        let (val, found) = store.get(&memiavl_key);
        assert!(found);
        assert_eq!(val.unwrap(), b"pending_val");
    }

    #[test]
    fn test_get_committed_storage() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(2);
        let slot = test_slot(0xBB);
        let memiavl_key = make_storage_key(&addr, &slot);

        let cs = evm_cs(vec![KvPair {
            key: memiavl_key.clone(),
            value: b"committed_val".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();

        // After commit, pending writes are cleared. Value should come from DB.
        let (val, found) = store.get(&memiavl_key);
        assert!(found);
        assert_eq!(val.unwrap(), b"committed_val");
    }

    #[test]
    fn test_get_pending_delete() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(3);
        let slot = test_slot(0xCC);
        let memiavl_key = make_storage_key(&addr, &slot);

        // First write, then commit.
        let cs1 = evm_cs(vec![KvPair {
            key: memiavl_key.clone(),
            value: b"to_delete".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs1).unwrap();
        store.commit().unwrap();

        // Now delete (pending).
        let cs2 =
            evm_cs(vec![KvPair { key: memiavl_key.clone(), value: Vec::new(), delete: true }]);
        store.apply_change_sets(&cs2).unwrap();

        // Pending delete should shadow the committed value.
        let (val, found) = store.get(&memiavl_key);
        assert!(!found);
        assert!(val.is_none());
    }

    #[test]
    fn test_get_nonce() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(4);
        let nonce_key = make_nonce_key(&addr);

        let cs =
            evm_cs(vec![KvPair { key: nonce_key.clone(), value: encode_nonce(42), delete: false }]);
        store.apply_change_sets(&cs).unwrap();

        // Read nonce from pending writes.
        let (val, found) = store.get(&nonce_key);
        assert!(found);
        let nonce_bytes = val.unwrap();
        assert_eq!(nonce_bytes.len(), NONCE_LEN);
        assert_eq!(u64::from_be_bytes(nonce_bytes.try_into().unwrap()), 42);

        // Also verify after commit (from DB).
        store.commit().unwrap();
        let (val2, found2) = store.get(&nonce_key);
        assert!(found2);
        let nonce_bytes2 = val2.unwrap();
        assert_eq!(u64::from_be_bytes(nonce_bytes2.try_into().unwrap()), 42);
    }

    #[test]
    fn test_get_codehash() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(5);
        let codehash_key = make_codehash_key(&addr);

        let mut code_hash = [0xABu8; CODE_HASH_LEN];
        code_hash[0] = 0x11;
        code_hash[31] = 0xCD;

        let cs = evm_cs(vec![KvPair {
            key: codehash_key.clone(),
            value: code_hash.to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();

        // Read from pending.
        let (val, found) = store.get(&codehash_key);
        assert!(found);
        assert_eq!(val.unwrap(), code_hash.to_vec());

        // Read from DB after commit.
        store.commit().unwrap();
        let (val2, found2) = store.get(&codehash_key);
        assert!(found2);
        assert_eq!(val2.unwrap(), code_hash.to_vec());
    }

    #[test]
    fn test_get_codehash_eoa() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(6);
        let codehash_key = make_codehash_key(&addr);

        // Set nonce only (no codehash) — account exists but is an EOA.
        let cs = evm_cs(vec![KvPair {
            key: make_nonce_key(&addr),
            value: encode_nonce(1),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();

        // Pending: codehash for EOA should return (None, false).
        let (val, found) = store.get(&codehash_key);
        assert!(!found);
        assert!(val.is_none());

        // After commit, same behavior from DB.
        store.commit().unwrap();
        let (val2, found2) = store.get(&codehash_key);
        assert!(!found2);
        assert!(val2.is_none());
    }

    #[test]
    fn test_get_code() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(7);
        let code_key = make_code_key(&addr);
        let bytecode = b"deadbeef_bytecode".to_vec();

        let cs =
            evm_cs(vec![KvPair { key: code_key.clone(), value: bytecode.clone(), delete: false }]);
        store.apply_change_sets(&cs).unwrap();

        // Read from pending.
        let (val, found) = store.get(&code_key);
        assert!(found);
        assert_eq!(val.unwrap(), bytecode);

        // Read from DB after commit.
        store.commit().unwrap();
        let (val2, found2) = store.get(&code_key);
        assert!(found2);
        assert_eq!(val2.unwrap(), bytecode);
    }

    #[test]
    fn test_get_legacy() {
        let (mut store, _dir) = open_store();
        let legacy_key = vec![0x01, 0xAA, 0xBB, 0xCC];

        let cs = evm_cs(vec![KvPair {
            key: legacy_key.clone(),
            value: b"legacy_data".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();

        // Read from pending.
        let (val, found) = store.get(&legacy_key);
        assert!(found);
        assert_eq!(val.unwrap(), b"legacy_data");

        // Read from DB after commit.
        store.commit().unwrap();
        let (val2, found2) = store.get(&legacy_key);
        assert!(found2);
        assert_eq!(val2.unwrap(), b"legacy_data");
    }

    #[test]
    fn test_get_unknown_key() {
        let (store, _dir) = open_store();

        // Empty key -> (None, false).
        let (val, found) = store.get(&[]);
        assert!(!found);
        assert!(val.is_none());
    }

    #[test]
    fn test_has() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(9);
        let slot = test_slot(0xDD);
        let memiavl_key = make_storage_key(&addr, &slot);

        assert!(!store.has(&memiavl_key));

        let cs = evm_cs(vec![KvPair {
            key: memiavl_key.clone(),
            value: b"exists".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();

        assert!(store.has(&memiavl_key));

        // After delete, has returns false.
        let cs_del =
            evm_cs(vec![KvPair { key: memiavl_key.clone(), value: Vec::new(), delete: true }]);
        store.apply_change_sets(&cs_del).unwrap();

        assert!(!store.has(&memiavl_key));
    }
}
