use alloy_primitives::{B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::{Nibbles, EMPTY_ROOT_HASH};
use fs4::fs_std::FileExt;
use rayon::prelude::*;
use revm_database::BundleState;
use seidb_common::error::{Result, SeiDbError};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use super::{
    manifest::VersionManifest,
    parallel::ParallelismThresholds,
    persisted::{self, PersistedTrieStore},
    r#trait::MptCommitter,
    state::{self, DirtyAccount},
    tree::MptTree,
};

/// Test-only failure injection points for deterministic failure testing.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitFailPoint {
    BeforePersist,
    AfterPersistBeforeManifest,
    ManifestSave,
}

/// Intermediate result from parallel storage trie root computation.
struct StorageTrieCommitArtifacts {
    hashed_address: B256,
    storage_root: B256,
    node_blobs: Vec<(B256, Vec<u8>)>,
}

/// MPT-based commit store with persistence, rollback, and recovery support.
pub struct MptCommitStore {
    #[allow(dead_code)]
    dir: PathBuf,
    manifest_path: PathBuf,

    account_trie: MptTree,
    /// Per-account storage tries (hashed_address -> storage trie).
    storage_tries: HashMap<B256, MptTree>,
    dirty_accounts: Vec<DirtyAccount>,

    persisted: PersistedTrieStore,
    manifest: VersionManifest,

    version: i64,
    applied_this_block: bool,
    poisoned: bool,
    read_only: bool,
    file_lock: Option<File>,

    parallelism: ParallelismThresholds,

    #[cfg(test)]
    fail_point: Option<CommitFailPoint>,
}

impl MptCommitStore {
    /// Open an MptCommitStore at the given directory.
    ///
    /// `read_only=true` disables writes and does not acquire the exclusive lock.
    pub fn open(dir: &Path, read_only: bool) -> Result<Self> {
        // Ensure directories exist
        fs::create_dir_all(dir)
            .map_err(|e| SeiDbError::Other(format!("create dir {}: {e}", dir.display())))?;
        let trie_nodes_dir = dir.join("trie_nodes");
        fs::create_dir_all(&trie_nodes_dir)
            .map_err(|e| SeiDbError::Other(format!("create trie_nodes dir: {e}")))?;

        // Writer lock
        let file_lock = if !read_only {
            let lock_path = dir.join("LOCK");
            let lock_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| SeiDbError::Other(format!("open LOCK file: {e}")))?;
            lock_file
                .try_lock_exclusive()
                .map_err(|e| SeiDbError::Other(format!("failed to lock db: {e}")))?;
            Some(lock_file)
        } else {
            None
        };

        let manifest_path = dir.join("manifest.json");
        let manifest = VersionManifest::load(&manifest_path)?;

        let persisted = PersistedTrieStore::open(&trie_nodes_dir)?;

        // Load account trie from latest version's root
        let root = manifest.get_root(manifest.latest_version).unwrap_or(EMPTY_ROOT_HASH);
        let account_trie = persisted::load_tree_from_root(&persisted, root)?;

        let version = manifest.latest_version;

        Ok(Self {
            dir: dir.to_path_buf(),
            manifest_path,
            account_trie,
            storage_tries: HashMap::new(),
            dirty_accounts: Vec::new(),
            persisted,
            manifest,
            version,
            applied_this_block: false,
            poisoned: false,
            read_only,
            file_lock,
            parallelism: ParallelismThresholds::default(),
            #[cfg(test)]
            fail_point: None,
        })
    }

    /// Try to extract storage_root from an existing account leaf in the trie.
    fn get_existing_storage_root(&self, hashed_address: &B256) -> B256 {
        let key = Nibbles::unpack(hashed_address);
        match self.account_trie.get(&key) {
            Some(rlp_bytes) => {
                // Decode TrieAccount RLP to extract storage_root
                match alloy_rlp::Decodable::decode(&mut &rlp_bytes[..]) {
                    Ok(trie_account) => {
                        let ta: alloy_trie::TrieAccount = trie_account;
                        ta.storage_root
                    }
                    Err(_) => EMPTY_ROOT_HASH,
                }
            }
            None => EMPTY_ROOT_HASH,
        }
    }

    /// Load or create a storage trie for the given account.
    fn get_or_load_storage_trie(
        &mut self,
        hashed_address: &B256,
        existing_root: B256,
    ) -> Result<()> {
        if self.storage_tries.contains_key(hashed_address) {
            return Ok(());
        }
        let trie = persisted::load_tree_from_root(&self.persisted, existing_root)?;
        self.storage_tries.insert(*hashed_address, trie);
        Ok(())
    }

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(SeiDbError::Other("store is read-only".to_string()));
        }
        Ok(())
    }

    fn check_not_poisoned(&self) -> Result<()> {
        if self.poisoned {
            return Err(SeiDbError::Other(
                "store is poisoned, call load_version() to recover".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
impl MptCommitStore {
    pub(crate) fn set_fail_point(&mut self, fail: Option<CommitFailPoint>) {
        self.fail_point = fail;
    }

    pub(crate) fn set_parallelism_thresholds(&mut self, thresholds: ParallelismThresholds) {
        self.parallelism = thresholds;
    }
}

impl MptCommitter for MptCommitStore {
    fn apply_bundle_state(&mut self, bundle: &BundleState) -> Result<()> {
        self.check_writable()?;
        self.check_not_poisoned()?;

        if self.applied_this_block {
            return Err(SeiDbError::Other(
                "apply_bundle_state already called for this block".to_string(),
            ));
        }

        let apply_result = self.apply_bundle_state_inner(bundle);
        if apply_result.is_err() {
            self.poisoned = true;
            return apply_result;
        }

        self.applied_this_block = true;
        Ok(())
    }

    fn commit(&mut self) -> Result<(i64, B256)> {
        self.check_writable()?;
        self.check_not_poisoned()?;

        if !self.applied_this_block {
            return Err(SeiDbError::Other("must call apply_bundle_state before commit".to_string()));
        }

        let result = self.commit_inner();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn version(&self) -> i64 {
        self.version
    }

    fn load_version(&mut self) -> Result<()> {
        // Always reload manifest from disk
        self.manifest = VersionManifest::load(&self.manifest_path)?;
        let root = self.manifest.get_root(self.manifest.latest_version).unwrap_or(EMPTY_ROOT_HASH);
        self.account_trie = persisted::load_tree_from_root(&self.persisted, root)?;
        self.version = self.manifest.latest_version;
        self.dirty_accounts.clear();
        self.storage_tries.clear();
        self.applied_this_block = false;
        self.poisoned = false;
        Ok(())
    }

    fn rollback(&mut self, target_version: i64) -> Result<()> {
        self.check_writable()?;

        if target_version < self.manifest.earliest_version ||
            target_version > self.manifest.latest_version
        {
            return Err(SeiDbError::Other(format!(
                "rollback target {} out of range [{}, {}]",
                target_version, self.manifest.earliest_version, self.manifest.latest_version
            )));
        }

        let mut manifest_copy = self.manifest.clone();
        manifest_copy.truncate_after(target_version);
        manifest_copy.save(&self.manifest_path)?;
        self.manifest = manifest_copy;
        self.load_version()
    }

    fn close(&mut self) -> Result<()> {
        self.persisted.close()?;
        self.file_lock = None;
        Ok(())
    }
}

impl MptCommitStore {
    fn apply_bundle_state_inner(&mut self, bundle: &BundleState) -> Result<()> {
        let dirty_accounts = state::collect_dirty_accounts(bundle)?;

        for dirty in &dirty_accounts {
            if dirty.storage_wiped {
                // Wiped: start from empty storage trie, apply new changes on top
                let mut new_trie = MptTree::new();
                for (hashed_slot, value) in &dirty.storage_changes {
                    let slot_key = Nibbles::unpack(hashed_slot);
                    if *value == U256::ZERO {
                        // ZERO = delete (no-op on empty trie)
                        new_trie.delete(&slot_key);
                    } else {
                        let encoded = alloy_rlp_encode_u256(value);
                        new_trie.insert(&slot_key, encoded);
                    }
                }
                self.storage_tries.insert(dirty.hashed_address, new_trie);
            } else if !dirty.storage_changes.is_empty() {
                // Non-wiped but has storage changes: load existing storage trie
                let existing_root = self.get_existing_storage_root(&dirty.hashed_address);
                self.get_or_load_storage_trie(&dirty.hashed_address, existing_root)?;

                let trie = self.storage_tries.get_mut(&dirty.hashed_address).unwrap();
                for (hashed_slot, value) in &dirty.storage_changes {
                    let slot_key = Nibbles::unpack(hashed_slot);
                    if *value == U256::ZERO {
                        trie.delete(&slot_key);
                    } else {
                        let encoded = alloy_rlp_encode_u256(value);
                        trie.insert(&slot_key, encoded);
                    }
                }
            }
            // If no storage changes and not wiped: no storage trie needed (REUSE)
        }

        self.dirty_accounts = dirty_accounts;
        Ok(())
    }

    fn commit_inner(&mut self) -> Result<(i64, B256)> {
        // Phase 1: compute storage roots for all dirty accounts.
        //
        // Collect DELETE/REUSE roots serially (cheap lookups), then compute
        // RECOMPUTE roots potentially in parallel using mem::take on
        // storage_tries for ownership transfer.
        let mut storage_roots: HashMap<B256, B256> = HashMap::new();

        // Pre-fill DELETE and REUSE cases (no trie computation needed)
        for dirty in &self.dirty_accounts {
            if dirty.info.is_none() && dirty.storage_wiped {
                // DELETE case
                storage_roots.insert(dirty.hashed_address, EMPTY_ROOT_HASH);
            } else if !self.storage_tries.contains_key(&dirty.hashed_address) {
                // REUSE case: get from existing account leaf
                let root = self.get_existing_storage_root(&dirty.hashed_address);
                storage_roots.insert(dirty.hashed_address, root);
            }
            // RECOMPUTE case handled below via parallel/serial path
        }

        // Take ownership of storage tries for parallel root computation
        let storage_tries = std::mem::take(&mut self.storage_tries);
        let storage_tries_vec: Vec<(B256, MptTree)> = storage_tries.into_iter().collect();
        let should_parallel =
            self.parallelism.should_parallelize_storage_tries(storage_tries_vec.len());

        let mut storage_artifacts: Vec<StorageTrieCommitArtifacts> = if should_parallel {
            storage_tries_vec
                .into_par_iter()
                .map(|(addr, mut trie)| StorageTrieCommitArtifacts {
                    hashed_address: addr,
                    storage_root: trie.root_hash(),
                    node_blobs: trie.collect_node_blobs(),
                })
                .collect()
        } else {
            storage_tries_vec
                .into_iter()
                .map(|(addr, mut trie)| StorageTrieCommitArtifacts {
                    hashed_address: addr,
                    storage_root: trie.root_hash(),
                    node_blobs: trie.collect_node_blobs(),
                })
                .collect()
        };

        // Sort by hashed_address for deterministic ordering
        storage_artifacts.sort_by_key(|a| a.hashed_address);

        // Merge RECOMPUTE roots into storage_roots map
        for artifact in &storage_artifacts {
            storage_roots.insert(artifact.hashed_address, artifact.storage_root);
        }

        // Phase 2: update account trie
        for dirty in &self.dirty_accounts {
            let key = Nibbles::unpack(&dirty.hashed_address);
            let storage_root = storage_roots[&dirty.hashed_address];

            match &dirty.info {
                None => {
                    // Account destroyed / doesn't exist: delete from trie
                    self.account_trie.delete(&key);
                }
                Some(info) => {
                    // EIP-161: empty account check
                    let is_empty = info.is_empty() && storage_root == EMPTY_ROOT_HASH;
                    if is_empty {
                        self.account_trie.delete(&key);
                    } else {
                        let trie_account = alloy_trie::TrieAccount {
                            nonce: info.nonce,
                            balance: info.balance,
                            storage_root,
                            code_hash: info.code_hash,
                        };
                        let mut rlp_buf = Vec::new();
                        trie_account.encode(&mut rlp_buf);
                        self.account_trie.insert(&key, rlp_buf);
                    }
                }
            }
        }

        // Phase 2b: compute state root (parallel if frontier is wide enough)
        let state_root = if self
            .parallelism
            .should_parallelize_account_frontier(self.account_trie.parallel_frontier_width())
        {
            self.account_trie.root_hash_parallel(&self.parallelism)
        } else {
            self.account_trie.root_hash()
        };

        // Collect all node blobs: account trie + storage artifacts
        let mut all_blobs = self.account_trie.collect_node_blobs();
        all_blobs.extend(storage_artifacts.into_iter().flat_map(|a| a.node_blobs));

        // Check test failpoint: BeforePersist
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::BeforePersist) {
            return Err(SeiDbError::Other("failpoint: BeforePersist".to_string()));
        }

        // Persist nodes
        self.persisted.persist_batch_durable(&all_blobs)?;

        // Check test failpoint: AfterPersistBeforeManifest
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::AfterPersistBeforeManifest) {
            return Err(SeiDbError::Other("failpoint: AfterPersistBeforeManifest".to_string()));
        }

        // Update manifest
        let new_version = self.version + 1;
        let mut manifest_copy = self.manifest.clone();
        manifest_copy.add_version(new_version, state_root)?;

        // Check test failpoint: ManifestSave
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::ManifestSave) {
            return Err(SeiDbError::Other("failpoint: ManifestSave".to_string()));
        }

        manifest_copy.save(&self.manifest_path)?;

        // Commit succeeded: update internal state
        self.manifest = manifest_copy;
        self.version = new_version;
        self.dirty_accounts.clear();
        self.storage_tries.clear();
        self.applied_this_block = false;

        Ok((new_version, state_root))
    }
}

/// Encode a U256 value using alloy_rlp (big-endian, trims leading zeros).
fn alloy_rlp_encode_u256(value: &U256) -> Vec<u8> {
    let mut buf = Vec::new();
    value.encode(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{keccak256, Address};
    use alloy_trie::KECCAK_EMPTY;
    use revm_database::{states::StorageSlot, BundleAccount};
    use revm_state::AccountInfo;
    use tempfile::TempDir;

    fn make_bundle(
        accounts: Vec<(
            Address,
            Option<AccountInfo>,
            revm_database::AccountStatus,
            Vec<(U256, U256, U256)>,
        )>,
    ) -> BundleState {
        let mut state: alloy_primitives::map::HashMap<Address, BundleAccount> =
            alloy_primitives::map::HashMap::default();
        for (address, info, status, storage) in accounts {
            let storage_map: revm_database::StorageWithOriginalValues = storage
                .into_iter()
                .map(|(key, orig, present)| (key, StorageSlot::new_changed(orig, present)))
                .collect();
            let bundle_account = BundleAccount::new(None, info, storage_map, status);
            state.insert(address, bundle_account);
        }
        BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    fn default_info(nonce: u64, balance: u64) -> AccountInfo {
        AccountInfo {
            nonce,
            balance: U256::from(balance),
            code_hash: KECCAK_EMPTY,
            account_id: None,
            code: None,
        }
    }

    /// T5.1: open fresh dir -> version=0
    #[test]
    fn t5_1_open_fresh() {
        let dir = TempDir::new().unwrap();
        let store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.version(), 0);
    }

    /// T5.2: read_only -> apply/commit/rollback all Err
    #[test]
    fn t5_2_read_only() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), true).unwrap();
        assert!(store.apply_bundle_state(&BundleState::default()).is_err());
        assert!(store.commit().is_err());
        assert!(store.rollback(0).is_err());
    }

    /// T5.3: writer double-open fails
    #[test]
    fn t5_3_writer_double_open() {
        let dir = TempDir::new().unwrap();
        let _store1 = MptCommitStore::open(dir.path(), false).unwrap();
        let result = MptCommitStore::open(dir.path(), false);
        assert!(result.is_err());
    }

    /// T5.4: empty bundle apply + commit -> version+1, root unchanged
    #[test]
    fn t5_4_empty_apply_commit() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, root) = store.commit().unwrap();
        assert_eq!(ver, 1);
        assert_eq!(root, EMPTY_ROOT_HASH);
    }

    /// T5.5: single account nonce/balance update -> state_root matches reference
    #[test]
    fn t5_5_single_account_update() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 1000);
        let bundle = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();

        // Compute reference root
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();

        assert_eq!(root, expected);
    }

    /// T5.6: single account storage update -> state_root matches reference
    #[test]
    fn t5_6_single_account_storage() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x02);
        let info = default_info(1, 500);
        let slot_key = U256::from(1);
        let slot_val = U256::from(42);
        let bundle = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot_key, U256::ZERO, slot_val)],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();

        // Compute reference storage root
        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        slot_val.encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot), &encoded_val);
        let storage_root = storage_hb.root();

        // Compute reference state root
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();

        assert_eq!(root, expected);
    }

    /// T5.7: only change account fields, no storage -> reuse old storage_root
    #[test]
    fn t5_7_reuse_storage_root() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x03);
        let info1 = default_info(1, 100);
        let slot_key = U256::from(5);
        let slot_val = U256::from(99);

        // Block 1: create account with storage
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info1),
            revm_database::AccountStatus::Changed,
            vec![(slot_key, U256::ZERO, slot_val)],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        // Block 2: only update balance (no storage changes)
        let info2 = default_info(2, 200);
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info2.clone()),
            revm_database::AccountStatus::Changed,
            vec![],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Root should change (balance changed) but storage_root should be same
        assert_ne!(root1, root2);

        // Verify: compute expected root2 with the storage_root from block 1
        let hashed_slot = keccak256(slot_key.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        slot_val.encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot), &encoded_val);
        let storage_root = storage_hb.root();

        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info2.nonce,
            balance: info2.balance,
            storage_root,
            code_hash: info2.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();
        assert_eq!(root2, expected);
    }

    /// T5.8: storage_wiped=true -> old storage cleared, new slots applied
    #[test]
    fn t5_8_storage_wiped() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x04);
        let info = default_info(1, 100);
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        // Block 1: account with slot1
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot1, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: destroy+recreate with only slot2
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::DestroyedChanged,
            vec![(slot2, U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: only slot2 in storage (slot1 wiped)
        let hashed_slot2 = keccak256(slot2.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        U256::from(20).encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot2), &encoded_val);
        let storage_root = storage_hb.root();

        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        assert_eq!(root2, hb.root());
    }

    /// T5.9: ZERO slot -> leaf deleted
    #[test]
    fn t5_9_zero_slot_deletes() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x05);
        let info = default_info(1, 100);
        let slot = U256::from(1);

        // Block 1: set slot to nonzero
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(77))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: set slot to zero (delete)
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(77), U256::ZERO)],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: no storage slots -> EMPTY_ROOT_HASH for storage
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        assert_eq!(root2, hb.root());
    }

    /// T5.10: selfdestruct without rebuild -> account leaf deleted
    #[test]
    fn t5_10_selfdestruct_no_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x06);
        let info = default_info(1, 100);

        // Block 1: create account
        let bundle1 =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: destroy
        let bundle2 =
            make_bundle(vec![(addr, None, revm_database::AccountStatus::Destroyed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: empty trie
        assert_eq!(root2, EMPTY_ROOT_HASH);
    }

    /// T5.11: selfdestruct then rebuild -> account leaf kept, storage_wiped
    #[test]
    fn t5_11_selfdestruct_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x07);
        let info1 = default_info(1, 100);

        // Block 1: create account with storage
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info1),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        // Block 2: destroy + recreate with new nonce, no storage
        let info2 = default_info(0, 50);
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info2.clone()),
            revm_database::AccountStatus::DestroyedChanged,
            vec![],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // Expected: account exists with empty storage
        let hashed_addr = keccak256(addr);
        let trie_account = alloy_trie::TrieAccount {
            nonce: info2.nonce,
            balance: info2.balance,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: info2.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        let expected = hb.root();

        // But EIP-161: empty account with EMPTY_ROOT_HASH storage => deleted
        // info2: nonce=0, balance=50, code_hash=KECCAK_EMPTY -> not empty (balance > 0)
        assert_eq!(root2, expected);
    }

    /// T5.12: double apply -> Err
    #[test]
    fn t5_12_double_apply() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let result = store.apply_bundle_state(&BundleState::default());
        assert!(result.is_err());
    }

    /// T5.13: commit without apply -> Err
    #[test]
    fn t5_13_commit_without_apply() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let result = store.commit();
        assert!(result.is_err());
    }

    /// T5.14: after successful commit, working state cleared
    #[test]
    fn t5_14_working_state_cleared() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        assert!(!store.applied_this_block);
        assert!(store.dirty_accounts.is_empty());
        assert!(store.storage_tries.is_empty());
    }

    /// T5.15: commit failure -> poisoned
    #[test]
    fn t5_15_commit_failure_poisoned() {
        let dir = TempDir::new().unwrap();

        for fp in [
            CommitFailPoint::BeforePersist,
            CommitFailPoint::AfterPersistBeforeManifest,
            CommitFailPoint::ManifestSave,
        ] {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_fail_point(Some(fp));
            store.apply_bundle_state(&BundleState::default()).unwrap();
            let result = store.commit();
            assert!(result.is_err(), "expected error for failpoint {fp:?}");
            assert!(store.poisoned);
            assert!(store.commit().is_err());
            assert!(store.apply_bundle_state(&BundleState::default()).is_err());
            // Close before reopening
            store.close().unwrap();
        }
    }

    /// T5.16: load_version clears poisoned state
    #[test]
    fn t5_16_load_version_clears_poisoned() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit block 1 successfully
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();
        assert_eq!(store.version(), 1);

        // Set failpoint for block 2
        store.set_fail_point(Some(CommitFailPoint::BeforePersist));
        store.apply_bundle_state(&BundleState::default()).unwrap();
        assert!(store.commit().is_err());
        assert!(store.poisoned);

        // Recover
        store.set_fail_point(None);
        store.load_version().unwrap();
        assert!(!store.poisoned);
        assert_eq!(store.version(), 1);
    }

    /// T5.17: rollback truncates future versions
    #[test]
    fn t5_17_rollback() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit 3 blocks
        for _ in 0..3 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 3);

        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);
        assert!(store.manifest.get_root(2).is_none());
        assert!(store.manifest.get_root(3).is_none());
    }

    /// T5.18: rollback then continue committing
    #[test]
    fn t5_18_rollback_then_commit() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        for _ in 0..3 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }

        store.rollback(1).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        let (ver, _) = store.commit().unwrap();
        assert_eq!(ver, 2);
    }

    /// T5.19: close releases writer lock, reopen succeeds
    #[test]
    fn t5_19_close_reopen() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.close().unwrap();
        // Reopen should succeed
        let _store2 = MptCommitStore::open(dir.path(), false).unwrap();
    }

    // ── Phase 4 parallel tests ──

    /// Helper: create a bundle with many accounts each having storage slots.
    fn make_storage_heavy_bundle(num_accounts: usize, slots_per_account: usize) -> BundleState {
        let mut accounts = Vec::new();
        for i in 0..num_accounts {
            let addr = Address::from_word(B256::from(U256::from(i + 1)));
            let info = default_info(1, 1000);
            let storage: Vec<(U256, U256, U256)> = (0..slots_per_account)
                .map(|s| (U256::from(s), U256::ZERO, U256::from(s + 1)))
                .collect();
            accounts.push((addr, Some(info), revm_database::AccountStatus::Changed, storage));
        }
        make_bundle(accounts)
    }

    /// T1.6: set_parallelism_thresholds() can override default values in tests.
    #[test]
    fn t1_6_set_parallelism_thresholds() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.parallelism, ParallelismThresholds::default());

        let custom = ParallelismThresholds { storage_tries_min: 1, account_frontier_min: 1 };
        store.set_parallelism_thresholds(custom);
        assert_eq!(store.parallelism, custom);
    }

    /// T3.1: open() initializes parallelism with Default values.
    #[test]
    fn t3_1_default_parallelism() {
        let dir = TempDir::new().unwrap();
        let store = MptCommitStore::open(dir.path(), false).unwrap();
        assert_eq!(store.parallelism, ParallelismThresholds::default());
    }

    /// T3.2: force storage_tries parallel path (threshold=1) -> root matches serial.
    #[test]
    fn t3_2_forced_parallel_storage_root_matches_serial() {
        let dir_serial = TempDir::new().unwrap();
        let dir_parallel = TempDir::new().unwrap();

        let bundle = make_storage_heavy_bundle(10, 5);

        // Serial path (high threshold)
        let mut store_s = MptCommitStore::open(dir_serial.path(), false).unwrap();
        store_s.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });
        store_s.apply_bundle_state(&bundle).unwrap();
        let (_, root_serial) = store_s.commit().unwrap();

        // Parallel path (threshold=1)
        let mut store_p = MptCommitStore::open(dir_parallel.path(), false).unwrap();
        store_p.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 1,
            account_frontier_min: 99999,
        });
        store_p.apply_bundle_state(&bundle).unwrap();
        let (_, root_parallel) = store_p.commit().unwrap();

        assert_eq!(root_serial, root_parallel);
    }

    /// T3.3: force storage_tries serial path (high threshold) -> root correct.
    #[test]
    fn t3_3_forced_serial_storage_root_correct() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(5, 3);

        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });
        store.apply_bundle_state(&bundle).unwrap();
        let (ver, root) = store.commit().unwrap();
        assert_eq!(ver, 1);
        assert_ne!(root, EMPTY_ROOT_HASH);
    }

    /// T3.4: force account_trie parallel hash path -> state_root matches serial.
    #[test]
    fn t3_4_forced_parallel_account_hash_matches_serial() {
        let dir_serial = TempDir::new().unwrap();
        let dir_parallel = TempDir::new().unwrap();

        // Many accounts to get a wide frontier
        let bundle = make_storage_heavy_bundle(50, 2);

        let mut store_s = MptCommitStore::open(dir_serial.path(), false).unwrap();
        store_s.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });
        store_s.apply_bundle_state(&bundle).unwrap();
        let (_, root_serial) = store_s.commit().unwrap();

        let mut store_p = MptCommitStore::open(dir_parallel.path(), false).unwrap();
        store_p.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 1,
        });
        store_p.apply_bundle_state(&bundle).unwrap();
        let (_, root_parallel) = store_p.commit().unwrap();

        assert_eq!(root_serial, root_parallel);
    }

    /// T3.5: same blocks, forced serial vs forced parallel -> identical results.
    #[test]
    fn t3_5_serial_vs_parallel_identical() {
        let dir_serial = TempDir::new().unwrap();
        let dir_parallel = TempDir::new().unwrap();

        let bundles: Vec<BundleState> =
            (0..3).map(|i| make_storage_heavy_bundle(10 + i * 5, 3)).collect();

        let mut store_s = MptCommitStore::open(dir_serial.path(), false).unwrap();
        store_s.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 99999,
            account_frontier_min: 99999,
        });

        let mut store_p = MptCommitStore::open(dir_parallel.path(), false).unwrap();
        store_p.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 1,
            account_frontier_min: 1,
        });

        for bundle in &bundles {
            store_s.apply_bundle_state(bundle).unwrap();
            let (vs, rs) = store_s.commit().unwrap();

            store_p.apply_bundle_state(bundle).unwrap();
            let (vp, rp) = store_p.commit().unwrap();

            assert_eq!(vs, vp);
            assert_eq!(rs, rp);
        }
    }

    /// T3.6: multi-account parallel commit + reopen/load_version -> consistent.
    #[test]
    fn t3_6_parallel_reopen_consistent() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(20, 4);

        let (version, root);
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.apply_bundle_state(&bundle).unwrap();
            let result = store.commit().unwrap();
            version = result.0;
            root = result.1;
            store.close().unwrap();
        }

        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            assert_eq!(store.version(), version);

            // Commit empty block: root should not change
            store.apply_bundle_state(&BundleState::default()).unwrap();
            let (_, root_after) = store.commit().unwrap();
            assert_eq!(root_after, root);
        }
    }

    /// T3.7: parallel commit artifacts are fully persisted; reopen/load_version root matches.
    #[test]
    fn t3_7_parallel_artifacts_persisted() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(15, 5);

        let root1;
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.apply_bundle_state(&bundle).unwrap();
            let (_, r) = store.commit().unwrap();
            root1 = r;
            store.close().unwrap();
        }

        {
            let store = MptCommitStore::open(dir.path(), false).unwrap();
            assert_eq!(store.version(), 1);
            // Verify the root from manifest matches what we committed
            let stored_root = store.manifest.get_root(1).unwrap();
            assert_eq!(stored_root, root1);
        }
    }

    /// T3.8: parallel commit with failpoints -> same behavior as serial.
    #[test]
    fn t3_8_parallel_failpoints() {
        for fp in [
            CommitFailPoint::BeforePersist,
            CommitFailPoint::AfterPersistBeforeManifest,
            CommitFailPoint::ManifestSave,
        ] {
            let dir = TempDir::new().unwrap();
            let bundle = make_storage_heavy_bundle(5, 2);

            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.set_fail_point(Some(fp));
            store.apply_bundle_state(&bundle).unwrap();
            let result = store.commit();
            assert!(result.is_err(), "expected error for failpoint {fp:?}");
            assert!(store.poisoned);
            store.close().unwrap();
        }
    }

    /// T3.9: after parallel commit success, dirty state is cleared.
    #[test]
    fn t3_9_parallel_clears_dirty_state() {
        let dir = TempDir::new().unwrap();
        let bundle = make_storage_heavy_bundle(10, 3);

        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_parallelism_thresholds(ParallelismThresholds {
            storage_tries_min: 1,
            account_frontier_min: 1,
        });
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        assert!(!store.applied_this_block);
        assert!(store.dirty_accounts.is_empty());
        assert!(store.storage_tries.is_empty());
    }

    /// T3.10: read_only / poisoned / rollback semantics unchanged by parallel path.
    #[test]
    fn t3_10_parallel_semantics_unchanged() {
        let dir = TempDir::new().unwrap();

        // read_only still rejects writes
        {
            let mut store = MptCommitStore::open(dir.path(), true).unwrap();
            assert!(store.apply_bundle_state(&BundleState::default()).is_err());
        }

        // poisoned still blocks operations
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            store.set_fail_point(Some(CommitFailPoint::BeforePersist));
            store.apply_bundle_state(&BundleState::default()).unwrap();
            assert!(store.commit().is_err());
            assert!(store.poisoned);
            assert!(store.commit().is_err());
            assert!(store.apply_bundle_state(&BundleState::default()).is_err());

            // load_version recovers
            store.set_fail_point(None);
            store.load_version().unwrap();
            assert!(!store.poisoned);
            store.close().unwrap();
        }

        // rollback works after parallel commits
        {
            let mut store = MptCommitStore::open(dir.path(), false).unwrap();
            store.set_parallelism_thresholds(ParallelismThresholds {
                storage_tries_min: 1,
                account_frontier_min: 1,
            });
            let bundle = make_storage_heavy_bundle(5, 2);
            store.apply_bundle_state(&bundle).unwrap();
            store.commit().unwrap();
            store.apply_bundle_state(&bundle).unwrap();
            store.commit().unwrap();
            assert_eq!(store.version(), 2);
            store.rollback(1).unwrap();
            assert_eq!(store.version(), 1);
            store.close().unwrap();
        }
    }
}
