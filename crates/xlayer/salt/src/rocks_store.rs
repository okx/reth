//! RocksDB-backed persistent storage for SALT.
//!
//! Uses RocksDB (LSM-tree) — the industry-standard KV store for blockchain nodes.
//! LSM-tree architecture is well-suited for SALT's write-heavy flat-state workload:
//! - **Writes**: go to memtable (in-memory) + WAL (sequential I/O) → very fast
//! - **Reads**: block cache (memory) → SST files (disk) with bloom filters
//! - **No COW/page overhead**: unlike MDBX's B-tree, no copy-on-write or page management
//!
//! ## Reverse index (`CF_INDEX`)
//!
//! `CF_INDEX` maps `plain_key (20 or 52 bytes)` → `SaltKey (u64, big-endian)`, enabling
//! O(1) `plain_value_fast` lookups instead of O(bucket_capacity) linear probes.
//!
//! Without this index, every `EphemeralSaltState::shi_upsert` call falls back to scanning
//! all 256+ entries in a bucket. With it, existing entries are located via one RocksDB
//! point read (single block-cache or disk I/O), matching megaETH's "1 I/O per state
//! update" design intent.
//!
//! The index is maintained atomically in the same `WriteBatch` as the state update:
//! - INSERT (`old=None, new=Some(val)`)  → `put(val.key(), salt_key)`
//! - UPDATE (`old=Some(_), new=Some(val)`) → `put(val.key(), salt_key)` (same plain_key)
//! - DELETE (`old=Some(old_val), new=None`) → `delete(old_val.key())`
//!
//! Meta-bucket entries (BucketMeta serializations) are excluded from the index.

use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, Options, WriteBatch, WriteOptions, DB,
};
use salt::{
    constant::default_commitment,
    traits::{StateReader, TrieReader},
    types::*,
    StateUpdates, TrieUpdates,
};
use std::{
    collections::HashMap,
    ops::{Range, RangeInclusive},
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

const CF_STATE: &str = "salt_state";
const CF_TRIE: &str = "salt_trie";
/// Reverse index: plain_key (20 or 52 bytes) → SaltKey (u64, big-endian 8 bytes).
const CF_INDEX: &str = "salt_index";

fn bytes_to_salt_value(bytes: &[u8]) -> SaltValue {
    let mut data = [0u8; MAX_SALT_VALUE_BYTES];
    let len = bytes.len().min(MAX_SALT_VALUE_BYTES);
    data[..len].copy_from_slice(&bytes[..len]);
    SaltValue { data }
}

/// Write statistics for a single block's persistence.
#[derive(Debug)]
pub struct WriteStats {
    /// Number of state entries written.
    pub entries: usize,
    /// Bytes written.
    pub bytes_written: usize,
    /// Total time for the write batch + WAL sync.
    pub persist_duration: Duration,
}

/// 512MB block cache — must exceed total data size across all CFs (~300MB for 200k accounts)
/// so that per-block random reads hit cache instead of SST files.
const BLOCK_CACHE_SIZE: usize = 512 * 1024 * 1024;

/// RocksDB-backed SALT store.
pub struct RocksSaltStore {
    db: DB,
    /// Block cache must outlive DB; sized so full state (~140MB+) fits for hot reads.
    _block_cache: Cache,
    /// In-memory bucket slot counts (same as `MemStore` — part of SALT's in-memory metadata).
    slot_counts: Mutex<HashMap<BucketId, u64>>,
}

impl std::fmt::Debug for RocksSaltStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksSaltStore").finish()
    }
}

impl RocksSaltStore {
    /// Opens or creates a RocksDB-backed SALT store at the given path.
    ///
    /// Uses a 512MB block cache so that all data (~300MB for 200k accounts) fits in cache.
    /// Bloom filters on every CF accelerate point lookups by skipping SST blocks
    /// that definitely don't contain the key.
    pub fn new(path: &Path) -> Result<Self, rocksdb::Error> {
        let block_cache = Cache::new_lru_cache(BLOCK_CACHE_SIZE);

        // Shared table options: block cache + 10-bit bloom filter for point reads.
        let mut table_opts = BlockBasedOptions::default();
        table_opts.set_block_cache(&block_cache);
        table_opts.set_bloom_filter(10.0, false);

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_write_buffer_size(64 * 1024 * 1024); // 64MB memtable
        opts.set_max_write_buffer_number(3);
        opts.set_allow_mmap_reads(true);
        opts.set_block_based_table_factory(&table_opts);

        let mut cf_state_opts = Options::default();
        cf_state_opts.set_block_based_table_factory(&table_opts);

        let mut cf_trie_opts = Options::default();
        cf_trie_opts.set_block_based_table_factory(&table_opts);

        // CF_INDEX: optimised for point reads (plain_key → SaltKey).
        // Keys are short (20 or 52 bytes), values are always 8 bytes.
        let mut cf_index_opts = Options::default();
        cf_index_opts.set_block_based_table_factory(&table_opts);

        let cf_descriptors = vec![
            ColumnFamilyDescriptor::new(CF_STATE, cf_state_opts),
            ColumnFamilyDescriptor::new(CF_TRIE, cf_trie_opts),
            ColumnFamilyDescriptor::new(CF_INDEX, cf_index_opts),
        ];

        let db = DB::open_cf_descriptors(&opts, path, cf_descriptors)?;
        Ok(Self { db, _block_cache: block_cache, slot_counts: Mutex::new(HashMap::new()) })
    }

    /// Applies state updates: writes a batch to RocksDB with WAL sync.
    pub fn update_state(&self, updates: StateUpdates) -> Result<WriteStats, rocksdb::Error> {
        let t0 = Instant::now();
        let cf_state = self.db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        let cf_index = self.db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");

        let mut batch = WriteBatch::default();
        let mut bytes = 0usize;

        // Update in-memory slot counts
        {
            let mut counts = self.slot_counts.lock().unwrap();
            for (key, (old_value, new_value)) in &updates.data {
                if !key.is_in_meta_bucket() {
                    let delta: i64 = match (old_value.is_some(), new_value.is_some()) {
                        (false, true) => 1,
                        (true, false) => -1,
                        _ => 0,
                    };
                    if delta != 0 {
                        let count = counts.entry(key.bucket_id()).or_insert(0);
                        *count = (*count as i64 + delta).max(0) as u64;
                    }
                }
            }
        }

        // Build write batch: state entries + reverse index updates
        let num_entries = updates.data.len();
        for (key, (old_value, new_value)) in updates.data {
            let key_bytes = key.0.to_be_bytes();

            // Maintain CF_INDEX for data buckets only (meta buckets store BucketMeta, not
            // plain account/storage keys).
            //
            // Key insight: on UPDATE the entry stays in the same bucket slot, so
            // plain_key → SaltKey is UNCHANGED.  We only write the index on INSERT
            // (new slot) and DELETE (slot freed).  Skipping UPDATE writes halves the
            // CF_INDEX traffic for workloads that mostly touch existing accounts.
            if !key.is_in_meta_bucket() {
                match (&old_value, &new_value) {
                    (None, Some(new_val)) => {
                        // INSERT: record the new plain_key → SaltKey mapping.
                        batch.put_cf(&cf_index, new_val.key(), &key_bytes);
                    }
                    (Some(old_val), None) => {
                        // DELETE: remove the plain_key → salt_key mapping.
                        batch.delete_cf(&cf_index, old_val.key());
                    }
                    // UPDATE (Some→Some) or no-op (None→None): SaltKey unchanged, skip.
                    _ => {}
                }
            }

            match new_value {
                Some(val) => {
                    let data_len = val.data_len();
                    batch.put_cf(&cf_state, &key_bytes, &val.data[..data_len]);
                    bytes += 8 + data_len;
                }
                None => {
                    batch.delete_cf(&cf_state, &key_bytes);
                    bytes += 8;
                }
            }
        }

        // Write with WAL sync (durable)
        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);
        self.db.write_opt(batch, &write_opts)?;

        Ok(WriteStats {
            entries: num_entries,
            bytes_written: bytes,
            persist_duration: t0.elapsed(),
        })
    }

    /// Applies trie updates to RocksDB (with WAL sync for durability parity with `update_state`).
    ///
    /// Returns the number of trie entries written.
    pub fn update_trie(&self, updates: TrieUpdates) -> Result<usize, rocksdb::Error> {
        let cf = self.db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        let mut batch = WriteBatch::default();
        let mut count = 0;
        for (node_id, (_, new_commitment)) in updates {
            batch.put_cf(&cf, &node_id.to_be_bytes(), &new_commitment);
            count += 1;
        }
        let mut write_opts = WriteOptions::default();
        write_opts.set_sync(true);
        self.db.write_opt(batch, &write_opts)?;
        Ok(count)
    }

    /// Applies state and trie updates atomically in a single WriteBatch.
    ///
    /// Combines both writes into one batch to avoid two separate fsync calls per block.
    /// Uses WAL without a forced fsync, matching MDBX test-mode durability semantics
    /// (data is recoverable from WAL on crash, but the OS may buffer the flush).
    /// Use this in benchmarks where both sides should be measured under the same
    /// durability policy.
    ///
    /// Returns `(WriteStats, trie_entries_written)`.
    pub fn update_state_and_trie(
        &self,
        state_updates: StateUpdates,
        trie_updates: TrieUpdates,
    ) -> Result<(WriteStats, usize), rocksdb::Error> {
        let t0 = Instant::now();
        let cf_state = self.db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        let cf_trie = self.db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        let cf_index = self.db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");

        let mut batch = WriteBatch::default();
        let mut bytes = 0usize;

        // Update in-memory slot counts (same logic as update_state).
        {
            let mut counts = self.slot_counts.lock().unwrap();
            for (key, (old_value, new_value)) in &state_updates.data {
                if !key.is_in_meta_bucket() {
                    let delta: i64 = match (old_value.is_some(), new_value.is_some()) {
                        (false, true) => 1,
                        (true, false) => -1,
                        _ => 0,
                    };
                    if delta != 0 {
                        let count = counts.entry(key.bucket_id()).or_insert(0);
                        *count = (*count as i64 + delta).max(0) as u64;
                    }
                }
            }
        }

        // State entries + reverse index updates.
        let num_state_entries = state_updates.data.len();
        for (key, (old_value, new_value)) in state_updates.data {
            let key_bytes = key.0.to_be_bytes();

            // Maintain CF_INDEX for data buckets only.
            // Only INSERT and DELETE change the plain_key → SaltKey mapping;
            // UPDATE leaves the SaltKey in place, so skip it.
            if !key.is_in_meta_bucket() {
                match (&old_value, &new_value) {
                    (None, Some(new_val)) => {
                        batch.put_cf(&cf_index, new_val.key(), &key_bytes);
                    }
                    (Some(old_val), None) => {
                        batch.delete_cf(&cf_index, old_val.key());
                    }
                    _ => {}
                }
            }

            match new_value {
                Some(val) => {
                    let data_len = val.data_len();
                    batch.put_cf(&cf_state, &key_bytes, &val.data[..data_len]);
                    bytes += 8 + data_len;
                }
                None => {
                    batch.delete_cf(&cf_state, &key_bytes);
                    bytes += 8;
                }
            }
        }

        // Trie entries.
        let mut trie_count = 0;
        for (node_id, (_, new_commitment)) in trie_updates {
            batch.put_cf(&cf_trie, &node_id.to_be_bytes(), &new_commitment);
            trie_count += 1;
        }

        // Single write — WAL-durable but no forced fsync.
        self.db.write_opt(batch, &WriteOptions::default())?;

        Ok((
            WriteStats {
                entries: num_state_entries,
                bytes_written: bytes,
                persist_duration: t0.elapsed(),
            },
            trie_count,
        ))
    }

    /// Logs bucket used-slot distribution after pre-pop for diagnosing SHI load factor / write
    /// amplification. High mean or max used per bucket suggests high load factor and longer probe
    /// chains.
    pub fn log_bucket_load_stats(&self) {
        let counts = self.slot_counts.lock().unwrap();
        let used: Vec<u64> = counts.values().copied().filter(|&u| u > 0).collect();
        drop(counts);
        if used.is_empty() {
            eprintln!("  [SALT] bucket load: 0 buckets with data");
            return;
        }
        let n = used.len();
        let max_used = *used.iter().max().unwrap_or(&0);
        let sum: u64 = used.iter().sum();
        let mean = sum as f64 / n as f64;
        let mut sorted = used.clone();
        sorted.sort_unstable();
        let p50 = sorted[n / 2];
        let idx99 = (n * 99) / 100;
        let p99 = sorted.get(idx99).copied().unwrap_or(p50);
        eprintln!(
            "  [SALT] bucket load: {} buckets with data, used_slots max={} mean={:.1} p50={} p99={}  (high values → load factor >0.7 → SHI probe chains)",
            n, max_used, mean, p50, p99
        );
    }
}

impl StateReader for RocksSaltStore {
    type Error = SaltError;

    fn value(&self, key: SaltKey) -> Result<Option<SaltValue>, Self::Error> {
        let cf = self.db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        match self.db.get_cf(&cf, key.0.to_be_bytes()) {
            Ok(Some(bytes)) => Ok(Some(bytes_to_salt_value(&bytes))),
            _ => Ok(None),
        }
    }

    fn entries(
        &self,
        range: RangeInclusive<SaltKey>,
    ) -> Result<Vec<(SaltKey, SaltValue)>, Self::Error> {
        let cf = self.db.cf_handle(CF_STATE).expect("CF_STATE must exist");
        let start = range.start().0.to_be_bytes();
        let end = range.end().0.to_be_bytes();

        let mut result = Vec::new();
        let iter = self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward));
        for item in iter.flatten() {
            let (k, v) = item;
            if k.as_ref() > end.as_slice() {
                break;
            }
            let key_u64 = u64::from_be_bytes(k.as_ref().try_into().unwrap_or([0; 8]));
            result.push((SaltKey(key_u64), bytes_to_salt_value(&v)));
        }
        Ok(result)
    }

    fn metadata(&self, bucket_id: BucketId) -> Result<BucketMeta, Self::Error> {
        let key = bucket_metadata_key(bucket_id);
        let mut meta = match self.value(key)? {
            Some(v) => (&v).try_into()?,
            None => BucketMeta::default(),
        };
        let counts = self.slot_counts.lock().unwrap();
        meta.used = Some(counts.get(&bucket_id).copied().unwrap_or(0));
        Ok(meta)
    }

    fn bucket_used_slots(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        if !is_valid_data_bucket(bucket_id) {
            return Ok(0);
        }
        Ok(self.slot_counts.lock().unwrap().get(&bucket_id).copied().unwrap_or(0))
    }

    /// O(1) lookup of the `SaltKey` for a given plain key via the reverse index (`CF_INDEX`).
    ///
    /// `CF_INDEX` is maintained atomically alongside every state write, so this always
    /// reflects the current committed state.  On a cache hit the lookup is entirely
    /// in-memory (RocksDB block cache); on a cold read it costs one disk I/O — matching
    /// megaETH's "1 I/O per state update" design intent.
    fn plain_value_fast(&self, plain_key: &[u8]) -> Result<SaltKey, Self::Error> {
        let cf = self.db.cf_handle(CF_INDEX).expect("CF_INDEX must exist");
        match self.db.get_cf(&cf, plain_key) {
            Ok(Some(bytes)) if bytes.len() == 8 => {
                let arr: [u8; 8] = bytes[..8].try_into().expect("length checked above");
                Ok(SaltKey(u64::from_be_bytes(arr)))
            }
            // Key not in index (not yet inserted) or unexpected length — fall through to SHI probe.
            _ => Err(SaltError::UnsupportedOperation {
                operation: "RocksSaltStore::plain_value_fast: key not in index",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{account_plain_key, storage_plain_key};
    use alloy_consensus::constants::KECCAK_EMPTY;
    use alloy_primitives::{map::HashMap as PrimitivesHashMap, Address, B256, U256};
    use revm_database::{states::StorageSlot, AccountStatus, StorageWithOriginalValues};
    use revm_state::AccountInfo;
    use salt::{EphemeralSaltState, StateRoot};
    use tempfile::TempDir;

    fn make_bundle(accounts: Vec<(Address, Vec<(B256, U256)>)>) -> revm_database::BundleState {
        let mut state = PrimitivesHashMap::default();
        for (addr, slots) in accounts {
            let info = AccountInfo {
                nonce: 1,
                balance: U256::from(1000u64),
                code_hash: KECCAK_EMPTY,
                account_id: None,
                code: None,
            };
            let mut storage = StorageWithOriginalValues::default();
            for (slot, val) in slots {
                storage.insert(slot.into(), StorageSlot::new_changed(U256::ONE, val));
            }
            state.insert(
                addr,
                revm_database::BundleAccount {
                    info: Some(info),
                    original_info: None,
                    status: AccountStatus::Changed,
                    storage,
                },
            );
        }
        revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    /// Verify that `plain_value_fast` returns the correct `SaltKey` after a state write,
    /// and returns an error for keys that were never inserted.
    #[test]
    fn test_plain_value_fast_hit_and_miss() {
        let tmp = TempDir::new().unwrap();
        let store = RocksSaltStore::new(tmp.path()).unwrap();

        let addr = Address::from([0xAB; 20]);
        let slot = B256::from([0x01; 32]);
        let bundle = make_bundle(vec![(addr, vec![(slot, U256::from(42u64))])]);

        // Apply state via EphemeralSaltState so CF_INDEX gets populated.
        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
        let mut eph = EphemeralSaltState::new(&store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        store.update_state(state_updates).unwrap();

        // Account plain_key (20 bytes) → should be in CF_INDEX.
        let acct_key = account_plain_key(&addr);
        let salt_key = store
            .plain_value_fast(&acct_key)
            .expect("account plain_key must be in CF_INDEX after update_state");
        // The SaltKey must resolve to the correct SaltValue.
        let val = store.value(salt_key).unwrap().expect("SaltKey must exist in CF_STATE");
        assert_eq!(val.key(), acct_key.as_slice(), "val.key() must round-trip to plain_key");

        // Storage plain_key (52 bytes) → should also be in CF_INDEX.
        let storage_key = storage_plain_key(&addr, &slot);
        let salt_key_s = store
            .plain_value_fast(&storage_key)
            .expect("storage plain_key must be in CF_INDEX after update_state");
        let val_s = store.value(salt_key_s).unwrap().expect("SaltKey must exist in CF_STATE");
        assert_eq!(val_s.key(), storage_key.as_slice(), "storage val.key() must round-trip");

        // Unknown key → must return Err (not a panic, not a wrong result).
        let unknown = account_plain_key(&Address::from([0xFF; 20]));
        assert!(store.plain_value_fast(&unknown).is_err(), "unknown key must return Err");
    }

    /// Verify that deleting an account removes it from CF_INDEX.
    #[test]
    fn test_plain_value_fast_removed_after_delete() {
        let tmp = TempDir::new().unwrap();
        let store = RocksSaltStore::new(tmp.path()).unwrap();

        let addr = Address::from([0xCD; 20]);
        let bundle_insert = make_bundle(vec![(addr, vec![])]);

        // Insert the account.
        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle_insert);
        let mut eph = EphemeralSaltState::new(&store);
        let su = eph.update_fin(&kvs).unwrap();
        store.update_state(su).unwrap();

        let acct_key = account_plain_key(&addr);
        assert!(store.plain_value_fast(&acct_key).is_ok(), "should be findable after insert");

        // Now destroy the account (status=Destroyed → account key = None).
        let mut state = PrimitivesHashMap::default();
        state.insert(
            addr,
            revm_database::BundleAccount {
                info: None,
                original_info: None,
                status: AccountStatus::Destroyed,
                storage: StorageWithOriginalValues::default(),
            },
        );
        let bundle_del = revm_database::BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        };

        let kvs2 = crate::convert::bundle_state_to_plain_kv(&bundle_del);
        let mut eph2 = EphemeralSaltState::new(&store);
        let su2 = eph2.update_fin(&kvs2).unwrap();
        store.update_state(su2).unwrap();

        // After deletion the key must no longer be in CF_INDEX.
        assert!(
            store.plain_value_fast(&acct_key).is_err(),
            "destroyed account must be removed from CF_INDEX"
        );
    }

    /// Verify that the full pipeline (EphemeralSaltState + StateRoot) produces a
    /// non-zero root and that the trie updates round-trip through `update_trie`.
    #[test]
    fn test_full_pipeline_with_rocks_store() {
        let tmp = TempDir::new().unwrap();
        let store = RocksSaltStore::new(tmp.path()).unwrap();

        let addr = Address::from([0x11; 20]);
        let slot = B256::from([0x22; 32]);
        let bundle = make_bundle(vec![(addr, vec![(slot, U256::from(99u64))])]);

        let kvs = crate::convert::bundle_state_to_plain_kv(&bundle);
        let mut eph = EphemeralSaltState::new(&store);
        let state_updates = eph.update_fin(&kvs).unwrap();
        store.update_state(state_updates.clone()).unwrap();

        let mut root_engine = StateRoot::new(&store);
        let (root_bytes, trie_updates) = root_engine.update_fin(&state_updates).unwrap();
        store.update_trie(trie_updates).unwrap();

        let root = B256::from(root_bytes);
        assert_ne!(root, B256::ZERO, "state root must be non-zero");
    }
}

impl TrieReader for RocksSaltStore {
    type Error = SaltError;

    fn commitment(&self, node_id: NodeId) -> Result<CommitmentBytes, Self::Error> {
        let cf = self.db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        if let Ok(Some(bytes)) = self.db.get_cf(&cf, node_id.to_be_bytes()) {
            if bytes.len() == 64 {
                let mut commitment = [0u8; 64];
                commitment.copy_from_slice(&bytes);
                return Ok(commitment);
            }
        }
        Ok(default_commitment(node_id))
    }

    fn node_entries(
        &self,
        range: Range<NodeId>,
    ) -> Result<Vec<(NodeId, CommitmentBytes)>, Self::Error> {
        let cf = self.db.cf_handle(CF_TRIE).expect("CF_TRIE must exist");
        let start = range.start.to_be_bytes();
        let end = range.end.to_be_bytes();

        let mut result = Vec::new();
        let iter = self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward));
        for item in iter.flatten() {
            let (k, v) = item;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            let node_id = u64::from_be_bytes(k.as_ref().try_into().unwrap_or([0; 8]));
            if v.len() == 64 {
                let mut commitment = [0u8; 64];
                commitment.copy_from_slice(&v);
                result.push((node_id, commitment));
            }
        }
        Ok(result)
    }
}
