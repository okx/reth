//! P1 acceptance test for SS EVMStateStore (B-06).
//!
//! Ported from Go TestCodeSizeGoesToLegacyDB in
//! mpt-db/state_db/ss/evm/db_test.go.

use mptdb_common::{
    config::StateStoreConfig,
    evm_keys::{
        CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, CODE_SIZE_KEY_PREFIX, NONCE_KEY_PREFIX,
        STATE_KEY_PREFIX,
    },
};
use mptdb_proto::{ChangeSet, KvPair, NamedChangeSet};
use mptdb_ss::evm::store::EVMStateStore;
use mptdb_traits::ss::StateStore;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const EVM_STORE_KEY: &str = "evm";

fn test_config(dir: &std::path::Path) -> StateStoreConfig {
    StateStoreConfig {
        db_directory: dir.to_string_lossy().to_string(),
        keep_last_version: true,
        ..Default::default()
    }
}

fn open_evm_store(dir: &std::path::Path) -> EVMStateStore {
    let cfg = test_config(dir);
    EVMStateStore::new(&dir.to_string_lossy(), &cfg).unwrap()
}

fn make_evm_changeset(pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
    vec![NamedChangeSet { name: EVM_STORE_KEY.to_string(), changeset: Some(ChangeSet { pairs }) }]
}

// ===========================================================================
// B-06: CodeSize key goes to legacy DB
// ===========================================================================
// Ported from Go TestCodeSizeGoesToLegacyDB.
//
// Create EVMStateStore. Apply a changeset with a CodeSize key (prefix 0x09).
// Verify:
// - The key is retrievable via get() with the original key
// - The key is NOT in nonce/codehash/code/storage sub-DBs (those would require different prefix
//   bytes)

#[test]
fn b06_codesize_key_goes_to_legacy_db() {
    let dir = tempdir().unwrap();
    let store = open_evm_store(dir.path());

    // Build a CodeSize key: prefix 0x09 + 20-byte address.
    let mut addr = [0u8; 20];
    addr[0] = 0x42;
    let mut code_size_key = vec![CODE_SIZE_KEY_PREFIX];
    code_size_key.extend_from_slice(&addr);

    let code_size_value = vec![0x00, 0x10]; // e.g., size = 16

    // Apply changeset with the CodeSize key.
    let cs = make_evm_changeset(vec![KvPair {
        delete: false,
        key: code_size_key.clone(),
        value: code_size_value.clone(),
    }]);
    store.apply_changeset_sync(1, &cs).unwrap();

    // Verify: get() with the original key returns the value.
    let val = store.get(EVM_STORE_KEY, 1, &code_size_key).unwrap();
    assert_eq!(val, Some(code_size_value.clone()), "CodeSize key should be retrievable via get()");

    // Verify: has() returns true for the CodeSize key.
    let has = store.has(EVM_STORE_KEY, 1, &code_size_key).unwrap();
    assert!(has, "CodeSize key should exist via has()");

    // Verify: the key is NOT accessible when constructed as nonce/codehash/code/storage keys.
    // If CodeSize were misrouted to nonce, a lookup with a nonce-prefixed key for the
    // same address would find it. It should NOT.
    let mut nonce_key = vec![NONCE_KEY_PREFIX];
    nonce_key.extend_from_slice(&addr);
    let nonce_val = store.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
    assert!(nonce_val.is_none(), "CodeSize value should NOT be in nonce sub-DB");

    let mut codehash_key = vec![CODE_HASH_KEY_PREFIX];
    codehash_key.extend_from_slice(&addr);
    let codehash_val = store.get(EVM_STORE_KEY, 1, &codehash_key).unwrap();
    assert!(codehash_val.is_none(), "CodeSize value should NOT be in codehash sub-DB");

    let mut code_key = vec![CODE_KEY_PREFIX];
    code_key.extend_from_slice(&addr);
    let code_val = store.get(EVM_STORE_KEY, 1, &code_key).unwrap();
    assert!(code_val.is_none(), "CodeSize value should NOT be in code sub-DB");

    // Storage key would need addr + 32-byte slot; just verify the CodeSize key
    // is not accidentally accessible via a storage lookup.
    let mut storage_key = vec![STATE_KEY_PREFIX];
    storage_key.extend_from_slice(&addr);
    storage_key.extend_from_slice(&[0u8; 32]);
    let storage_val = store.get(EVM_STORE_KEY, 1, &storage_key).unwrap();
    assert!(storage_val.is_none(), "CodeSize value should NOT be in storage sub-DB");
}
