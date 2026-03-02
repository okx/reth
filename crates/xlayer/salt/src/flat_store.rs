//! Flat-file persistent storage for SALT.
//!
//! A simple disk-backed KV store:
//! - **Reads**: served from an in-memory `BTreeMap` (equivalent to any production store's block
//!   cache / memtable — MDBX also serves reads from mmap page cache)
//! - **Writes**: serialized into a contiguous buffer, then written with **truncate** (overwrite,
//!   not append): each `update_state` replaces the file with the current block's delta and
//!   `fsync`'s. This represents minimal I/O for flat state persistence.
//!
//! This avoids MDBX's B-tree page management, copy-on-write, and freelist overhead,
//! which are irrelevant to SALT's simple flat KV workload.

use salt::{
    constant::default_commitment,
    traits::{StateReader, TrieReader},
    types::*,
    StateUpdates, TrieUpdates,
};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{File, OpenOptions},
    io::Write,
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
    sync::RwLock,
    time::{Duration, Instant},
};

/// Write statistics for a single block's persistence.
#[derive(Debug)]
pub struct WriteStats {
    /// Number of state entries written.
    pub entries: usize,
    /// Bytes written to disk.
    pub bytes_written: usize,
    /// Time for serialization + file write + fsync.
    pub persist_duration: Duration,
}

/// Flat-file backed SALT store.
///
/// State and trie data are cached in memory for reads. Writes go to a flat
/// file with truncate (overwrite) + `fsync` for durability — the minimal disk
/// I/O for crash-safe flat KV persistence.
pub struct FlatFileStore {
    state: RwLock<StateStore>,
    trie: RwLock<BTreeMap<NodeId, CommitmentBytes>>,
    state_path: PathBuf,
}

#[derive(Default)]
struct StateStore {
    kvs: BTreeMap<SaltKey, SaltValue>,
    used_slots: HashMap<BucketId, u64>,
}

impl std::fmt::Debug for FlatFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatFileStore").field("path", &self.state_path).finish()
    }
}

impl FlatFileStore {
    /// Creates a new flat-file store at the given directory.
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let state_path = dir.join("salt_flat_state.bin");
        // Create/truncate the state file
        File::create(&state_path)?;
        Ok(Self {
            state: RwLock::new(StateStore::default()),
            trie: RwLock::new(BTreeMap::new()),
            state_path,
        })
    }

    /// Applies state updates: updates in-memory cache, then writes to disk with fsync.
    pub fn update_state(&self, updates: StateUpdates) -> std::io::Result<WriteStats> {
        let num_entries = updates.data.len();
        let t0 = Instant::now();

        // Serialize all changes into a contiguous buffer
        let mut buf = Vec::with_capacity(num_entries * 100);
        for (key, (_, new_val)) in &updates.data {
            buf.extend_from_slice(&key.0.to_be_bytes());
            match new_val {
                Some(val) => {
                    let data_len = val.data_len();
                    buf.push(data_len as u8);
                    buf.extend_from_slice(&val.data[..data_len]);
                }
                None => {
                    buf.push(0); // deletion marker
                }
            }
        }

        // Sequential write + fsync — the minimum disk I/O for durability
        let mut file = OpenOptions::new().write(true).truncate(true).open(&self.state_path)?;
        file.write_all(&buf)?;
        file.sync_all()?;

        let bytes_written = buf.len();
        let persist_duration = t0.elapsed();

        // Update in-memory state (after successful disk write)
        let mut state = self.state.write().unwrap();
        for (key, (old_value, new_value)) in updates.data {
            if !key.is_in_meta_bucket() {
                let delta: i64 = match (old_value.is_some(), new_value.is_some()) {
                    (false, true) => 1,
                    (true, false) => -1,
                    _ => 0,
                };
                if delta != 0 {
                    let count = state.used_slots.entry(key.bucket_id()).or_insert(0);
                    *count = (*count as i64 + delta).max(0) as u64;
                }
            }
            match new_value {
                Some(val) => state.kvs.insert(key, val),
                None => state.kvs.remove(&key),
            };
        }

        Ok(WriteStats { entries: num_entries, bytes_written, persist_duration })
    }

    /// Applies trie updates to memory only (trie lives in memory per SALT design).
    pub fn update_trie(&self, updates: TrieUpdates) {
        let mut trie = self.trie.write().unwrap();
        for (node_id, (_, new_val)) in updates {
            trie.insert(node_id, new_val);
        }
    }
}

impl StateReader for FlatFileStore {
    type Error = SaltError;

    fn value(&self, key: SaltKey) -> Result<Option<SaltValue>, Self::Error> {
        Ok(self.state.read().unwrap().kvs.get(&key).cloned())
    }

    fn entries(
        &self,
        range: RangeInclusive<SaltKey>,
    ) -> Result<Vec<(SaltKey, SaltValue)>, Self::Error> {
        Ok(self.state.read().unwrap().kvs.range(range).map(|(k, v)| (*k, v.clone())).collect())
    }

    fn metadata(&self, bucket_id: BucketId) -> Result<BucketMeta, Self::Error> {
        let key = bucket_metadata_key(bucket_id);
        let state = self.state.read().unwrap();
        let mut meta = match state.kvs.get(&key) {
            Some(v) => v.try_into()?,
            None => BucketMeta::default(),
        };
        meta.used = Some(*state.used_slots.get(&bucket_id).unwrap_or(&0));
        Ok(meta)
    }

    fn bucket_used_slots(&self, bucket_id: BucketId) -> Result<u64, Self::Error> {
        if !is_valid_data_bucket(bucket_id) {
            return Ok(0);
        }
        Ok(*self.state.read().unwrap().used_slots.get(&bucket_id).unwrap_or(&0))
    }

    fn plain_value_fast(&self, _plain_key: &[u8]) -> Result<SaltKey, Self::Error> {
        Err(SaltError::UnsupportedOperation { operation: "FlatFileStore::plain_value_fast" })
    }
}

impl TrieReader for FlatFileStore {
    type Error = SaltError;

    fn commitment(&self, node_id: NodeId) -> Result<CommitmentBytes, Self::Error> {
        Ok(self
            .trie
            .read()
            .unwrap()
            .get(&node_id)
            .copied()
            .unwrap_or_else(|| default_commitment(node_id)))
    }

    fn node_entries(
        &self,
        range: Range<NodeId>,
    ) -> Result<Vec<(NodeId, CommitmentBytes)>, Self::Error> {
        Ok(self.trie.read().unwrap().range(range).map(|(k, v)| (*k, *v)).collect())
    }
}
