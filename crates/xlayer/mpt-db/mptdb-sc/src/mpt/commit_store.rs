use alloy_primitives::{Address, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::{Nibbles, EMPTY_ROOT_HASH};
use fs4::fs_std::FileExt;
use mptdb_common::error::{MptDbError, Result};
use parking_lot::Mutex;
use rayon::prelude::*;
use reth_trie_common::AccountProof;
use revm_database::BundleState;
use schnellru::{ByLength, LruMap};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use super::{
    config::MptConfig,
    gc,
    manifest::VersionManifest,
    parallel::ParallelismThresholds,
    persisted::{self, PersistedTrieStore},
    proof,
    r#trait::{CommitFrontier, MptCommitter, MptGcStats, MptSnapshotExporter, MptSnapshotImporter},
    snapshot::{SnapshotExporter, SnapshotImporter},
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
    /// The trie after root computation, returned for LRU caching.
    trie: MptTree,
}

/// A persist job sent to the background worker thread.
struct PersistJob {
    barrier_only: bool,
    blobs: Vec<(B256, Vec<u8>)>,
    manifest: VersionManifest,
    manifest_path: PathBuf,
    /// The version this persist job makes durable. Used to update `durable_version`.
    version: i64,
    /// If set, the background worker signals completion on this channel after
    /// finishing the persist. Used by `flush_persist()` to wait for drain.
    done: Option<crossbeam_channel::Sender<Result<()>>>,
}

struct StorageTrieCache {
    inner: LruMap<B256, MptTree, ByLength>,
    capacity: usize,
}

impl StorageTrieCache {
    fn new(capacity: usize) -> Self {
        let limit = capacity.max(1) as u32;
        Self { inner: LruMap::new(ByLength::new(limit)), capacity }
    }

    fn remove(&mut self, key: &B256) -> Option<MptTree> {
        self.inner.remove(key)
    }

    fn insert(&mut self, key: B256, trie: MptTree) {
        if self.capacity == 0 {
            return;
        }
        self.inner.remove(&key);
        let _ = self.inner.get_or_insert(key, || trie);
    }

    fn contains_key(&self, key: &B256) -> bool {
        self.inner.peek(key).is_some()
    }

    fn clear(&mut self) {
        self.inner.clear();
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CommitProfile {
    pub apply_bundle_state: Duration,
    pub apply_collect_dirty_accounts: Duration,
    pub apply_get_or_load_storage_tries: Duration,
    pub apply_storage_slot_updates: Duration,
    pub storage_roots: Duration,
    pub account_updates: Duration,
    pub account_root_and_blobs: Duration,
    pub persist_and_manifest: Duration,
    pub cache_publish: Duration,
    pub total_commit: Duration,
}

#[derive(Serialize, Deserialize)]
struct AccountTrieCheckpoint {
    version: i64,
    root: B256,
    trie: MptTree,
}

/// MPT-based commit store with persistence, rollback, and recovery support.
pub struct MptCommitStore {
    #[allow(dead_code)]
    dir: PathBuf,
    manifest_path: PathBuf,

    account_trie: MptTree,
    /// Per-account storage tries (hashed_address -> storage trie) for the current block.
    storage_tries: HashMap<B256, MptTree>,
    /// Cross-block LRU cache for storage tries. After commit, clean tries are moved here
    /// so the next block can reuse them without reloading from RocksDB.
    storage_trie_cache: StorageTrieCache,
    dirty_accounts: Vec<DirtyAccount>,

    persisted: Arc<PersistedTrieStore>,
    manifest: VersionManifest,

    version: i64,
    applied_this_block: bool,
    poisoned: bool,
    read_only: bool,
    file_lock: Option<File>,

    parallelism: ParallelismThresholds,
    config: MptConfig,

    /// Latest version whose nodes and manifest are confirmed on stable storage.
    durable_version: Arc<AtomicI64>,

    /// Channel to send persist jobs to the background worker.
    persist_tx: Option<crossbeam_channel::Sender<PersistJob>>,
    /// Handle to the background persist worker thread.
    persist_handle: Option<JoinHandle<()>>,
    async_error: Arc<AtomicBool>,
    async_error_detail: Arc<Mutex<Option<String>>>,
    last_apply_duration: Duration,
    last_apply_collect_dirty_accounts: Duration,
    last_apply_get_or_load_storage_tries: Duration,
    last_apply_storage_slot_updates: Duration,
    last_commit_profile: CommitProfile,
    #[cfg(test)]
    loaded_from_checkpoint: bool,

    #[cfg(test)]
    fail_point: Option<CommitFailPoint>,
    #[cfg(test)]
    async_fail_mode: Arc<std::sync::atomic::AtomicU8>,
}

impl MptCommitStore {
    fn current_async_error(detail: &Mutex<Option<String>>) -> MptDbError {
        MptDbError::Other(
            detail.lock().clone().unwrap_or_else(|| "mpt async persist failed".to_string()),
        )
    }

    fn report_async_error(
        async_error: &AtomicBool,
        detail: &Mutex<Option<String>>,
        err: &MptDbError,
    ) {
        *detail.lock() = Some(format!("mpt async persist failed: {err}"));
        async_error.store(true, Ordering::Relaxed);
    }

    fn check_async_error(&self) -> Result<()> {
        if self.async_error.load(Ordering::Relaxed) {
            Err(Self::current_async_error(&self.async_error_detail))
        } else {
            Ok(())
        }
    }

    fn checkpoint_path(dir: &Path) -> PathBuf {
        dir.join("account_trie_checkpoint.bin")
    }

    fn try_load_checkpoint(dir: &Path, version: i64, root: B256) -> Result<Option<MptTree>> {
        let path = Self::checkpoint_path(dir);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(None),
        };
        let checkpoint: AccountTrieCheckpoint = match bincode::deserialize(&bytes) {
            Ok(cp) => cp,
            Err(_) => return Ok(None),
        };
        if checkpoint.version != version || checkpoint.root != root {
            return Ok(None);
        }
        Ok(Some(checkpoint.trie))
    }

    fn save_checkpoint(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        if self.applied_this_block {
            return Ok(());
        }
        let root = self.manifest.get_root(self.version).unwrap_or(EMPTY_ROOT_HASH);
        let checkpoint =
            AccountTrieCheckpoint { version: self.version, root, trie: self.account_trie.clone() };
        let bytes = bincode::serialize(&checkpoint)
            .map_err(|e| MptDbError::Other(format!("serialize account trie checkpoint: {e}")))?;
        let path = Self::checkpoint_path(&self.dir);
        let tmp = path.with_extension("bin.tmp");
        fs::write(&tmp, bytes)
            .map_err(|e| MptDbError::Other(format!("write account trie checkpoint tmp: {e}")))?;
        fs::rename(&tmp, &path)
            .map_err(|e| MptDbError::Other(format!("rename account trie checkpoint: {e}")))?;
        Ok(())
    }

    fn shutdown(&mut self, best_effort: bool) -> Result<()> {
        if best_effort {
            let _ = self.flush_persist();
            let _ = self.save_checkpoint();
        } else {
            self.flush_persist()?;
            self.save_checkpoint()?;
        }

        self.persist_tx.take();
        if let Some(handle) = self.persist_handle.take() {
            if best_effort {
                let _ = handle.join();
            } else {
                handle
                    .join()
                    .map_err(|_| MptDbError::Other("persist worker panicked".to_string()))?;
            }
        }

        if let Some(persisted) = Arc::get_mut(&mut self.persisted) {
            if best_effort {
                let _ = persisted.close();
            } else {
                persisted.close()?;
            }
        }

        self.file_lock = None;

        if best_effort {
            Ok(())
        } else {
            self.check_async_error()
        }
    }

    fn reload_account_trie(&mut self) -> Result<()> {
        let root = self.manifest.get_root(self.manifest.latest_version).unwrap_or(EMPTY_ROOT_HASH);
        self.account_trie =
            match Self::try_load_checkpoint(&self.dir, self.manifest.latest_version, root)? {
                Some(trie) => trie,
                None => persisted::load_tree_from_root(&self.persisted, root)?,
            };
        Ok(())
    }

    /// Open an MptCommitStore at the given directory with default configuration.
    ///
    /// `read_only=true` disables writes and does not acquire the exclusive lock.
    pub fn open(dir: &Path, read_only: bool) -> Result<Self> {
        Self::open_with_config(dir, read_only, MptConfig::default())
    }

    /// Open an MptCommitStore at the given directory with custom configuration.
    ///
    /// `read_only=true` disables writes and does not acquire the exclusive lock.
    pub fn open_with_config(dir: &Path, read_only: bool, config: MptConfig) -> Result<Self> {
        // Ensure directories exist
        fs::create_dir_all(dir)
            .map_err(|e| MptDbError::Other(format!("create dir {}: {e}", dir.display())))?;
        let trie_nodes_dir = dir.join("trie_nodes");
        fs::create_dir_all(&trie_nodes_dir)
            .map_err(|e| MptDbError::Other(format!("create trie_nodes dir: {e}")))?;

        // Lock: exclusive for writer, shared for reader
        let file_lock = {
            let lock_path = dir.join("LOCK");
            let lock_file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .map_err(|e| MptDbError::Other(format!("open LOCK file: {e}")))?;
            if read_only {
                lock_file.try_lock_shared().map_err(|e| {
                    MptDbError::Other(format!("failed to acquire shared lock: {e}"))
                })?;
            } else {
                lock_file
                    .try_lock_exclusive()
                    .map_err(|e| MptDbError::Other(format!("failed to lock db: {e}")))?;
            }
            Some(lock_file)
        };

        let manifest_path = dir.join("manifest.json");
        let manifest = VersionManifest::load(&manifest_path)?;

        let persisted = Arc::new(PersistedTrieStore::open_with_capacity(
            &trie_nodes_dir,
            config.persisted_node_cache_capacity,
        )?);

        let root = manifest.get_root(manifest.latest_version).unwrap_or(EMPTY_ROOT_HASH);
        #[cfg(test)]
        let (account_trie, loaded_from_checkpoint) =
            match Self::try_load_checkpoint(dir, manifest.latest_version, root)? {
                Some(trie) => (trie, true),
                None => (persisted::load_tree_from_root(&persisted, root)?, false),
            };
        #[cfg(not(test))]
        let account_trie = match Self::try_load_checkpoint(dir, manifest.latest_version, root)? {
            Some(trie) => trie,
            None => persisted::load_tree_from_root(&persisted, root)?,
        };

        let version = manifest.latest_version;

        let parallelism = ParallelismThresholds {
            storage_tries_min: config.parallel_storage_tries_min,
            account_frontier_min: config.parallel_account_frontier_min,
        };

        let async_error = Arc::new(AtomicBool::new(false));
        let async_error_detail = Arc::new(Mutex::new(None));
        let durable_version = Arc::new(AtomicI64::new(version));
        #[cfg(test)]
        let async_fail_mode = Arc::new(std::sync::atomic::AtomicU8::new(0));

        // Spawn background persist worker for writable stores.
        // The bounded channel provides natural backpressure: once it fills,
        // commit() will block on enqueue until the worker catches up.
        let (persist_tx, persist_handle) = if !read_only {
            let (tx, rx) = crossbeam_channel::bounded::<PersistJob>(config.async_queue_depth);
            let persisted_clone = Arc::clone(&persisted);
            let async_error_clone = Arc::clone(&async_error);
            let async_error_detail_clone = Arc::clone(&async_error_detail);
            let durable_version_clone = Arc::clone(&durable_version);
            #[cfg(test)]
            let async_fail_mode_clone = Arc::clone(&async_fail_mode);
            let handle = std::thread::Builder::new()
                .name("mpt-persist".to_string())
                .spawn(move || {
                    for job in rx {
                        if async_error_clone.load(Ordering::Relaxed) {
                            if let Some(done) = job.done {
                                let _ = done.send(Err(Self::current_async_error(
                                    &async_error_detail_clone,
                                )));
                            }
                            continue;
                        }

                        if !job.barrier_only {
                            #[cfg(test)]
                            let forced_error = match async_fail_mode_clone.load(Ordering::Relaxed) {
                                1 => Some(MptDbError::Other(
                                    "forced async persist failure".to_string(),
                                )),
                                2 => Some(MptDbError::Other(
                                    "forced async manifest failure".to_string(),
                                )),
                                _ => None,
                            };

                            #[cfg(not(test))]
                            let forced_error: Option<MptDbError> = None;

                            let result = if let Some(err) = forced_error {
                                Err(err)
                            } else {
                                persisted_clone
                                    .persist_batch(&job.blobs, true)
                                    .and_then(|_| job.manifest.save(&job.manifest_path))
                            };

                            if let Err(e) = result {
                                Self::report_async_error(
                                    &async_error_clone,
                                    &async_error_detail_clone,
                                    &e,
                                );
                                tracing::error!(?e, "background persist failed");
                            } else {
                                // Update durable_version via CAS (only advance forward)
                                let _ = durable_version_clone.fetch_update(
                                    Ordering::Release,
                                    Ordering::Relaxed,
                                    |cur| {
                                        if job.version > cur {
                                            Some(job.version)
                                        } else {
                                            None
                                        }
                                    },
                                );
                            }
                        }

                        if let Some(done) = job.done {
                            let result = if async_error_clone.load(Ordering::Relaxed) {
                                Err(Self::current_async_error(&async_error_detail_clone))
                            } else {
                                Ok(())
                            };
                            let _ = done.send(result);
                        }
                    }
                })
                .map_err(|e| MptDbError::Other(format!("spawn persist thread: {e}")))?;
            (Some(tx), Some(handle))
        } else {
            (None, None)
        };

        Ok(Self {
            dir: dir.to_path_buf(),
            manifest_path,
            account_trie,
            storage_tries: HashMap::new(),
            storage_trie_cache: StorageTrieCache::new(config.storage_trie_cache_capacity),
            dirty_accounts: Vec::new(),
            persisted,
            manifest,
            version,
            applied_this_block: false,
            poisoned: false,
            read_only,
            file_lock,
            parallelism,
            config,
            durable_version,
            persist_tx,
            persist_handle,
            async_error,
            async_error_detail,
            last_apply_duration: Duration::ZERO,
            last_apply_collect_dirty_accounts: Duration::ZERO,
            last_apply_get_or_load_storage_tries: Duration::ZERO,
            last_apply_storage_slot_updates: Duration::ZERO,
            last_commit_profile: CommitProfile::default(),
            #[cfg(test)]
            loaded_from_checkpoint,
            #[cfg(test)]
            fail_point: None,
            #[cfg(test)]
            async_fail_mode,
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
    ///
    /// Lookup order: working set -> cross-block cache -> RocksDB.
    /// When falling through to RocksDB, flushes any in-flight background
    /// persist jobs first to ensure the latest nodes are on disk.
    fn get_or_load_storage_trie(
        &mut self,
        hashed_address: &B256,
        existing_root: B256,
    ) -> Result<()> {
        if self.storage_tries.contains_key(hashed_address) {
            return Ok(());
        }
        // Fresh accounts and empty storage roots do not need a disk lookup.
        // Materializing an empty trie in memory is equivalent and avoids
        // forcing a persist barrier on the first touched block.
        if existing_root == EMPTY_ROOT_HASH {
            self.storage_tries.insert(*hashed_address, MptTree::new());
            return Ok(());
        }
        // Check cross-block cache before hitting RocksDB
        if let Some(cached_trie) = self.storage_trie_cache.remove(hashed_address) {
            self.storage_tries.insert(*hashed_address, cached_trie);
            return Ok(());
        }
        // Fall through to RocksDB (or node cache populated by prior commit)
        self.flush_persist()?;
        let trie = persisted::load_tree_from_root(&self.persisted, existing_root)?;
        self.storage_tries.insert(*hashed_address, trie);
        Ok(())
    }

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(MptDbError::Other("store is read-only".to_string()));
        }
        Ok(())
    }

    fn check_not_poisoned(&self) -> Result<()> {
        if self.poisoned {
            return Err(MptDbError::Other(
                "store is poisoned, call load_version() to recover".to_string(),
            ));
        }
        self.check_async_error()
    }

    fn check_not_applied(&self) -> Result<()> {
        if self.applied_this_block {
            return Err(MptDbError::Other(
                "cannot perform this operation while a block is being applied".to_string(),
            ));
        }
        Ok(())
    }

    /// Wait for all in-flight background persist jobs to complete.
    ///
    /// Sends a barrier job through the channel and waits for it to be
    /// processed. Since the channel is FIFO, all previously sent jobs will
    /// have been completed by the time the barrier finishes. The barrier
    /// itself does not perform any extra RocksDB or manifest writes.
    pub fn flush_persist(&self) -> Result<()> {
        self.check_async_error()?;
        if self.durable_version.load(Ordering::Acquire) >= self.version {
            return Ok(());
        }
        if let Some(ref tx) = self.persist_tx {
            let (done_tx, done_rx) = crossbeam_channel::bounded::<Result<()>>(0);
            let job = PersistJob {
                barrier_only: true,
                blobs: vec![],
                manifest: self.manifest.clone(),
                manifest_path: self.manifest_path.clone(),
                version: self.version,
                done: Some(done_tx),
            };
            if tx.send(job).is_ok() {
                match done_rx.recv() {
                    Ok(result) => {
                        result?;
                        // After barrier completes, all prior jobs are durable
                        self.durable_version.store(self.version, Ordering::Release);
                        return Ok(());
                    }
                    Err(_) => return self.check_async_error(),
                }
            }
        }
        self.check_async_error()
    }

    /// Check if this is a fresh DB (manifest = {0->EMPTY_ROOT_HASH}, no other versions, empty
    /// store).
    fn is_fresh_db(&self) -> Result<bool> {
        if self.manifest.latest_version != 0 {
            return Ok(false);
        }
        if self.manifest.versions.len() != 1 {
            return Ok(false);
        }
        if self.manifest.get_root(0) != Some(EMPTY_ROOT_HASH) {
            return Ok(false);
        }
        self.persisted.is_empty()
    }
}

#[cfg(test)]
impl MptCommitStore {
    pub(crate) fn set_fail_point(&mut self, fail: Option<CommitFailPoint>) {
        self.fail_point = fail;
    }

    pub(crate) fn set_async_fail_mode(&self, mode: u8) {
        self.async_fail_mode.store(mode, Ordering::Relaxed);
    }

    pub(crate) fn set_parallelism_thresholds(&mut self, thresholds: ParallelismThresholds) {
        self.parallelism = thresholds;
    }

    pub(crate) fn loaded_from_checkpoint(&self) -> bool {
        self.loaded_from_checkpoint
    }
}

impl MptCommitter for MptCommitStore {
    fn apply_bundle_state(&mut self, bundle: &BundleState) -> Result<()> {
        self.check_writable()?;
        self.check_not_poisoned()?;

        if self.applied_this_block {
            return Err(MptDbError::Other(
                "apply_bundle_state already called for this block".to_string(),
            ));
        }

        let start = std::time::Instant::now();
        let apply_result = self.apply_bundle_state_inner(bundle);
        if apply_result.is_err() {
            self.poisoned = true;
            return apply_result;
        }

        self.last_apply_duration = start.elapsed();
        self.applied_this_block = true;
        Ok(())
    }

    fn commit(&mut self) -> Result<(i64, B256)> {
        self.check_writable()?;
        self.check_not_poisoned()?;

        if !self.applied_this_block {
            return Err(MptDbError::Other("must call apply_bundle_state before commit".to_string()));
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
        // Wait for any in-flight persist jobs to complete before reloading from disk
        self.flush_persist()?;
        // Always reload manifest from disk
        self.manifest = VersionManifest::load(&self.manifest_path)?;
        self.reload_account_trie()?;
        self.version = self.manifest.latest_version;
        self.dirty_accounts.clear();
        self.storage_tries.clear();
        self.storage_trie_cache.clear();
        self.applied_this_block = false;
        self.poisoned = false;
        #[cfg(test)]
        {
            let root =
                self.manifest.get_root(self.manifest.latest_version).unwrap_or(EMPTY_ROOT_HASH);
            self.loaded_from_checkpoint =
                Self::try_load_checkpoint(&self.dir, self.manifest.latest_version, root)?.is_some();
        }
        Ok(())
    }

    fn rollback(&mut self, target_version: i64) -> Result<()> {
        self.check_writable()?;
        // Wait for any in-flight persist jobs before modifying manifest
        self.flush_persist()?;

        if target_version < self.manifest.earliest_version ||
            target_version > self.manifest.latest_version
        {
            return Err(MptDbError::Other(format!(
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
        self.shutdown(false)
    }

    fn prune_before(&mut self, version: i64) -> Result<()> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;
        // Wait for any in-flight persist jobs before modifying manifest
        self.flush_persist()?;

        if version < self.manifest.earliest_version || version > self.manifest.latest_version {
            return Err(MptDbError::Other(format!(
                "prune_before: version {} out of range [{}, {}]",
                version, self.manifest.earliest_version, self.manifest.latest_version
            )));
        }

        // Remove [earliest, version) from manifest, keep version itself
        let mut new_manifest = self.manifest.clone();
        let to_remove: Vec<i64> = new_manifest
            .versions
            .keys()
            .copied()
            .filter(|v| *v >= new_manifest.earliest_version && *v < version)
            .collect();
        for v in to_remove {
            new_manifest.versions.remove(&v);
        }
        new_manifest.earliest_version = version;
        new_manifest.save(&self.manifest_path)?;
        self.manifest = new_manifest;

        Ok(())
    }

    fn gc(&mut self) -> Result<MptGcStats> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;
        // Wait for any in-flight persist jobs before GC scans reachable nodes
        self.flush_persist()?;

        let live_roots: Vec<B256> = self.manifest.versions.values().copied().collect();
        let live = gc::collect_reachable_hashes(&self.persisted, live_roots)?;
        gc::gc_unreachable_nodes(&self.persisted, &live)
    }

    fn account_proof(
        &self,
        version: i64,
        address: Address,
        slots: &[B256],
    ) -> Result<AccountProof> {
        // Wait for any in-flight persist jobs to ensure latest nodes are on disk
        self.flush_persist()?;
        let root = self.manifest.get_root(version).ok_or_else(|| {
            MptDbError::Other(format!("account_proof: version {version} not in manifest"))
        })?;
        proof::build_account_proof_from_root(&self.persisted, root, address, slots)
    }

    fn exporter(&self, version: i64) -> Result<Box<dyn MptSnapshotExporter>> {
        // Wait for any in-flight persist jobs to ensure all nodes are on disk
        self.flush_persist()?;
        let root = self.manifest.get_root(version).ok_or_else(|| {
            MptDbError::Other(format!("exporter: version {version} not in manifest"))
        })?;
        let exp = SnapshotExporter::new(self.persisted.clone(), root, version)?;
        Ok(Box::new(exp))
    }

    fn frontier(&self) -> CommitFrontier {
        let logical = self.version;
        let durable = self.durable_version.load(Ordering::Relaxed);
        let committed_root = self.manifest.get_root(logical).unwrap_or(EMPTY_ROOT_HASH);
        let durable_root = self.manifest.get_root(durable).unwrap_or(EMPTY_ROOT_HASH);
        CommitFrontier {
            logical_version: logical,
            durable_version: durable,
            committed_root,
            durable_root,
        }
    }

    fn importer(
        &mut self,
        version: i64,
        expected_root: B256,
    ) -> Result<Box<dyn MptSnapshotImporter + '_>> {
        self.check_writable()?;
        self.check_not_poisoned()?;
        self.check_not_applied()?;

        if !self.is_fresh_db()? {
            return Err(MptDbError::Other("importer: DB is not fresh, cannot import".to_string()));
        }

        let imp = SnapshotImporter::new(
            version,
            expected_root,
            self.persisted.clone(),
            self.manifest_path.clone(),
        )?;

        // Wrap in a struct that holds &mut self to bind lifetime
        Ok(Box::new(BoundImporter { inner: imp, store: self }))
    }
}

impl MptCommitStore {
    pub fn last_commit_profile(&self) -> &CommitProfile {
        &self.last_commit_profile
    }

    pub fn commit_with_profile(&mut self) -> Result<((i64, B256), CommitProfile)> {
        let result = self.commit()?;
        Ok((result, self.last_commit_profile.clone()))
    }

    fn apply_bundle_state_inner(&mut self, bundle: &BundleState) -> Result<()> {
        let collect_start = std::time::Instant::now();
        let dirty_accounts = state::collect_dirty_accounts(bundle)?;
        let collect_elapsed = collect_start.elapsed();
        let load_start = std::time::Instant::now();
        let mut storage_loads = Vec::new();

        for dirty in &dirty_accounts {
            if dirty.storage_wiped || dirty.storage_changes.is_empty() {
                continue;
            }
            if self.storage_tries.contains_key(&dirty.hashed_address) {
                continue;
            }
            if dirty.storage_known_empty {
                self.storage_tries.insert(dirty.hashed_address, MptTree::new());
                continue;
            }
            if let Some(cached_trie) = self.storage_trie_cache.remove(&dirty.hashed_address) {
                self.storage_tries.insert(dirty.hashed_address, cached_trie);
                continue;
            }

            let existing_root = self.get_existing_storage_root(&dirty.hashed_address);
            if existing_root == EMPTY_ROOT_HASH {
                self.storage_tries.insert(dirty.hashed_address, MptTree::new());
            } else {
                storage_loads.push((dirty.hashed_address, existing_root));
            }
        }

        if !storage_loads.is_empty() {
            self.flush_persist()?;
            let persisted = Arc::clone(&self.persisted);
            let loaded_tries: Vec<(B256, MptTree)> = storage_loads
                .into_par_iter()
                .map(|(hashed_address, existing_root)| {
                    persisted::load_tree_from_root(&persisted, existing_root)
                        .map(|trie| (hashed_address, trie))
                })
                .collect::<Result<_>>()?;
            self.storage_tries.extend(loaded_tries);
        }

        let get_or_load_elapsed = load_start.elapsed();
        let mut slot_updates_elapsed = Duration::ZERO;

        for dirty in &dirty_accounts {
            if dirty.storage_wiped {
                // Evict from cache: selfdestruct invalidates any cached trie
                self.storage_trie_cache.remove(&dirty.hashed_address);
                // Wiped: start from empty storage trie, apply new changes on top
                let mut new_trie = MptTree::new();
                let slot_updates_start = std::time::Instant::now();
                for change in &dirty.storage_changes {
                    let slot_key = Nibbles::unpack(&change.hashed_slot);
                    if change.value == U256::ZERO {
                        // ZERO = delete (no-op on empty trie)
                        new_trie.delete(&slot_key);
                    } else {
                        let encoded = alloy_rlp_encode_u256(&change.value);
                        new_trie.insert(&slot_key, encoded);
                    }
                }
                slot_updates_elapsed += slot_updates_start.elapsed();
                self.storage_tries.insert(dirty.hashed_address, new_trie);
            } else if !dirty.storage_changes.is_empty() {
                let trie = self.storage_tries.get_mut(&dirty.hashed_address).unwrap();
                let slot_updates_start = std::time::Instant::now();
                for change in &dirty.storage_changes {
                    let slot_key = Nibbles::unpack(&change.hashed_slot);
                    if change.value == U256::ZERO {
                        trie.delete(&slot_key);
                    } else {
                        let encoded = alloy_rlp_encode_u256(&change.value);
                        trie.insert(&slot_key, encoded);
                    }
                }
                slot_updates_elapsed += slot_updates_start.elapsed();
            }
            // If no storage changes and not wiped: no storage trie needed (REUSE)
        }

        self.dirty_accounts = dirty_accounts;
        self.last_apply_collect_dirty_accounts = collect_elapsed;
        self.last_apply_get_or_load_storage_tries = get_or_load_elapsed;
        self.last_apply_storage_slot_updates = slot_updates_elapsed;
        Ok(())
    }

    /// Core commit logic: compute roots, persist nodes, update manifest.
    ///
    /// ## Persistence Protocol
    ///
    /// Synchronous commits durably persist dirty trie nodes before atomically
    /// saving the manifest. Asynchronous commits publish the new logical
    /// version immediately and rely on the background worker to make that
    /// version durable; callers that need durability must call `flush_persist`.
    fn commit_inner(&mut self) -> Result<(i64, B256)> {
        // Phase 1: compute storage roots for all dirty accounts.
        //
        // Collect DELETE/REUSE roots serially (cheap lookups), then compute
        // RECOMPUTE roots potentially in parallel using mem::take on
        // storage_tries for ownership transfer.
        let profile_start = std::time::Instant::now();
        let mut storage_roots: HashMap<B256, B256> =
            HashMap::with_capacity(self.dirty_accounts.len());
        let storage_start = std::time::Instant::now();

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

        let storage_artifacts: Vec<StorageTrieCommitArtifacts> = if should_parallel {
            storage_tries_vec
                .into_par_iter()
                .map(|(addr, mut trie)| {
                    let (root, blobs) = trie.root_hash_and_dirty_blobs();
                    StorageTrieCommitArtifacts {
                        hashed_address: addr,
                        storage_root: root,
                        node_blobs: blobs,
                        trie,
                    }
                })
                .collect()
        } else {
            storage_tries_vec
                .into_iter()
                .map(|(addr, mut trie)| {
                    let (root, blobs) = trie.root_hash_and_dirty_blobs();
                    StorageTrieCommitArtifacts {
                        hashed_address: addr,
                        storage_root: root,
                        node_blobs: blobs,
                        trie,
                    }
                })
                .collect()
        };

        // Merge RECOMPUTE roots into storage_roots map
        for artifact in &storage_artifacts {
            storage_roots.insert(artifact.hashed_address, artifact.storage_root);
        }

        let storage_roots_elapsed = storage_start.elapsed();

        // Phase 2: update account trie
        let account_updates_start = std::time::Instant::now();
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
        let account_updates_elapsed = account_updates_start.elapsed();

        // Phase 2b: compute state root and collect dirty blobs in one pass
        let account_root_start = std::time::Instant::now();
        let (state_root, account_blobs) =
            self.account_trie.root_hash_and_dirty_blobs_parallel(&self.parallelism);
        let account_root_elapsed = account_root_start.elapsed();

        // Separate node blobs from tries so we can cache tries after persist
        let mut storage_cache_candidates: Vec<(B256, MptTree)> =
            Vec::with_capacity(storage_artifacts.len());
        let extra_blob_capacity: usize =
            storage_artifacts.iter().map(|artifact| artifact.node_blobs.len()).sum();
        let mut all_blobs = Vec::with_capacity(account_blobs.len() + extra_blob_capacity);
        all_blobs.extend(account_blobs);
        for artifact in storage_artifacts {
            all_blobs.extend(artifact.node_blobs);
            storage_cache_candidates.push((artifact.hashed_address, artifact.trie));
        }

        // Check test failpoint: BeforePersist
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::BeforePersist) {
            return Err(MptDbError::Other("failpoint: BeforePersist".to_string()));
        }

        // Clear dirty flags now (in-memory state is authoritative after root computation)
        self.account_trie.clear_dirty();

        // Check test failpoint: AfterPersistBeforeManifest
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::AfterPersistBeforeManifest) {
            return Err(MptDbError::Other("failpoint: AfterPersistBeforeManifest".to_string()));
        }

        // Update manifest (in-memory)
        let new_version = self.version + 1;
        let mut manifest_copy = self.manifest.clone();
        manifest_copy.add_version(new_version, state_root)?;

        // Check test failpoint: ManifestSave
        #[cfg(test)]
        if self.fail_point == Some(CommitFailPoint::ManifestSave) {
            return Err(MptDbError::Other("failpoint: ManifestSave".to_string()));
        }

        // Decide async vs sync based on batch size: async for small-to-medium
        // batches (saves fsync latency), sync for large batches (avoids the
        // cache clone overhead that exceeds fsync savings).
        let use_async =
            all_blobs.len() < self.config.async_blob_threshold && self.persist_tx.is_some();

        let persist_start = std::time::Instant::now();
        if use_async {
            let tx = self.persist_tx.as_ref().unwrap();
            let job = PersistJob {
                barrier_only: false,
                blobs: all_blobs,
                manifest: manifest_copy.clone(),
                manifest_path: self.manifest_path.clone(),
                version: new_version,
                done: None,
            };
            tx.send(job).map_err(|e| MptDbError::Other(format!("send persist job: {e}")))?;
        } else {
            // Synchronous persist for large batches or when no background worker
            self.persisted.persist_batch(&all_blobs, true)?;
            manifest_copy.save(&self.manifest_path)?;
            // Sync persist is immediately durable
            self.durable_version.store(new_version, Ordering::Release);
        }
        let persist_elapsed = persist_start.elapsed();

        // Collect deleted accounts before clearing dirty_accounts, so we can
        // exclude them from the storage trie cache.
        let deleted_accounts: HashSet<B256> = self
            .dirty_accounts
            .iter()
            .filter(|d| d.info.is_none() && d.storage_wiped)
            .map(|d| d.hashed_address)
            .collect();

        // Commit succeeded: update internal state
        self.manifest = manifest_copy;
        self.version = new_version;
        self.dirty_accounts.clear();
        self.storage_tries.clear();
        self.applied_this_block = false;

        // Move committed storage tries into the cross-block cache so the next
        // block can reuse them without reloading from RocksDB.
        let cache_publish_start = std::time::Instant::now();
        for (addr, mut trie) in storage_cache_candidates {
            if deleted_accounts.contains(&addr) {
                continue;
            }
            trie.clear_dirty();
            self.storage_trie_cache.insert(addr, trie);
        }
        let cache_publish_elapsed = cache_publish_start.elapsed();

        self.last_commit_profile = CommitProfile {
            apply_bundle_state: self.last_apply_duration,
            apply_collect_dirty_accounts: self.last_apply_collect_dirty_accounts,
            apply_get_or_load_storage_tries: self.last_apply_get_or_load_storage_tries,
            apply_storage_slot_updates: self.last_apply_storage_slot_updates,
            storage_roots: storage_roots_elapsed,
            account_updates: account_updates_elapsed,
            account_root_and_blobs: account_root_elapsed,
            persist_and_manifest: persist_elapsed,
            cache_publish: cache_publish_elapsed,
            total_commit: profile_start.elapsed(),
        };

        Ok((new_version, state_root))
    }
}

/// Encode a U256 value using alloy_rlp (big-endian, trims leading zeros).
fn alloy_rlp_encode_u256(value: &U256) -> Vec<u8> {
    let mut buf = Vec::new();
    value.encode(&mut buf);
    buf
}

/// Wrapper that binds an importer's lifetime to the MptCommitStore.
/// On close, it updates the store's internal state to reflect the import.
struct BoundImporter<'a> {
    inner: SnapshotImporter,
    store: &'a mut MptCommitStore,
}

impl<'a> super::r#trait::MptSnapshotImporter for BoundImporter<'a> {
    fn add_node(&mut self, node: &super::r#trait::MptSnapshotNode) -> Result<()> {
        self.inner.add_node(node)
    }

    fn close(&mut self) -> Result<()> {
        // Drop the persist channel so the background thread finishes and
        // releases its Arc<PersistedTrieStore>. Then close the old store
        // to release the RocksDB lock before the atomic rename in inner.close().
        self.store.persist_tx.take();
        if let Some(handle) = self.store.persist_handle.take() {
            let _ = handle.join();
        }
        // Now self.store.persisted should be the only Arc reference
        if let Some(old_store) = Arc::get_mut(&mut self.store.persisted) {
            old_store.close()?;
        }

        self.inner.close()?;

        // After successful import with atomic install, the live trie_nodes dir
        // has been replaced. Reopen the PersistedTrieStore at the new directory.
        let trie_nodes_dir = self.store.dir.join("trie_nodes");
        self.store.persisted = Arc::new(PersistedTrieStore::open_with_capacity(
            &trie_nodes_dir,
            self.store.config.persisted_node_cache_capacity,
        )?);

        // Reload manifest from disk
        self.store.manifest = VersionManifest::load(&self.store.manifest_path)?;
        self.store.version = self.store.manifest.latest_version;
        self.store.durable_version.store(self.store.version, Ordering::Release);

        // Reload account trie from imported root
        self.store.reload_account_trie()?;

        // Reset working state
        self.store.dirty_accounts.clear();
        self.store.storage_tries.clear();
        self.store.storage_trie_cache =
            StorageTrieCache::new(self.store.config.storage_trie_cache_capacity);
        self.store.applied_this_block = false;
        self.store.poisoned = false;
        #[cfg(test)]
        {
            self.store.loaded_from_checkpoint = true;
        }

        Ok(())
    }
}

impl Drop for MptCommitStore {
    fn drop(&mut self) {
        let _ = self.shutdown(true);
    }
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

    #[test]
    fn t5_20_checkpoint_written_and_used_on_reopen() {
        let dir = TempDir::new().unwrap();
        let checkpoint_path = MptCommitStore::checkpoint_path(dir.path());

        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        let addr = Address::repeat_byte(0x44);
        let info = default_info(1, 123);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(9))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
        store.close().unwrap();

        assert!(checkpoint_path.exists(), "checkpoint should be written on close");

        let store2 = MptCommitStore::open(dir.path(), false).unwrap();
        assert!(store2.loaded_from_checkpoint(), "reopen should load account trie checkpoint");
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

    // ── Phase 5 tests ──

    /// T6.1: read_only open uses shared lock; verified by: reader close then reopen succeeds
    /// Note: Two concurrent readers at the same RocksDB path is blocked by RocksDB's internal
    /// directory lock, so we verify the file-level shared lock semantics via close+reopen.
    /// Full reader+reader coexistence requires sub-process testing (see integration tests).
    #[test]
    fn t6_1_shared_lock_readers() {
        let dir = TempDir::new().unwrap();
        // Open reader, verify it acquires shared lock (file_lock is Some)
        let mut reader1 = MptCommitStore::open(dir.path(), true).unwrap();
        assert!(reader1.file_lock.is_some());
        reader1.close().unwrap();
        // Can reopen as reader after close
        let _reader2 = MptCommitStore::open(dir.path(), true).unwrap();
    }

    /// T6.2: writer open then reader open fails
    #[test]
    fn t6_2_writer_blocks_reader() {
        let dir = TempDir::new().unwrap();
        let _writer = MptCommitStore::open(dir.path(), false).unwrap();
        let result = MptCommitStore::open(dir.path(), true);
        assert!(result.is_err());
    }

    /// T6.3: reader open then writer open fails
    #[test]
    fn t6_3_reader_blocks_writer() {
        let dir = TempDir::new().unwrap();
        let _reader = MptCommitStore::open(dir.path(), true).unwrap();
        let result = MptCommitStore::open(dir.path(), false);
        assert!(result.is_err());
    }

    /// T6.4: close releases shared/exclusive lock
    #[test]
    fn t6_4_close_releases_lock() {
        let dir = TempDir::new().unwrap();
        let mut reader = MptCommitStore::open(dir.path(), true).unwrap();
        reader.close().unwrap();
        // Writer should succeed now
        let _writer = MptCommitStore::open(dir.path(), false).unwrap();
    }

    /// T6.4b: implicit drop also releases the writer lock and background resources.
    #[test]
    fn t6_4b_drop_releases_lock() {
        let dir = TempDir::new().unwrap();
        {
            let mut writer = MptCommitStore::open(dir.path(), false).unwrap();
            writer.apply_bundle_state(&BundleState::default()).unwrap();
            writer.commit().unwrap();
        }
        let _writer2 = MptCommitStore::open(dir.path(), false).unwrap();
    }

    /// T6.5: prune_before correctly updates manifest earliest_version
    #[test]
    fn t6_5_prune_before() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        for _ in 0..5 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }
        assert_eq!(store.version(), 5);

        store.prune_before(3).unwrap();
        assert_eq!(store.manifest.earliest_version, 3);
        assert!(store.manifest.get_root(1).is_none());
        assert!(store.manifest.get_root(2).is_none());
        assert!(store.manifest.get_root(3).is_some());
        assert!(store.manifest.get_root(5).is_some());
    }

    /// T6.6: prune_before(latest) is legal, keeps only latest
    #[test]
    fn t6_6_prune_before_latest() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        for _ in 0..3 {
            store.apply_bundle_state(&BundleState::default()).unwrap();
            store.commit().unwrap();
        }

        store.prune_before(3).unwrap();
        assert_eq!(store.manifest.earliest_version, 3);
        assert!(store.manifest.get_root(3).is_some());
        // Only version 3 should remain (version 0 was pruned)
        assert!(store.manifest.get_root(0).is_none());
    }

    /// T6.7: prune_before(out of range) -> Err
    #[test]
    fn t6_7_prune_before_out_of_range() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        assert!(store.prune_before(-1).is_err());
        assert!(store.prune_before(999).is_err());
    }

    /// T6.8: gc returns stats and doesn't change manifest/latest version
    #[test]
    fn t6_8_gc_returns_stats() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 1000);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let ver_before = store.version();
        let stats = store.gc().unwrap();
        assert!(stats.scanned_nodes > 0);
        assert_eq!(stats.deleted_nodes, 0); // no orphans yet
        assert_eq!(store.version(), ver_before);
    }

    /// T6.9: read_only prune/gc/importer all Err
    #[test]
    fn t6_9_read_only_maintenance_errors() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), true).unwrap();
        assert!(store.prune_before(0).is_err());
        assert!(store.gc().is_err());
        assert!(store.importer(1, B256::ZERO).is_err());
    }

    /// T6.10: applied_this_block=true blocks prune/gc
    #[test]
    fn t6_10_applied_blocks_maintenance() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.apply_bundle_state(&BundleState::default()).unwrap();
        assert!(store.prune_before(0).is_err());
        assert!(store.gc().is_err());
    }

    /// T6.11: exporter(valid version) creates and streams
    #[test]
    fn t6_11_exporter_valid_version() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 100);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let mut exp = store.exporter(1).unwrap();
        let meta = exp.meta();
        assert_eq!(meta.version, 1);
        // Should produce at least one node
        assert!(exp.next_node().unwrap().is_some());
        exp.close().unwrap();
    }

    /// T6.12: exporter(missing version) -> Err
    #[test]
    fn t6_12_exporter_missing_version() {
        let dir = TempDir::new().unwrap();
        let store = MptCommitStore::open(dir.path(), false).unwrap();
        assert!(store.exporter(999).is_err());
    }

    /// T6.13: importer(fresh DB) succeeds and load_version works after
    #[test]
    fn t6_13_importer_fresh_db() {
        // Build source
        let src_dir = TempDir::new().unwrap();
        let mut src_store = MptCommitStore::open(src_dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 100);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        src_store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = src_store.commit().unwrap();

        // Export
        let mut exp = src_store.exporter(1).unwrap();
        let mut nodes = Vec::new();
        while let Some(n) = exp.next_node().unwrap() {
            nodes.push(n);
        }
        exp.close().unwrap();
        src_store.close().unwrap();

        // Import into fresh DB
        let dst_dir = TempDir::new().unwrap();
        let mut dst_store = MptCommitStore::open(dst_dir.path(), false).unwrap();

        {
            let mut imp = dst_store.importer(1, root).unwrap();
            for n in &nodes {
                imp.add_node(n).unwrap();
            }
            imp.close().unwrap();
        }

        // After import, store should reflect new state
        assert_eq!(dst_store.version(), 1);
        assert_eq!(dst_store.manifest.get_root(1), Some(root));
    }

    /// T6.14: importer(non-fresh DB) -> Err
    #[test]
    fn t6_14_importer_non_fresh() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        // Commit something first
        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        let result = store.importer(2, B256::ZERO);
        assert!(result.is_err());
    }

    /// T6.15: account_proof(version) for specified committed root is correct
    #[test]
    fn t6_15_account_proof_version() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info = default_info(1, 1000);
        let bundle =
            make_bundle(vec![(addr, Some(info), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle).unwrap();
        let (_, root) = store.commit().unwrap();

        let proof = store.account_proof(1, addr, &[]).unwrap();
        assert!(proof.info.is_some());
        proof.verify(root).unwrap();
    }

    /// T6.16: rollback then exporter/proof only sees new latest/kept versions
    #[test]
    fn t6_16_rollback_then_proof() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);
        let info1 = default_info(1, 100);
        let bundle1 =
            make_bundle(vec![(addr, Some(info1), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        let info2 = default_info(2, 200);
        let bundle2 =
            make_bundle(vec![(addr, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        store.commit().unwrap();

        store.rollback(1).unwrap();
        assert_eq!(store.version(), 1);

        // Version 2 should be gone
        assert!(store.exporter(2).is_err());
        assert!(store.account_proof(2, addr, &[]).is_err());

        // Version 1 should work
        let proof = store.account_proof(1, addr, &[]).unwrap();
        proof.verify(root1).unwrap();
    }

    /// T6.17: historical version proof before prune works, after prune+gc -> Err
    #[test]
    fn t6_17_historical_proof_prune_gc() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0x01);

        // v1: create account with large storage to ensure hash children
        let info1 = default_info(1, 100);
        let slots1: Vec<(U256, U256, U256)> =
            (0..10).map(|i| (U256::from(i), U256::ZERO, U256::from(i + 100))).collect();
        let bundle1 =
            make_bundle(vec![(addr, Some(info1), revm_database::AccountStatus::Changed, slots1)]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        // v2: completely different state
        let addr2 = Address::repeat_byte(0x02);
        let info2 = default_info(1, 999);
        let bundle2 =
            make_bundle(vec![(addr2, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        store.apply_bundle_state(&bundle2).unwrap();
        let (_, root2) = store.commit().unwrap();

        // v1 proof should work before prune
        let proof1 = store.account_proof(1, addr, &[]).unwrap();
        proof1.verify(root1).unwrap();

        // Prune v1 + gc
        store.prune_before(2).unwrap();
        store.gc().unwrap();

        // v1 should no longer be in manifest
        assert!(store.account_proof(1, addr, &[]).is_err());

        // v2 should still work
        let proof2 = store.account_proof(2, addr2, &[]).unwrap();
        proof2.verify(root2).unwrap();
    }

    // ── Storage trie cache tests ──

    /// Cross-block cache hit: account A modified in block 1 and block 2.
    /// Block 2 should use the cached trie instead of reloading from RocksDB.
    #[test]
    fn storage_trie_cache_cross_block_hit() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0xAA);
        let info = default_info(1, 1000);
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        // Block 1: create account with slot1
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot1, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        let (_, root1) = store.commit().unwrap();

        // After commit, storage trie should be in cache
        let hashed_addr = keccak256(addr);
        assert!(
            store.storage_trie_cache.contains_key(&hashed_addr),
            "storage trie should be in cache after commit"
        );

        // Block 2: add slot2 to same account
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot2, U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();

        // The trie should have been moved from cache to working set
        assert!(
            !store.storage_trie_cache.contains_key(&hashed_addr),
            "trie should be removed from cache after loading into working set"
        );
        assert!(store.storage_tries.contains_key(&hashed_addr), "trie should be in working set");

        let (_, root2) = store.commit().unwrap();
        assert_ne!(root1, root2, "root should change after adding slot2");

        // After block 2 commit, trie should be back in cache
        assert!(store.storage_trie_cache.contains_key(&hashed_addr));
    }

    /// Selfdestruct (storage_wiped) should evict cached trie and not reuse it.
    #[test]
    fn storage_trie_cache_selfdestruct_evicts() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0xBB);
        let info = default_info(1, 1000);
        let slot1 = U256::from(1);
        let slot2 = U256::from(2);

        // Block 1: create account with slot1
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot1, U256::ZERO, U256::from(10))],
        )]);
        store.apply_bundle_state(&bundle1).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        assert!(store.storage_trie_cache.contains_key(&hashed_addr));

        // Block 2: selfdestruct + recreate with slot2 only
        let bundle2 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::DestroyedChanged,
            vec![(slot2, U256::ZERO, U256::from(20))],
        )]);
        store.apply_bundle_state(&bundle2).unwrap();

        // Cache should be evicted after apply with storage_wiped
        assert!(!store.storage_trie_cache.contains_key(&hashed_addr));

        let (_, root2) = store.commit().unwrap();

        // Verify: only slot2 in storage (slot1 was wiped)
        let hashed_slot2 = keccak256(slot2.to_be_bytes::<32>());
        let mut storage_hb = alloy_trie::HashBuilder::default();
        let mut encoded_val = Vec::new();
        U256::from(20).encode(&mut encoded_val);
        storage_hb.add_leaf(Nibbles::unpack(hashed_slot2), &encoded_val);
        let expected_storage_root = storage_hb.root();

        let trie_account = alloy_trie::TrieAccount {
            nonce: info.nonce,
            balance: info.balance,
            storage_root: expected_storage_root,
            code_hash: info.code_hash,
        };
        let account_rlp = alloy_rlp::encode(&trie_account);
        let mut hb = alloy_trie::HashBuilder::default();
        hb.add_leaf(Nibbles::unpack(hashed_addr), &account_rlp);
        assert_eq!(root2, hb.root());
    }

    /// load_version and rollback clear the storage trie cache.
    #[test]
    fn storage_trie_cache_cleared_on_load_version_and_rollback() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        let addr = Address::repeat_byte(0xCC);
        let info = default_info(1, 1000);
        let bundle = make_bundle(vec![(
            addr,
            Some(info),
            revm_database::AccountStatus::Changed,
            vec![(U256::from(1), U256::ZERO, U256::from(42))],
        )]);
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();

        let hashed_addr = keccak256(addr);
        assert!(store.storage_trie_cache.contains_key(&hashed_addr));

        // load_version clears cache
        store.load_version().unwrap();
        assert!(store.storage_trie_cache.is_empty());

        // Re-populate cache
        store.apply_bundle_state(&bundle).unwrap();
        store.commit().unwrap();
        assert!(store.storage_trie_cache.contains_key(&hashed_addr));

        // rollback clears cache
        store.rollback(1).unwrap();
        assert!(store.storage_trie_cache.is_empty());
    }

    /// Cache correctness across 3 blocks: account touched in block 1 and 3 (not 2)
    /// should still produce correct roots.
    #[test]
    fn storage_trie_cache_correctness_across_blocks() {
        let dir_cached = TempDir::new().unwrap();
        let dir_nocache = TempDir::new().unwrap();

        let addr = Address::repeat_byte(0xDD);
        let info = default_info(1, 1000);
        let slot = U256::from(1);

        // Block 1: set slot=10
        let bundle1 = make_bundle(vec![(
            addr,
            Some(info.clone()),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::ZERO, U256::from(10))],
        )]);
        // Block 2: only change balance (no storage)
        let info2 = default_info(2, 2000);
        let bundle2 =
            make_bundle(vec![(addr, Some(info2), revm_database::AccountStatus::Changed, vec![])]);
        // Block 3: set slot=30
        let info3 = default_info(3, 3000);
        let bundle3 = make_bundle(vec![(
            addr,
            Some(info3),
            revm_database::AccountStatus::Changed,
            vec![(slot, U256::from(10), U256::from(30))],
        )]);

        // Run with cache (normal)
        let mut store_c = MptCommitStore::open(dir_cached.path(), false).unwrap();
        store_c.apply_bundle_state(&bundle1).unwrap();
        let (_, r1c) = store_c.commit().unwrap();
        store_c.apply_bundle_state(&bundle2).unwrap();
        let (_, r2c) = store_c.commit().unwrap();
        store_c.apply_bundle_state(&bundle3).unwrap();
        let (_, r3c) = store_c.commit().unwrap();

        // Run without cache (clear cache after each commit to simulate old behavior)
        let mut store_n = MptCommitStore::open(dir_nocache.path(), false).unwrap();
        store_n.apply_bundle_state(&bundle1).unwrap();
        let (_, r1n) = store_n.commit().unwrap();
        store_n.storage_trie_cache.clear();
        store_n.apply_bundle_state(&bundle2).unwrap();
        let (_, r2n) = store_n.commit().unwrap();
        store_n.storage_trie_cache.clear();
        store_n.apply_bundle_state(&bundle3).unwrap();
        let (_, r3n) = store_n.commit().unwrap();

        assert_eq!(r1c, r1n, "block 1 roots must match");
        assert_eq!(r2c, r2n, "block 2 roots must match");
        assert_eq!(r3c, r3n, "block 3 roots must match");
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

    #[test]
    fn t_async_flush_reports_background_persist_failure() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();
        store.set_async_fail_mode(1);

        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();

        let err = store.flush_persist().unwrap_err();
        assert!(err.to_string().contains("forced async persist failure"));
        assert!(store.apply_bundle_state(&BundleState::default()).is_err());
        assert!(store.close().is_err());
    }

    #[test]
    fn t_async_flush_is_true_barrier() {
        let dir = TempDir::new().unwrap();
        let mut store = MptCommitStore::open(dir.path(), false).unwrap();

        store.apply_bundle_state(&BundleState::default()).unwrap();
        store.commit().unwrap();
        store.flush_persist().unwrap();

        let manifest = VersionManifest::load(&dir.path().join("manifest.json")).unwrap();
        assert_eq!(manifest.latest_version, 1);
        store.close().unwrap();
    }
}
