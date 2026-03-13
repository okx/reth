use crate::flatkv::{
    keys::{
        account_key, address_from_bytes, decode_account_value, encode_account_value, AccountValue,
        Address, CODE_HASH_LEN, NONCE_LEN,
    },
    lthash_compute::{compute_lt_hash, KvPairWithLastValue},
    store::{CommitStore, PendingAccountWrite, PendingKvWrite},
};
use seidb_common::{
    error::{Result, SeiDbError},
    evm_keys::{parse_evm_key, EvmKeyKind},
};
use seidb_proto::NamedChangeSet;
use seidb_traits::kv::KvEngine;
use std::collections::HashMap;

/// The store name that contains EVM state changesets.
const EVM_STORE_NAME: &str = "evm";

impl CommitStore {
    /// Buffers EVM changesets and updates the working LtHash.
    ///
    /// Multiple calls accumulate before `commit()`. LtHash delta order:
    /// storage -> account -> code -> legacy (matches Go).
    pub fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()> {
        // Save original changesets for changelog (extend, don't replace).
        self.pending_change_sets.extend_from_slice(cs);

        // Collect LtHash pairs per DB (using internal key format).
        let mut storage_pairs: Vec<KvPairWithLastValue> = Vec::new();
        let mut code_pairs: Vec<KvPairWithLastValue> = Vec::new();
        let mut legacy_pairs: Vec<KvPairWithLastValue> = Vec::new();
        // Account pairs are collected at the end after all account changes are processed.

        // Pre-capture raw encoded account bytes so LtHash delta uses the correct
        // baseline across multiple ApplyChangeSets calls before Commit.
        // None means the account didn't exist (no phantom MixOut for new accounts).
        let mut old_account_raw_values: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();

        for named_cs in cs {
            // Only process EVM store changesets.
            if named_cs.name != EVM_STORE_NAME {
                continue;
            }

            let changeset = match &named_cs.changeset {
                Some(cs) => cs,
                None => continue,
            };

            for pair in &changeset.pairs {
                let (kind, key_bytes) = parse_evm_key(&pair.key);

                match kind {
                    EvmKeyKind::Empty => continue,

                    EvmKeyKind::Storage => {
                        // Get old value for LtHash.
                        let old_value = self.get_storage_last_value(key_bytes)?;

                        let key_vec = key_bytes.to_vec();
                        let value = if pair.delete { Vec::new() } else { pair.value.clone() };

                        storage_pairs.push(KvPairWithLastValue {
                            key: key_vec.clone(),
                            value: value.clone(),
                            last_value: old_value.unwrap_or_default(),
                            delete: pair.delete,
                        });

                        self.storage_writes.insert(
                            key_vec.clone(),
                            PendingKvWrite { key: key_vec, value, is_delete: pair.delete },
                        );
                    }

                    EvmKeyKind::Nonce | EvmKeyKind::CodeHash => {
                        let addr = address_from_bytes(key_bytes).ok_or_else(|| {
                            SeiDbError::Other(format!(
                                "invalid address length {} for key kind {:?}",
                                key_bytes.len(),
                                kind,
                            ))
                        })?;
                        let addr_key = addr.to_vec();

                        // Pre-capture: record old raw value BEFORE updating, only once per addr.
                        if let std::collections::hash_map::Entry::Vacant(entry) =
                            old_account_raw_values.entry(addr_key.clone())
                        {
                            if let Some(paw) = self.account_writes.get(entry.key()) {
                                // Account already in pending from a previous apply — use its
                                // already-captured last_raw_value to avoid phantom MixOut.
                                entry.insert(paw.last_raw_value.clone().unwrap_or_default());
                            } else {
                                // Load from DB.
                                let db = self.account_db.as_ref().ok_or_else(|| {
                                    SeiDbError::Other("account_db not open".into())
                                })?;
                                match db.get(&account_key(&addr))? {
                                    Some(raw) => {
                                        entry.insert(raw);
                                    }
                                    None => {
                                        // New account — empty means no MixOut.
                                        entry.insert(Vec::new());
                                    }
                                }
                            }
                        }

                        // Get or create the pending account write.
                        let paw = self.get_or_create_pending_account(&addr)?;

                        if pair.delete {
                            if kind == EvmKeyKind::Nonce {
                                paw.value.nonce = 0;
                            } else {
                                paw.value.code_hash = [0u8; CODE_HASH_LEN];
                            }
                        } else if kind == EvmKeyKind::Nonce {
                            if pair.value.len() != NONCE_LEN {
                                return Err(SeiDbError::Other(format!(
                                    "invalid nonce value length: got {}, expected {}",
                                    pair.value.len(),
                                    NONCE_LEN,
                                )));
                            }
                            paw.value.nonce =
                                u64::from_be_bytes(pair.value[..NONCE_LEN].try_into().unwrap());
                        } else {
                            // CodeHash
                            if pair.value.len() != CODE_HASH_LEN {
                                return Err(SeiDbError::Other(format!(
                                    "invalid codehash value length: got {}, expected {}",
                                    pair.value.len(),
                                    CODE_HASH_LEN,
                                )));
                            }
                            paw.value.code_hash.copy_from_slice(&pair.value);
                        }
                    }

                    EvmKeyKind::Code => {
                        let old_value = self.get_code_last_value(key_bytes)?;

                        let key_vec = key_bytes.to_vec();
                        let value = if pair.delete { Vec::new() } else { pair.value.clone() };

                        code_pairs.push(KvPairWithLastValue {
                            key: key_vec.clone(),
                            value: value.clone(),
                            last_value: old_value.unwrap_or_default(),
                            delete: pair.delete,
                        });

                        self.code_writes.insert(
                            key_vec.clone(),
                            PendingKvWrite { key: key_vec, value, is_delete: pair.delete },
                        );
                    }

                    EvmKeyKind::Legacy => {
                        let old_value = self.get_legacy_last_value(key_bytes)?;

                        let key_vec = key_bytes.to_vec();
                        let value = if pair.delete { Vec::new() } else { pair.value.clone() };

                        legacy_pairs.push(KvPairWithLastValue {
                            key: key_vec.clone(),
                            value: value.clone(),
                            last_value: old_value.unwrap_or_default(),
                            delete: pair.delete,
                        });

                        self.legacy_writes.insert(
                            key_vec.clone(),
                            PendingKvWrite { key: key_vec, value, is_delete: pair.delete },
                        );
                    }
                }
            }
        }

        // Collect account LtHash pairs from accounts that were touched.
        let mut account_pairs: Vec<KvPairWithLastValue> =
            Vec::with_capacity(old_account_raw_values.len());
        for (addr_key, old_raw) in &old_account_raw_values {
            if let Some(paw) = self.account_writes.get(addr_key) {
                account_pairs.push(KvPairWithLastValue {
                    key: account_key(&paw.addr),
                    value: encode_account_value(&paw.value),
                    last_value: old_raw.clone(), // empty for new accounts -> no phantom MixOut
                    delete: paw.is_delete,
                });
            }
        }

        // Combine: storage -> account -> code -> legacy (matches Go L216-218).
        let mut all_pairs = storage_pairs;
        all_pairs.append(&mut account_pairs);
        all_pairs.append(&mut code_pairs);
        all_pairs.append(&mut legacy_pairs);

        if !all_pairs.is_empty() {
            self.working_lt_hash = compute_lt_hash(&self.working_lt_hash, &all_pairs);
        }

        Ok(())
    }

    /// Returns a mutable reference to the pending account write for `addr`,
    /// creating one from DB state if it doesn't exist yet.
    fn get_or_create_pending_account(
        &mut self,
        addr: &Address,
    ) -> Result<&mut PendingAccountWrite> {
        use std::collections::hash_map::Entry;

        let addr_key = addr.to_vec();
        match self.account_writes.entry(addr_key) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                // Load existing account value from DB (or default for new accounts).
                let db = self
                    .account_db
                    .as_ref()
                    .ok_or_else(|| SeiDbError::Other("account_db not open".into()))?;

                let (value, last_raw_value) = match db.get(&account_key(addr))? {
                    Some(raw) => {
                        let av = decode_account_value(&raw)?;
                        (av, Some(raw))
                    }
                    None => (AccountValue::default(), None),
                };

                Ok(entry.insert(PendingAccountWrite {
                    addr: *addr,
                    value,
                    is_delete: false,
                    last_raw_value,
                }))
            }
        }
    }

    /// Returns the last (current) value for a storage key, checking pending
    /// writes first then falling back to the storage DB.
    fn get_storage_last_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(pw) = self.storage_writes.get(key) {
            if pw.is_delete {
                return Ok(None);
            }
            return Ok(Some(pw.value.clone()));
        }

        let db = self
            .storage_db
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("storage_db not open".into()))?;
        db.get(key)
    }

    /// Returns the last (current) value for a code key, checking pending
    /// writes first then falling back to the code DB.
    fn get_code_last_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(pw) = self.code_writes.get(key) {
            if pw.is_delete {
                return Ok(None);
            }
            return Ok(Some(pw.value.clone()));
        }

        let db =
            self.code_db.as_ref().ok_or_else(|| SeiDbError::Other("code_db not open".into()))?;
        db.get(key)
    }

    /// Returns the last (current) value for a legacy key, checking pending
    /// writes first then falling back to the legacy DB.
    fn get_legacy_last_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(pw) = self.legacy_writes.get(key) {
            if pw.is_delete {
                return Ok(None);
            }
            return Ok(Some(pw.value.clone()));
        }

        let db = self
            .legacy_db
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("legacy_db not open".into()))?;
        db.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatkv::keys::{ADDRESS_LEN, CODE_HASH_LEN};
    use seidb_common::{
        config::FlatKvConfig,
        evm_keys::{CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX},
    };
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::TempDir;

    /// Helper: create a CommitStore with all DBs open.
    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    /// Helper: build a test address.
    fn test_addr(seed: u8) -> [u8; ADDRESS_LEN] {
        let mut addr = [0u8; ADDRESS_LEN];
        addr[0] = seed;
        addr[19] = seed;
        addr
    }

    /// Helper: build a 32-byte slot.
    fn test_slot(seed: u8) -> [u8; 32] {
        let mut slot = [0u8; 32];
        slot[0] = seed;
        slot
    }

    /// Helper: build a storage memiavl key (prefix 0x03 || addr || slot).
    fn make_storage_key(addr: &[u8; ADDRESS_LEN], slot: &[u8; 32]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN + 32);
        key.push(STATE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key.extend_from_slice(slot);
        key
    }

    /// Helper: build a nonce memiavl key (prefix 0x0a || addr).
    fn make_nonce_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(NONCE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    /// Helper: build a codehash memiavl key (prefix 0x08 || addr).
    fn make_codehash_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(CODE_HASH_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    /// Helper: build a code memiavl key (prefix 0x07 || addr).
    fn make_code_key(addr: &[u8; ADDRESS_LEN]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN);
        key.push(CODE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key
    }

    /// Helper: build a single-pair EVM NamedChangeSet.
    fn evm_cs(pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
        vec![NamedChangeSet { name: "evm".to_string(), changeset: Some(ChangeSet { pairs }) }]
    }

    /// Helper: encode nonce as 8-byte big-endian.
    fn encode_nonce(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    #[test]
    fn test_apply_storage_write() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(1);
        let slot = test_slot(0xAA);
        let memiavl_key = make_storage_key(&addr, &slot);

        let cs = evm_cs(vec![KvPair {
            key: memiavl_key,
            value: b"storage_value".to_vec(),
            delete: false,
        }]);

        store.apply_change_sets(&cs).unwrap();

        // The internal key in storage_writes is addr||slot (stripped prefix).
        let mut internal_key = addr.to_vec();
        internal_key.extend_from_slice(&slot);
        let pw = store.storage_writes.get(&internal_key).unwrap();
        assert!(!pw.is_delete);
        assert_eq!(pw.value, b"storage_value");
    }

    #[test]
    fn test_apply_account_nonce() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(2);
        let key = make_nonce_key(&addr);

        let cs = evm_cs(vec![KvPair { key, value: encode_nonce(42), delete: false }]);

        store.apply_change_sets(&cs).unwrap();

        let paw = store.account_writes.get(&addr.to_vec()).unwrap();
        assert_eq!(paw.value.nonce, 42);
    }

    #[test]
    fn test_apply_account_codehash() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(3);
        let key = make_codehash_key(&addr);

        let mut code_hash = [0xABu8; CODE_HASH_LEN];
        code_hash[31] = 0xCD;

        let cs = evm_cs(vec![KvPair { key, value: code_hash.to_vec(), delete: false }]);

        store.apply_change_sets(&cs).unwrap();

        let paw = store.account_writes.get(&addr.to_vec()).unwrap();
        assert_eq!(paw.value.code_hash, code_hash);
    }

    #[test]
    fn test_apply_code_write() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(4);
        let key = make_code_key(&addr);
        let bytecode = b"deadbeef_bytecode".to_vec();

        let cs = evm_cs(vec![KvPair { key, value: bytecode.clone(), delete: false }]);

        store.apply_change_sets(&cs).unwrap();

        let pw = store.code_writes.get(&addr.to_vec()).unwrap();
        assert!(!pw.is_delete);
        assert_eq!(pw.value, bytecode);
    }

    #[test]
    fn test_apply_legacy_write() {
        let (mut store, _dir) = open_store();
        // A key with an unknown prefix is legacy.
        let legacy_key = vec![0x01, 0xAA, 0xBB, 0xCC];

        let cs = evm_cs(vec![KvPair {
            key: legacy_key.clone(),
            value: b"legacy_value".to_vec(),
            delete: false,
        }]);

        store.apply_change_sets(&cs).unwrap();

        // Legacy keys are stored with the full original key.
        let pw = store.legacy_writes.get(&legacy_key).unwrap();
        assert!(!pw.is_delete);
        assert_eq!(pw.value, b"legacy_value");
    }

    #[test]
    fn test_apply_delete() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(5);
        let slot = test_slot(0xBB);
        let memiavl_key = make_storage_key(&addr, &slot);

        let cs = evm_cs(vec![KvPair { key: memiavl_key, value: Vec::new(), delete: true }]);

        store.apply_change_sets(&cs).unwrap();

        let mut internal_key = addr.to_vec();
        internal_key.extend_from_slice(&slot);
        let pw = store.storage_writes.get(&internal_key).unwrap();
        assert!(pw.is_delete);
    }

    #[test]
    fn test_apply_account_field_merge() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(6);
        let nonce_key = make_nonce_key(&addr);
        let codehash_key = make_codehash_key(&addr);

        let mut code_hash = [0xFFu8; CODE_HASH_LEN];
        code_hash[0] = 0x11;

        // Both nonce and codehash for the same address in one changeset.
        let cs = evm_cs(vec![
            KvPair { key: nonce_key, value: encode_nonce(99), delete: false },
            KvPair { key: codehash_key, value: code_hash.to_vec(), delete: false },
        ]);

        store.apply_change_sets(&cs).unwrap();

        let paw = store.account_writes.get(&addr.to_vec()).unwrap();
        assert_eq!(paw.value.nonce, 99);
        assert_eq!(paw.value.code_hash, code_hash);
    }

    #[test]
    fn test_apply_multiple_before_commit() {
        let (mut store, _dir) = open_store();
        let addr = test_addr(7);
        let slot1 = test_slot(0x01);
        let slot2 = test_slot(0x02);

        // First apply.
        let cs1 = evm_cs(vec![KvPair {
            key: make_storage_key(&addr, &slot1),
            value: b"val1".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs1).unwrap();

        // Second apply.
        let cs2 = evm_cs(vec![KvPair {
            key: make_storage_key(&addr, &slot2),
            value: b"val2".to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs2).unwrap();

        // Both writes should be accumulated.
        let mut key1 = addr.to_vec();
        key1.extend_from_slice(&slot1);
        let mut key2 = addr.to_vec();
        key2.extend_from_slice(&slot2);

        assert!(store.storage_writes.contains_key(&key1));
        assert!(store.storage_writes.contains_key(&key2));

        // pending_change_sets should have both.
        assert_eq!(store.pending_change_sets.len(), 2);
    }

    #[test]
    fn test_apply_non_evm_ignored() {
        let (mut store, _dir) = open_store();

        let cs = vec![NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair {
                    key: vec![0x01, 0x02, 0x03],
                    value: b"should_be_ignored".to_vec(),
                    delete: false,
                }],
            }),
        }];

        store.apply_change_sets(&cs).unwrap();

        // No writes should have been buffered.
        assert!(store.storage_writes.is_empty());
        assert!(store.account_writes.is_empty());
        assert!(store.code_writes.is_empty());
        assert!(store.legacy_writes.is_empty());

        // But pending_change_sets still recorded for changelog.
        assert_eq!(store.pending_change_sets.len(), 1);
    }

    #[test]
    fn test_lt_hash_updated() {
        let (mut store, _dir) = open_store();
        let hash_before = store.working_lt_hash.clone();

        let addr = test_addr(8);
        let slot = test_slot(0xCC);
        let cs = evm_cs(vec![KvPair {
            key: make_storage_key(&addr, &slot),
            value: b"some_value".to_vec(),
            delete: false,
        }]);

        store.apply_change_sets(&cs).unwrap();

        // Working LtHash must have changed.
        assert_ne!(store.working_lt_hash, hash_before);
    }

    #[test]
    fn test_lt_hash_no_phantom_mixout() {
        // For a brand-new account (not in DB, not in pending), the LtHash should
        // only MixIn the new value, NOT MixOut any phantom old value.
        let (mut store, _dir) = open_store();
        let hash_before = store.working_lt_hash.clone();

        let addr = test_addr(9);
        let nonce_key = make_nonce_key(&addr);

        let cs = evm_cs(vec![KvPair { key: nonce_key, value: encode_nonce(1), delete: false }]);

        store.apply_change_sets(&cs).unwrap();

        // Manually compute what the expected LtHash should be:
        // 1. Account pair: key=addr(20), value=encode(nonce=1, balance=0, codehash=0),
        //    last_value=empty -> MixIn only, no MixOut.
        let mut expected_av = AccountValue::default();
        expected_av.nonce = 1;
        let account_internal_key = account_key(&addr);
        let account_new_value = encode_account_value(&expected_av);

        let pairs = vec![KvPairWithLastValue {
            key: account_internal_key,
            value: account_new_value,
            last_value: Vec::new(), // empty = no MixOut
            delete: false,
        }];
        let expected_hash = compute_lt_hash(&hash_before, &pairs);

        assert_eq!(store.working_lt_hash, expected_hash);
    }
}
