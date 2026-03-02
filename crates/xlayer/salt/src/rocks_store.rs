//! RocksDB-backed persistent storage for SALT.
//!
//! Uses RocksDB (LSM-tree) — the industry-standard KV store for blockchain nodes.
//! LSM-tree architecture is well-suited for SALT's write-heavy flat-state workload:
//! - **Writes**: go to memtable (in-memory) + WAL (sequential I/O) → very fast
//! - **Reads**: block cache (memory) → SST files (disk) with bloom filters
//! - **No COW/page overhead**: unlike MDBX's B-tree, no copy-on-write or page management

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

/// 256MB block cache — for ~140MB+ state data, need >data size to get hot reads (avoids delta ~90ms
/// → ~5–15ms).
const BLOCK_CACHE_SIZE: usize = 256 * 1024 * 1024;

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
    /// Uses a 256MB block cache so that ~140MB+ state fits in cache and delta (read path) stays
    /// fast.
    pub fn new(path: &Path) -> Result<Self, rocksdb::Error> {
        let block_cache = Cache::new_lru_cache(BLOCK_CACHE_SIZE);
        let mut table_opts = BlockBasedOptions::default();
        table_opts.set_block_cache(&block_cache);

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

        let cf_descriptors = vec![
            ColumnFamilyDescriptor::new(CF_STATE, cf_state_opts),
            ColumnFamilyDescriptor::new(CF_TRIE, cf_trie_opts),
        ];

        let db = DB::open_cf_descriptors(&opts, path, cf_descriptors)?;
        Ok(Self { db, _block_cache: block_cache, slot_counts: Mutex::new(HashMap::new()) })
    }

    /// Applies state updates: writes a batch to RocksDB with WAL sync.
    pub fn update_state(&self, updates: StateUpdates) -> Result<WriteStats, rocksdb::Error> {
        let t0 = Instant::now();
        let cf = self.db.cf_handle(CF_STATE).expect("CF_STATE must exist");

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

        // Build write batch
        let num_entries = updates.data.len();
        for (key, (_old, new_value)) in updates.data {
            let key_bytes = key.0.to_be_bytes();
            match new_value {
                Some(val) => {
                    let data_len = val.data_len();
                    batch.put_cf(&cf, &key_bytes, &val.data[..data_len]);
                    bytes += 8 + data_len;
                }
                None => {
                    batch.delete_cf(&cf, &key_bytes);
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

    fn plain_value_fast(&self, _plain_key: &[u8]) -> Result<SaltKey, Self::Error> {
        Err(SaltError::UnsupportedOperation { operation: "RocksSaltStore::plain_value_fast" })
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
