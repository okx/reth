/// Stale Root Index — incremental GC helper.
///
/// Tracks state-root hashes that have been removed from the manifest (i.e.
/// their version was pruned via `prune_before`).  A subsequent `gc()` call
/// only needs to BFS from these stale roots rather than scanning every node
/// in the persisted store.
///
/// # Storage layout
///
/// Backed by a small dedicated RocksDB instance (separate from the main
/// `trie_nodes` store).
///
/// Key:   `stale_since_version (i64, 8 bytes big-endian) || state_root (B256, 32 bytes)`
/// Value: empty `[]`
///
/// The big-endian version prefix means RocksDB keeps entries sorted by
/// version, making range scans for "all entries before version V" cheap.
use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use mptdb_common::error::{MptDbError, Result};
use mptdb_engine::engine::RocksDbEngine;
use mptdb_traits::{
    kv::KvEngine,
    types::{IterOptions, WriteOptions},
};
use std::path::Path;

pub struct StaleRootIndex {
    engine: RocksDbEngine,
}

impl StaleRootIndex {
    /// Open (or create) the stale index at `path`.
    ///
    /// Uses a 16 MB block cache instead of `open_plain`'s default 1 GB because:
    ///   - The stale index stores only 40-byte keys with empty values.
    ///   - It is accessed only on low-frequency GC / prune paths.
    ///   - A 1 GB cache would directly compete with the main trie_nodes DB.
    /// All other RocksDB parameters remain identical to `open_plain`.
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .map_err(|e| MptDbError::Other(format!("create stale_index dir: {e}")))?;
        let engine = RocksDbEngine::open_plain_with_cache_mb(path, 16)?;
        Ok(Self { engine })
    }

    /// Record that `state_root` became stale starting from `version`.
    ///
    /// Skips `EMPTY_ROOT_HASH` — the empty trie has no persisted nodes.
    /// Uses `sync: false`; durability is provided by the caller's manifest
    /// fsync (the manifest save happens after this call on the same thread).
    pub fn record_stale_root(&self, version: i64, state_root: B256) -> Result<()> {
        if state_root == EMPTY_ROOT_HASH {
            return Ok(());
        }
        let key = encode_key(version, &state_root);
        self.engine.set(&key, &[], &WriteOptions { sync: false })
    }

    /// Delete index entries whose `stale_since_version < prune_before_version`.
    ///
    /// Called after a successful incremental GC to keep the index lean.
    pub fn remove_entries_before(&self, prune_before_version: i64) -> Result<()> {
        // Upper bound key: (prune_before_version, 00..00) — exclusive upper
        let upper = encode_key(prune_before_version, &B256::ZERO);
        let opts = IterOptions { lower_bound: None, upper_bound: Some(upper.to_vec()) };
        let mut iter = self.engine.new_iter(&opts)?;

        let mut keys_to_delete: Vec<Vec<u8>> = Vec::new();
        if iter.first() {
            loop {
                keys_to_delete.push(iter.key().to_vec());
                if !iter.next() {
                    break;
                }
            }
        }
        iter.close()?;

        if keys_to_delete.is_empty() {
            return Ok(());
        }

        let mut batch = self.engine.new_batch();
        for key in &keys_to_delete {
            batch.delete(key)?;
        }
        batch.commit(&WriteOptions { sync: true })?;
        Ok(())
    }

    /// Collect all stale root hashes recorded in the index.
    ///
    /// Returns deduplicated roots; the same root can theoretically appear under
    /// multiple versions if a given state root was re-committed then pruned
    /// again, but deduplication in the BFS (`HashSet`) handles that.
    pub fn collect_stale_roots(&self) -> Result<Vec<B256>> {
        let opts = IterOptions { lower_bound: None, upper_bound: None };
        let mut iter = self.engine.new_iter(&opts)?;
        let mut roots: Vec<B256> = Vec::new();

        if iter.first() {
            loop {
                let key = iter.key();
                if key.len() == KEY_LEN {
                    roots.push(B256::from_slice(&key[VERSION_BYTES..]));
                }
                if !iter.next() {
                    break;
                }
            }
        }
        iter.close()?;
        Ok(roots)
    }

    /// Return true if there are no stale root entries.
    pub fn is_empty(&self) -> Result<bool> {
        let opts = IterOptions { lower_bound: None, upper_bound: None };
        let mut iter = self.engine.new_iter(&opts)?;
        let has = iter.first();
        iter.close()?;
        Ok(!has)
    }
}

// ---------------------------------------------------------------------------
// Key encoding helpers
// ---------------------------------------------------------------------------

const VERSION_BYTES: usize = 8;
const ROOT_BYTES: usize = 32;
const KEY_LEN: usize = VERSION_BYTES + ROOT_BYTES;

fn encode_key(version: i64, root: &B256) -> [u8; KEY_LEN] {
    let mut key = [0u8; KEY_LEN];
    key[..VERSION_BYTES].copy_from_slice(&version.to_be_bytes());
    key[VERSION_BYTES..].copy_from_slice(root.as_slice());
    key
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use tempfile::TempDir;

    fn tmp_index() -> (StaleRootIndex, TempDir) {
        let dir = TempDir::new().unwrap();
        let idx = StaleRootIndex::open(dir.path()).unwrap();
        (idx, dir)
    }

    #[test]
    fn empty_on_open() {
        let (idx, _dir) = tmp_index();
        assert!(idx.is_empty().unwrap());
        assert!(idx.collect_stale_roots().unwrap().is_empty());
    }

    #[test]
    fn record_and_collect() {
        let (idx, _dir) = tmp_index();
        let r1 = B256::repeat_byte(0x11);
        let r2 = B256::repeat_byte(0x22);
        idx.record_stale_root(10, r1).unwrap();
        idx.record_stale_root(20, r2).unwrap();

        assert!(!idx.is_empty().unwrap());
        let roots = idx.collect_stale_roots().unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&r1));
        assert!(roots.contains(&r2));
    }

    #[test]
    fn skip_empty_root_hash() {
        let (idx, _dir) = tmp_index();
        idx.record_stale_root(1, EMPTY_ROOT_HASH).unwrap();
        assert!(idx.is_empty().unwrap());
    }

    #[test]
    fn remove_entries_before() {
        let (idx, _dir) = tmp_index();
        let r1 = B256::repeat_byte(0xaa);
        let r2 = B256::repeat_byte(0xbb);
        let r3 = B256::repeat_byte(0xcc);
        idx.record_stale_root(5, r1).unwrap();
        idx.record_stale_root(10, r2).unwrap();
        idx.record_stale_root(15, r3).unwrap();

        // Remove entries with version < 10 (i.e. version 5)
        idx.remove_entries_before(10).unwrap();
        let roots = idx.collect_stale_roots().unwrap();
        assert!(!roots.contains(&r1), "version 5 should be removed");
        assert!(roots.contains(&r2), "version 10 should be kept");
        assert!(roots.contains(&r3), "version 15 should be kept");
    }

    #[test]
    fn idempotent_record() {
        let (idx, _dir) = tmp_index();
        let r = B256::repeat_byte(0x55);
        idx.record_stale_root(1, r).unwrap();
        idx.record_stale_root(1, r).unwrap(); // duplicate key — RocksDB overwrites
        let roots = idx.collect_stale_roots().unwrap();
        assert_eq!(roots.len(), 1);
    }
}
