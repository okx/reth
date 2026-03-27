use crate::evm_types::{all_evm_store_types, store_type_name, sub_db_config};
use crossbeam_channel::Receiver;
use mptdb_common::{
    config::{StateStoreConfig, WalConfig},
    error::{MptDbError, Result},
    evm_keys::{parse_evm_key, EvmKeyKind},
    path::get_changelog_path,
};
use mptdb_engine::mvcc::db::MvccDatabase;
use mptdb_proto::{ChangeSet, ChangelogEntry, KvPair};
use mptdb_traits::{iterator::DbIterator, ss::StateStore, types::SnapshotNode, wal::Wal};
use mptdb_wal::changelog::{new_changelog_wal, ChangelogWal};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const IMPORT_BUFFER_SIZE: usize = 10000;

struct AsyncCommitJob {
    barrier_only: bool,
    version: i64,
    changeset: ChangeSet,
    done: Option<crossbeam_channel::Sender<Result<()>>>,
}

/// Manages 5 independent MVCC sub-databases (one per EVM key type) and
/// implements [`StateStore`]. Key routing is handled via [`parse_evm_key`],
/// so callers pass raw EVM keys and the store dispatches to the correct sub-DB.
pub struct EVMStateStore {
    sub_dbs: HashMap<EvmKeyKind, Arc<MvccDatabase>>,
    wal: Mutex<Option<ChangelogWal>>,
    pending_changes_tx: Mutex<Option<crossbeam_channel::Sender<AsyncCommitJob>>>,
    worker_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    async_error: Arc<AtomicBool>,
    async_error_detail: Arc<Mutex<Option<String>>>,
}

impl EVMStateStore {
    fn wal_config(config: &StateStoreConfig) -> WalConfig {
        WalConfig {
            keep_recent: config.keep_recent.max(0) as u64,
            prune_interval: if config.prune_interval_seconds > 0 {
                Duration::from_secs(config.prune_interval_seconds as u64)
            } else {
                Duration::ZERO
            },
            write_buffer_size: config.async_write_buffer,
            write_batch_size: 64,
            fsync_enabled: false,
            deep_copy_enabled: false,
        }
    }

    fn current_async_error(detail: &Mutex<Option<String>>) -> MptDbError {
        MptDbError::Other(
            detail.lock().clone().unwrap_or_else(|| "evm async commit failed".to_string()),
        )
    }

    fn report_async_error(
        async_error: &AtomicBool,
        detail: &Mutex<Option<String>>,
        err: &MptDbError,
    ) {
        *detail.lock() = Some(format!("evm async commit failed: {err}"));
        async_error.store(true, Ordering::Relaxed);
    }

    fn check_async_error(&self) -> Result<()> {
        if self.async_error.load(Ordering::Relaxed) {
            Err(Self::current_async_error(&self.async_error_detail))
        } else {
            Ok(())
        }
    }

    /// Opens 5 MVCC sub-databases under `dir`, one per EVM store type.
    pub fn new(dir: &str, config: &StateStoreConfig) -> Result<Self> {
        let mut sub_dbs =
            HashMap::<EvmKeyKind, Arc<MvccDatabase>>::with_capacity(all_evm_store_types().len());

        for store_type in all_evm_store_types() {
            let sub_dir = Path::new(dir).join(store_type_name(store_type));
            let mut sub_config = sub_db_config(config);
            sub_config.db_directory = sub_dir.to_string_lossy().to_string();

            let db = Arc::new(MvccDatabase::open_db(&sub_config).map_err(|e| {
                MptDbError::Other(format!(
                    "failed to open EVM MVCC DB for {}: {}",
                    store_type_name(store_type),
                    e
                ))
            })?);
            sub_dbs.insert(store_type, db);
        }

        let changelog_path = get_changelog_path(Path::new(dir));
        let async_error = Arc::new(AtomicBool::new(false));
        let async_error_detail = Arc::new(Mutex::new(None));
        let store = Self {
            sub_dbs,
            wal: Mutex::new(None),
            pending_changes_tx: Mutex::new(None),
            worker_handle: Mutex::new(None),
            async_error,
            async_error_detail,
        };

        crate::recovery::recover_state_store(&changelog_path, &store)?;

        let wal = new_changelog_wal(Self::wal_config(config), &changelog_path)?;
        store.wal.lock().replace(wal);
        if config.async_write_buffer > 0 {
            store.init_async_coordinator(config.async_write_buffer)?;
        }

        Ok(store)
    }

    /// Parse an EVM key and return the routed sub-DB kind and stripped key.
    /// Returns `None` for empty keys.
    fn route_key(&self, key: &[u8]) -> Option<(EvmKeyKind, Vec<u8>)> {
        let (kind, stripped) = parse_evm_key(key);
        if kind == EvmKeyKind::Empty {
            return None;
        }
        Some((kind, stripped.to_vec()))
    }

    /// Groups changeset pairs by EVM key kind, stripping prefixes.
    fn group_by_key_kind(changeset: &ChangeSet) -> HashMap<EvmKeyKind, Vec<KvPair>> {
        let mut grouped = HashMap::<EvmKeyKind, Vec<KvPair>>::new();
        for pair in &changeset.pairs {
            let (kind, stripped) = parse_evm_key(&pair.key);
            if kind == EvmKeyKind::Empty {
                continue;
            }
            grouped.entry(kind).or_default().push(KvPair {
                key: stripped.to_vec(),
                value: pair.value.clone(),
                delete: pair.delete,
            });
        }
        grouped
    }

    fn init_async_coordinator(&self, buffer: usize) -> Result<()> {
        let (tx, rx) = crossbeam_channel::bounded(buffer);
        self.pending_changes_tx.lock().replace(tx);

        let sub_dbs = self.sub_dbs.clone();
        let async_error = Arc::clone(&self.async_error);
        let async_error_detail = Arc::clone(&self.async_error_detail);
        let handle = std::thread::Builder::new()
            .name("evm-ss-async-writer".to_string())
            .spawn(move || {
                for job in rx {
                    if async_error.load(Ordering::Relaxed) {
                        if let Some(done) = job.done {
                            let _ = done.send(Err(Self::current_async_error(&async_error_detail)));
                        }
                        continue;
                    }

                    let result = if job.barrier_only {
                        Ok(())
                    } else {
                        Self::apply_grouped_atomic_to_sub_dbs(
                            &sub_dbs,
                            job.version,
                            Self::group_by_key_kind(&job.changeset),
                        )
                    };
                    if let Err(ref e) = result {
                        Self::report_async_error(&async_error, &async_error_detail, e);
                    }
                    if let Some(done) = job.done {
                        let _ = done.send(result.map_err(|e| {
                            Self::report_async_error(&async_error, &async_error_detail, &e);
                            e
                        }));
                    }
                }
            })
            .map_err(|e| MptDbError::Other(format!("failed to spawn EVM async writer: {e}")))?;
        self.worker_handle.lock().replace(handle);
        Ok(())
    }

    fn apply_grouped_atomic_to_sub_dbs(
        sub_dbs: &HashMap<EvmKeyKind, Arc<MvccDatabase>>,
        version: i64,
        grouped: HashMap<EvmKeyKind, Vec<KvPair>>,
    ) -> Result<()> {
        if grouped.is_empty() {
            for db in sub_dbs.values() {
                db.set_latest_version(version)?;
            }
            return Ok(());
        }

        if grouped.len() == 1 {
            let (kind, pairs) = grouped.into_iter().next().unwrap();
            Self::apply_to_sub_db_atomic(sub_dbs, kind, version, pairs)?;
            for db in sub_dbs.values() {
                db.set_latest_version(version)?;
            }
            return Ok(());
        }

        let errors: Vec<MptDbError> = std::thread::scope(|s| {
            let handles: Vec<_> = grouped
                .into_iter()
                .map(|(kind, pairs)| {
                    s.spawn(move || Self::apply_to_sub_db_atomic(sub_dbs, kind, version, pairs))
                })
                .collect();

            handles.into_iter().filter_map(|h| h.join().ok().and_then(|r| r.err())).collect()
        });

        if let Some(first) = errors.into_iter().next() {
            return Err(first);
        }

        for db in sub_dbs.values() {
            db.set_latest_version(version)?;
        }

        Ok(())
    }

    fn apply_grouped_atomic(
        &self,
        version: i64,
        grouped: HashMap<EvmKeyKind, Vec<KvPair>>,
    ) -> Result<()> {
        Self::apply_grouped_atomic_to_sub_dbs(&self.sub_dbs, version, grouped)
    }

    fn enqueue_async_commit(&self, version: i64, changeset: &ChangeSet, wait: bool) -> Result<()> {
        let entry = ChangelogEntry { version, changeset: Some(changeset.clone()) };
        if let Some(wal) = self.wal.lock().as_ref() {
            wal.write(entry)?;
        }

        let tx = self.pending_changes_tx.lock().as_ref().cloned();
        match tx {
            Some(tx) => {
                let done = if wait {
                    let (done_tx, done_rx) = crossbeam_channel::bounded(0);
                    tx.send(AsyncCommitJob {
                        barrier_only: false,
                        version,
                        changeset: changeset.clone(),
                        done: Some(done_tx),
                    })
                    .map_err(|e| MptDbError::Other(format!("failed to send async commit: {e}")))?;
                    return match done_rx.recv() {
                        Ok(result) => result,
                        Err(_) => self.check_async_error(),
                    };
                } else {
                    None
                };

                tx.send(AsyncCommitJob {
                    barrier_only: false,
                    version,
                    changeset: changeset.clone(),
                    done,
                })
                .map_err(|e| MptDbError::Other(format!("failed to send async commit: {e}")))?;
                Ok(())
            }
            None => self.apply_grouped_atomic(version, Self::group_by_key_kind(changeset)),
        }
    }

    pub fn wait_for_pending_writes(&self) -> Result<()> {
        self.check_async_error()?;
        let tx = match self.pending_changes_tx.lock().as_ref().cloned() {
            Some(tx) => tx,
            None => return Ok(()),
        };
        let (done_tx, done_rx) = crossbeam_channel::bounded(0);
        tx.send(AsyncCommitJob {
            barrier_only: true,
            version: 0,
            changeset: ChangeSet { pairs: vec![] },
            done: Some(done_tx),
        })
        .map_err(|e| MptDbError::Other(format!("failed to send async barrier: {e}")))?;
        match done_rx.recv() {
            Ok(result) => result,
            Err(_) => self.check_async_error(),
        }
    }

    /// Apply pairs to a single sub-DB, constructing a ChangeSet from the pairs.
    fn apply_to_sub_db_atomic(
        sub_dbs: &HashMap<EvmKeyKind, Arc<MvccDatabase>>,
        kind: EvmKeyKind,
        version: i64,
        pairs: Vec<KvPair>,
    ) -> Result<()> {
        let db = match sub_dbs.get(&kind) {
            Some(db) => db,
            None => return Ok(()),
        };
        let cs = ChangeSet { pairs };
        db.apply_changeset_data_only(version, &cs)
    }
}

impl StateStore for EVMStateStore {
    fn get(&self, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.check_async_error()?;
        if version > self.get_latest_version() {
            return Ok(None);
        }
        let (kind, stripped) = match self.route_key(key) {
            Some(v) => v,
            None => return Ok(None),
        };
        let db = match self.sub_dbs.get(&kind) {
            Some(db) => db,
            None => return Ok(None),
        };
        db.get(version, &stripped)
    }

    fn has(&self, version: i64, key: &[u8]) -> Result<bool> {
        self.check_async_error()?;
        if version > self.get_latest_version() {
            return Ok(false);
        }
        let (kind, stripped) = match self.route_key(key) {
            Some(v) => v,
            None => return Ok(false),
        };
        let db = match self.sub_dbs.get(&kind) {
            Some(db) => db,
            None => return Ok(false),
        };
        db.has(version, &stripped)
    }

    fn iterator(&self, _version: i64, _start: &[u8], _end: &[u8]) -> Result<Box<dyn DbIterator>> {
        self.check_async_error()?;
        Err(MptDbError::Other("evm state store: cross-type iteration not supported".into()))
    }

    fn reverse_iterator(
        &self,
        _version: i64,
        _start: &[u8],
        _end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        self.check_async_error()?;
        Err(MptDbError::Other("evm state store: cross-type reverse iteration not supported".into()))
    }

    fn raw_iterate(&self, _f: &mut dyn FnMut(&[u8], &[u8], i64) -> bool) -> Result<bool> {
        self.check_async_error()?;
        Err(MptDbError::Other("evm state store: RawIterate not supported".into()))
    }

    fn get_latest_version(&self) -> i64 {
        let mut min_version: i64 = -1;
        for db in self.sub_dbs.values() {
            let v = db.get_latest_version();
            if min_version < 0 || v < min_version {
                min_version = v;
            }
        }
        if min_version < 0 {
            0
        } else {
            min_version
        }
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.check_async_error()?;
        for db in self.sub_dbs.values() {
            db.set_latest_version(version)?;
        }
        Ok(())
    }

    fn get_earliest_version(&self) -> i64 {
        let mut min_version: i64 = -1;
        for db in self.sub_dbs.values() {
            let v = db.get_earliest_version();
            if min_version < 0 || v < min_version {
                min_version = v;
            }
        }
        if min_version < 0 {
            0
        } else {
            min_version
        }
    }

    fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
        self.check_async_error()?;
        for db in self.sub_dbs.values() {
            db.set_earliest_version(version, ignore_version)?;
        }
        Ok(())
    }

    fn apply_changeset_sync(&self, version: i64, changeset: &ChangeSet) -> Result<()> {
        self.check_async_error()?;
        self.enqueue_async_commit(version, changeset, true)
    }

    fn apply_changeset_async(&self, version: i64, changeset: &ChangeSet) -> Result<()> {
        self.check_async_error()?;
        self.enqueue_async_commit(version, changeset, false)
    }

    fn prune(&self, version: i64) -> Result<()> {
        self.check_async_error()?;
        self.wait_for_pending_writes()?;
        if self.sub_dbs.is_empty() {
            return Ok(());
        }

        let errors: Vec<MptDbError> = std::thread::scope(|s| {
            let handles: Vec<_> =
                self.sub_dbs.values().map(|db| s.spawn(move || db.prune(version))).collect();

            handles.into_iter().filter_map(|h| h.join().ok().and_then(|r| r.err())).collect()
        });

        if let Some(first) = errors.into_iter().next() {
            return Err(first);
        }
        Ok(())
    }

    fn import(&self, version: i64, nodes: Receiver<SnapshotNode>) -> Result<()> {
        self.check_async_error()?;
        self.wait_for_pending_writes()?;
        let mut grouped = HashMap::<EvmKeyKind, Vec<KvPair>>::new();
        let mut pending = 0;

        for node in nodes.iter() {
            let (kind, stripped) = parse_evm_key(&node.key);
            if kind == EvmKeyKind::Empty {
                continue;
            }
            grouped.entry(kind).or_default().push(KvPair {
                key: stripped.to_vec(),
                value: node.value,
                delete: false,
            });
            pending += 1;

            if pending >= IMPORT_BUFFER_SIZE {
                self.apply_grouped_atomic(version, std::mem::take(&mut grouped))?;
                pending = 0;
            }
        }

        // Flush remaining
        if !grouped.is_empty() {
            self.apply_grouped_atomic(version, grouped)?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let pending_tx = self.pending_changes_tx.lock().take();
        drop(pending_tx);
        if let Some(handle) = self.worker_handle.lock().take() {
            handle
                .join()
                .map_err(|_| MptDbError::Other("evm async writer panicked".to_string()))?;
        }
        if let Some(ref mut wal) = *self.wal.lock() {
            wal.close()?;
        }
        let mut last_err: Option<MptDbError> = None;
        for db in self.sub_dbs.values() {
            if let Err(e) = db.shutdown() {
                last_err = Some(e);
            }
        }
        self.check_async_error()?;
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl EVMStateStore {
    // ── Version availability ───────────────────────────────────────────────

    /// Returns `true` if `version` is within SS's retained range
    /// `[earliest_version, latest_version]`.
    ///
    /// A `false` result means data at that version has been pruned or SS has
    /// not yet received any writes.  Callers should fall back to MDBX or
    /// return a clear error when `false`.
    pub fn is_version_available(&self, version: i64) -> bool {
        use mptdb_traits::ss::StateStore as _;
        let earliest = self.get_earliest_version();
        let latest = self.get_latest_version();
        // latest == 0 with no writes yet → treat as unavailable.
        latest > 0 && version >= earliest && version <= latest
    }

    // ── Typed read interfaces for reth StateProvider integration ──────────

    /// Encode `(nonce, balance, code_hash)` into the 72-byte Account value format.
    ///
    /// Layout (fixed-size, no length prefix):
    ///   [0..8]   nonce:     u64 big-endian
    ///   [8..40]  balance:   U256 big-endian (32 bytes)
    ///   [40..72] code_hash: B256 (32 bytes)
    pub fn encode_account_value(nonce: u64, balance: [u8; 32], code_hash: [u8; 32]) -> [u8; 72] {
        let mut buf = [0u8; 72];
        buf[..8].copy_from_slice(&nonce.to_be_bytes());
        buf[8..40].copy_from_slice(&balance);
        buf[40..72].copy_from_slice(&code_hash);
        buf
    }

    /// Decode the 72-byte Account value into `(nonce, balance_bytes, code_hash_bytes)`.
    pub fn decode_account_value(raw: &[u8]) -> Option<(u64, [u8; 32], [u8; 32])> {
        if raw.len() != 72 {
            return None;
        }
        let nonce = u64::from_be_bytes(raw[..8].try_into().ok()?);
        let mut balance = [0u8; 32];
        balance.copy_from_slice(&raw[8..40]);
        let mut code_hash = [0u8; 32];
        code_hash.copy_from_slice(&raw[40..72]);
        Some((nonce, balance, code_hash))
    }

    /// Read the combined `(nonce, balance, code_hash)` for `address` at `version`.
    ///
    /// Returns `None` if the account does not exist or the version is out of range.
    /// `address` is the raw 20-byte Ethereum address (not keccak-hashed).
    pub fn get_account(
        &self,
        version: i64,
        address: &[u8; 20],
    ) -> Result<Option<(u64, [u8; 32], [u8; 32])>> {
        use mptdb_common::evm_keys::{make_account_key, EvmKeyKind};
        let raw_key = make_account_key(address);
        let db = match self.sub_dbs.get(&EvmKeyKind::Account) {
            Some(db) => db,
            None => return Ok(None),
        };
        match db.get(version, &raw_key[1..])? {
            None => Ok(None),
            Some(raw) => Ok(Self::decode_account_value(&raw)),
        }
    }

    /// Read the storage value for `(address, slot)` at `version`.
    ///
    /// Returns `None` if the slot is zero / not set.
    /// Both `address` (20 bytes) and `slot` (32 bytes) are raw, not keccak-hashed.
    pub fn get_storage(
        &self,
        version: i64,
        address: &[u8; 20],
        slot: &[u8; 32],
    ) -> Result<Option<Vec<u8>>> {
        use mptdb_common::evm_keys::{make_storage_key, EvmKeyKind};
        let raw_key = make_storage_key(address, slot);
        let db = match self.sub_dbs.get(&EvmKeyKind::Storage) {
            Some(db) => db,
            None => return Ok(None),
        };
        // Stripped key = raw_key without the 0x03 prefix
        db.get(version, &raw_key[1..])
    }
}

impl Drop for EVMStateStore {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_common::evm_keys::{
        CODE_HASH_KEY_PREFIX, CODE_KEY_PREFIX, NONCE_KEY_PREFIX, STATE_KEY_PREFIX,
    };
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    fn make_nonce_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![NONCE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn make_codehash_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![CODE_HASH_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn make_code_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![CODE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn make_storage_key(addr: &[u8; 20], slot: &[u8; 32]) -> Vec<u8> {
        let mut key = vec![STATE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key.extend_from_slice(slot);
        key
    }

    fn make_legacy_key(data: &[u8]) -> Vec<u8> {
        // Use a prefix byte that doesn't match any known EVM prefix
        let mut key = vec![0x01];
        key.extend_from_slice(data);
        key
    }

    fn test_addr() -> [u8; 20] {
        let mut addr = [0u8; 20];
        for (i, b) in addr.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        addr
    }

    fn test_slot() -> [u8; 32] {
        let mut slot = [0u8; 32];
        for (i, b) in slot.iter_mut().enumerate() {
            *b = (0xa0 + i) as u8;
        }
        slot
    }

    fn make_evm_changeset(pairs: Vec<(Vec<u8>, Option<&[u8]>)>) -> ChangeSet {
        ChangeSet {
            pairs: pairs
                .into_iter()
                .map(|(k, v)| KvPair {
                    delete: v.is_none(),
                    key: k,
                    value: v.unwrap_or_default().to_vec(),
                })
                .collect(),
        }
    }

    fn open_evm_store(dir: &std::path::Path) -> EVMStateStore {
        let cfg = test_config(dir);
        EVMStateStore::new(&dir.to_string_lossy(), &cfg).unwrap()
    }

    #[test]
    fn test_evm_store_get_has() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"42"))]);
        store.apply_changeset_sync(1, &cs).unwrap();

        let val = store.get(1, &nonce_key).unwrap();
        assert_eq!(val, Some(b"42".to_vec()));

        assert!(store.has(1, &nonce_key).unwrap());

        // Non-existent key
        let addr2 = [0xffu8; 20];
        let missing_key = make_nonce_key(&addr2);
        assert_eq!(store.get(1, &missing_key).unwrap(), None);
        assert!(!store.has(1, &missing_key).unwrap());
    }

    #[test]
    fn test_evm_store_all_sub_dbs() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let slot = test_slot();

        let nonce_key = make_nonce_key(&addr);
        let codehash_key = make_codehash_key(&addr);
        let code_key = make_code_key(&addr);
        let storage_key = make_storage_key(&addr, &slot);
        let legacy_key = make_legacy_key(b"some_mapping");

        let cs = make_evm_changeset(vec![
            (nonce_key.clone(), Some(b"1")),
            (codehash_key.clone(), Some(b"hash_val")),
            (code_key.clone(), Some(b"bytecode")),
            (storage_key.clone(), Some(b"slot_val")),
            (legacy_key.clone(), Some(b"legacy_val")),
        ]);
        store.apply_changeset_sync(1, &cs).unwrap();

        assert_eq!(store.get(1, &nonce_key).unwrap(), Some(b"1".to_vec()));
        assert_eq!(store.get(1, &codehash_key).unwrap(), Some(b"hash_val".to_vec()));
        assert_eq!(store.get(1, &code_key).unwrap(), Some(b"bytecode".to_vec()));
        assert_eq!(store.get(1, &storage_key).unwrap(), Some(b"slot_val".to_vec()));
        assert_eq!(store.get(1, &legacy_key).unwrap(), Some(b"legacy_val".to_vec()));
    }

    #[test]
    fn test_evm_store_version_tracking() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());

        assert_eq!(store.get_latest_version(), 0);
        assert_eq!(store.get_earliest_version(), 0);

        store.set_latest_version(10).unwrap();
        assert_eq!(store.get_latest_version(), 10);

        store.set_earliest_version(5, false).unwrap();
        assert_eq!(store.get_earliest_version(), 5);
    }

    #[test]
    fn test_evm_store_delete_tombstone() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write
        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"42"))]);
        store.apply_changeset_sync(1, &cs).unwrap();
        assert!(store.has(1, &nonce_key).unwrap());

        // Delete (tombstone)
        let cs = make_evm_changeset(vec![(nonce_key.clone(), None)]);
        store.apply_changeset_sync(2, &cs).unwrap();
        assert!(!store.has(2, &nonce_key).unwrap());
        assert_eq!(store.get(2, &nonce_key).unwrap(), None);
    }

    #[test]
    fn test_evm_store_prune() {
        // EVM sub-DBs normally use the default comparer. The MVCC prune seek
        // optimisation requires the custom comparator, so we open with
        // use_default_comparer=false here to exercise the prune-delegation
        // logic without hitting the seek issue.
        let dir = tempdir().unwrap();
        let cfg = StateStoreConfig {
            db_directory: dir.path().to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        };
        // Open sub-DBs with the MVCC comparator for correct prune behaviour.
        let mut sub_dbs =
            HashMap::<EvmKeyKind, Arc<MvccDatabase>>::with_capacity(all_evm_store_types().len());
        for store_type in all_evm_store_types() {
            let sub_dir = std::path::Path::new(&cfg.db_directory).join(store_type_name(store_type));
            let mut sub_cfg = cfg.clone();
            sub_cfg.db_directory = sub_dir.to_string_lossy().to_string();
            sub_cfg.use_default_comparer = false;
            let db = Arc::new(MvccDatabase::open_db(&sub_cfg).unwrap());
            sub_dbs.insert(store_type, db);
        }
        let store = EVMStateStore {
            sub_dbs,
            wal: Mutex::new(None),
            pending_changes_tx: Mutex::new(None),
            worker_handle: Mutex::new(None),
            async_error: Arc::new(AtomicBool::new(false)),
            async_error_detail: Arc::new(Mutex::new(None)),
        };

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write version 1 and 2
        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"v1"))]);
        store.apply_changeset_sync(1, &cs).unwrap();
        let cs = make_evm_changeset(vec![(nonce_key.clone(), Some(b"v2"))]);
        store.apply_changeset_sync(2, &cs).unwrap();

        // Prune version 1
        store.prune(1).unwrap();

        // Version 2 should still be available
        assert_eq!(store.get(2, &nonce_key).unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_evm_store_iterator_not_supported() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());

        assert!(store.iterator(1, b"a", b"z").is_err());
        assert!(store.reverse_iterator(1, b"a", b"z").is_err());
        assert!(store.raw_iterate(&mut |_, _, _| true).is_err());
    }

    #[test]
    fn test_evm_store_parallel_apply() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let slot = test_slot();

        // Build a changeset touching multiple sub-DB types
        let nonce_key = make_nonce_key(&addr);
        let storage_key = make_storage_key(&addr, &slot);
        let code_key = make_code_key(&addr);

        let cs = make_evm_changeset(vec![
            (nonce_key.clone(), Some(b"10")),
            (storage_key.clone(), Some(b"slot_data")),
            (code_key.clone(), Some(b"0xdeadbeef")),
        ]);
        store.apply_changeset_sync(1, &cs).unwrap();

        assert_eq!(store.get(1, &nonce_key).unwrap(), Some(b"10".to_vec()));
        assert_eq!(store.get(1, &storage_key).unwrap(), Some(b"slot_data".to_vec()));
        assert_eq!(store.get(1, &code_key).unwrap(), Some(b"0xdeadbeef".to_vec()));
    }

    #[test]
    fn test_evm_store_async_apply_uses_initialized_workers() {
        let dir = tempdir().unwrap();
        let store = open_evm_store(dir.path());
        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);
        let code_key = make_code_key(&addr);

        let cs = make_evm_changeset(vec![
            (nonce_key.clone(), Some(b"7")),
            (code_key.clone(), Some(b"0xcafe")),
        ]);
        store.apply_changeset_async(1, &cs).unwrap();
        store.wait_for_pending_writes().unwrap();

        assert_eq!(store.get(1, &nonce_key).unwrap(), Some(b"7".to_vec()));
        assert_eq!(store.get(1, &code_key).unwrap(), Some(b"0xcafe".to_vec()));
    }

    #[test]
    fn test_evm_store_close() {
        let dir = tempdir().unwrap();
        let mut store = open_evm_store(dir.path());
        store.close().unwrap();
    }

    // ── Account key (0x0b) acceptance tests ─────────────────────────────────

    fn make_account_changeset(
        addr: &[u8; 20],
        nonce: u64,
        balance: [u8; 32],
        code_hash: [u8; 32],
    ) -> mptdb_proto::ChangeSet {
        use mptdb_common::evm_keys::{make_account_key, ACCOUNT_KEY_PREFIX as _};
        let key = make_account_key(addr);
        let value = EVMStateStore::encode_account_value(nonce, balance, code_hash);
        mptdb_proto::ChangeSet {
            pairs: vec![mptdb_proto::KvPair {
                delete: false,
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        }
    }

    fn delete_account_changeset(addr: &[u8; 20]) -> mptdb_proto::ChangeSet {
        use mptdb_common::evm_keys::make_account_key;
        let key = make_account_key(addr);
        mptdb_proto::ChangeSet {
            pairs: vec![mptdb_proto::KvPair { delete: true, key: key.to_vec(), value: vec![] }],
        }
    }

    fn delete_storage_changeset(addr: &[u8; 20], slot: &[u8; 32]) -> mptdb_proto::ChangeSet {
        use mptdb_common::evm_keys::make_storage_key;
        let key = make_storage_key(addr, slot);
        mptdb_proto::ChangeSet {
            pairs: vec![mptdb_proto::KvPair { delete: true, key: key.to_vec(), value: vec![] }],
        }
    }

    /// get_account returns correct (nonce, balance, code_hash) after write.
    #[test]
    fn account_key_write_read() {
        let dir = tempdir().unwrap();
        let mut store = open_evm_store(dir.path());
        let addr = test_addr();
        let balance = {
            let mut b = [0u8; 32];
            b[31] = 42;
            b
        };
        let code_hash = [0xabu8; 32];

        let cs = make_account_changeset(&addr, 7, balance, code_hash);
        store.apply_changeset_sync(1, &cs).unwrap();

        let result = store.get_account(1, &addr).unwrap();
        let (nonce, bal, ch) = result.expect("account must exist");
        assert_eq!(nonce, 7);
        assert_eq!(bal, balance);
        assert_eq!(ch, code_hash);
    }

    /// After selfdestruct (delete Account key), get_account returns None.
    #[test]
    fn account_key_selfdestruct_returns_none() {
        let dir = tempdir().unwrap();
        let mut store = open_evm_store(dir.path());
        let addr = test_addr();
        let cs = make_account_changeset(&addr, 1, [0u8; 32], [0u8; 32]);
        store.apply_changeset_sync(1, &cs).unwrap();
        assert!(store.get_account(1, &addr).unwrap().is_some());

        let del = delete_account_changeset(&addr);
        store.apply_changeset_sync(2, &del).unwrap();
        assert!(store.get_account(2, &addr).unwrap().is_none(), "deleted account must return None");
    }

    /// After clearing a storage slot (delete Storage key), get_storage returns None.
    #[test]
    fn storage_key_clear_returns_none() {
        let dir = tempdir().unwrap();
        let mut store = open_evm_store(dir.path());
        let addr = test_addr();
        let slot = [0x01u8; 32];

        // Write a non-zero storage value
        let write_cs = mptdb_proto::ChangeSet {
            pairs: vec![mptdb_proto::KvPair {
                delete: false,
                key: {
                    use mptdb_common::evm_keys::make_storage_key;
                    make_storage_key(&addr, &slot).to_vec()
                },
                value: vec![0u8; 31].into_iter().chain([99u8]).collect(),
            }],
        };
        store.apply_changeset_sync(1, &write_cs).unwrap();
        assert!(store.get_storage(1, &addr, &slot).unwrap().is_some());

        // Clear (tombstone) the slot
        let del = delete_storage_changeset(&addr, &slot);
        store.apply_changeset_sync(2, &del).unwrap();
        assert!(
            store.get_storage(2, &addr, &slot).unwrap().is_none(),
            "cleared storage slot must return None"
        );
    }

    /// encode_account_value / decode_account_value round-trip.
    #[test]
    fn account_value_codec_roundtrip() {
        let nonce = 0xDEAD_BEEF_u64;
        let mut balance = [0u8; 32];
        balance[0] = 0xFF;
        balance[31] = 0x01;
        let code_hash = [0x42u8; 32];

        let encoded = EVMStateStore::encode_account_value(nonce, balance, code_hash);
        assert_eq!(encoded.len(), 72);

        let (n, b, c) = EVMStateStore::decode_account_value(&encoded).unwrap();
        assert_eq!(n, nonce);
        assert_eq!(b, balance);
        assert_eq!(c, code_hash);
    }
}
