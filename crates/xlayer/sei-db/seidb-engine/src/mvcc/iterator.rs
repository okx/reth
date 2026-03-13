use crate::mvcc::{
    db::{prepend_store_key, store_prefix, MvccDatabase},
    encoding::{decode_uint64_ascending, mvcc_encode, split_mvcc_key, split_mvcc_value},
};
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::{
    iterator::DbIterator,
    kv::{KvEngine, KvIterator},
    types::IterOptions,
};

/// MVCC version-aware iterator over a store's key space.
///
/// Iterates over user keys at a specific version, skipping keys that either
/// don't have a version <= the target or that have been tombstoned. Supports
/// both forward and reverse iteration.
///
/// Internally uses a `Box<dyn KvIterator>` for backend-agnostic iteration,
/// and caches the decoded user key and value in owned `Vec<u8>` fields so the
/// `DbIterator` trait (which returns `&[u8]`) can be satisfied.
#[allow(dead_code)]
pub(crate) struct MvccIterator {
    /// The underlying KvEngine raw iterator.
    raw: Box<dyn KvIterator>,
    /// Store name (e.g. "bank").
    #[allow(dead_code)]
    store_key: String,
    /// Store prefix bytes: `s/k:{store}/`.
    prefix: Vec<u8>,
    /// Original start bound (user key, without store prefix).
    start: Option<Vec<u8>>,
    /// Original end bound (user key, without store prefix).
    end: Option<Vec<u8>>,
    /// Target MVCC version.
    version: i64,
    /// Whether this is a reverse iterator.
    reverse: bool,
    /// Whether the iterator is currently positioned at a valid entry.
    is_valid: bool,
    /// Current user key (store prefix stripped, MVCC decoded).
    cached_key: Vec<u8>,
    /// Current value (tombstone stripped).
    cached_value: Vec<u8>,
    /// Current error, if any.
    cached_error: Option<SeiDbError>,
}

#[allow(dead_code)]
impl MvccIterator {
    /// Create a new MVCC iterator over the given store's key space.
    ///
    /// - `start`/`end` are user key bounds (without store prefix).
    /// - `version` is the target MVCC version; only entries with version <= this are visible.
    /// - `earliest_version`: if `version < earliest_version`, the iterator is immediately invalid.
    /// - `reverse`: if true, iterate in descending key order.
    pub(crate) fn new(
        engine: &dyn KvEngine,
        store_key: &str,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        version: i64,
        earliest_version: i64,
        reverse: bool,
    ) -> Result<Self> {
        let prefix = store_prefix(store_key);

        // Compute MVCC-encoded lower bound.
        let mvcc_start = if let Some(s) = start {
            mvcc_encode(&prepend_store_key(store_key, s), 0)
        } else {
            mvcc_encode(&prefix, 0)
        };

        // Compute MVCC-encoded upper bound.
        let mvcc_end = if let Some(e) = end {
            mvcc_encode(&prepend_store_key(store_key, e), 0)
        } else if let Some(pe) = MvccDatabase::prefix_end(&prefix) {
            mvcc_encode(&pe, 0)
        } else {
            vec![]
        };

        let iter_opts = IterOptions {
            lower_bound: Some(mvcc_start),
            upper_bound: if !mvcc_end.is_empty() { Some(mvcc_end) } else { None },
        };

        let raw = engine.new_iter(&iter_opts)?;

        let mut itr = Self {
            raw,
            store_key: store_key.to_string(),
            prefix,
            start: start.map(|s| s.to_vec()),
            end: end.map(|e| e.to_vec()),
            version,
            reverse,
            is_valid: false,
            cached_key: Vec::new(),
            cached_value: Vec::new(),
            cached_error: None,
        };

        // Return invalid iterator if requested version is below the earliest version
        if version < earliest_version {
            return Ok(itr);
        }

        // Initial positioning
        if reverse {
            itr.raw.last();
        } else {
            itr.raw.first();
        }

        if !itr.raw.valid() {
            return Ok(itr);
        }

        // Get the current key and check its version
        let cur_user_key = match itr.current_user_key() {
            Some(k) => k,
            None => return Ok(itr),
        };

        let cur_version = match itr.current_key_version() {
            Some(v) => v,
            None => return Ok(itr),
        };

        if cur_version > itr.version {
            // Current key's version is too high; advance to find a valid entry
            itr.is_valid = true;
            if reverse {
                itr.next_reverse();
            } else {
                itr.next_forward();
            }
        } else {
            // Seek to the latest version <= target for this user key.
            let seek_key = mvcc_encode(&cur_user_key, itr.version + 1);
            itr.seek_lt(&seek_key);
            itr.is_valid = itr.raw.valid();
        }

        // Skip tombstoned entries
        if itr.is_valid && itr.cursor_tombstoned() {
            if reverse {
                itr.next_reverse();
            } else {
                itr.next_forward();
            }
        }

        // Cache the current key/value if valid
        if itr.is_valid {
            itr.update_cache();
        }

        Ok(itr)
    }

    /// Extract the full user key (with store prefix) from the current raw iterator position.
    fn current_user_key(&self) -> Option<Vec<u8>> {
        if !self.raw.valid() {
            return None;
        }
        let raw_key = self.raw.key();
        if raw_key.is_empty() {
            return None;
        }
        let (user_key, _) = split_mvcc_key(raw_key)?;
        Some(user_key.to_vec())
    }

    /// Decode the version from the current raw iterator key.
    fn current_key_version(&self) -> Option<i64> {
        if !self.raw.valid() {
            return None;
        }
        let raw_key = self.raw.key();
        if raw_key.is_empty() {
            return None;
        }
        let (_, version_bytes) = split_mvcc_key(raw_key)?;
        match version_bytes {
            Some(vb) => decode_uint64_ascending(vb).ok(),
            None => Some(0),
        }
    }

    /// Check if the current cursor position points to a tombstoned entry.
    fn cursor_tombstoned(&self) -> bool {
        if !self.raw.valid() {
            return false;
        }
        let raw_val = self.raw.value();
        if raw_val.is_empty() {
            return false;
        }
        let (_, tomb_bytes) = match split_mvcc_value(raw_val) {
            Some(v) => v,
            None => return false,
        };
        let tomb_bytes = match tomb_bytes {
            Some(tb) if !tb.is_empty() => tb,
            _ => return false,
        };
        match decode_uint64_ascending(tomb_bytes) {
            Ok(tombstone) => tombstone <= self.version,
            Err(_) => false,
        }
    }

    /// Update the cached key and value from the current raw iterator position.
    fn update_cache(&mut self) {
        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        // Decode and cache the user key (strip store prefix)
        let raw_key = self.raw.key();
        if raw_key.is_empty() {
            self.is_valid = false;
            return;
        }
        let (user_key, _) = match split_mvcc_key(raw_key) {
            Some(v) => v,
            None => {
                self.is_valid = false;
                self.cached_error =
                    Some(SeiDbError::Other("invalid MVCC key in iterator".to_string()));
                return;
            }
        };
        if user_key.starts_with(&self.prefix) {
            self.cached_key = user_key[self.prefix.len()..].to_vec();
        } else {
            self.cached_key = user_key.to_vec();
        }

        // Decode and cache the value (strip tombstone suffix)
        let raw_val = self.raw.value();
        if raw_val.is_empty() {
            self.is_valid = false;
            return;
        }
        let (val, _) = match split_mvcc_value(raw_val) {
            Some(v) => v,
            None => {
                self.is_valid = false;
                self.cached_error =
                    Some(SeiDbError::Other("invalid MVCC value in iterator".to_string()));
                return;
            }
        };
        self.cached_value = val.to_vec();
    }

    /// Simulate PebbleDB's `SeekLT(target)`: find the largest key strictly
    /// less than `target`.
    fn seek_lt(&mut self, target: &[u8]) {
        self.raw.seek_lt(target);
    }

    /// Simulate PebbleDB's `NextPrefix()`: advance past all versions of the
    /// current user key by seeking to a key that sorts after all MVCC-encoded
    /// versions of `user_key`.
    fn seek_next_user_key_forward(&mut self, user_key: &[u8]) {
        let mut next_key = user_key.to_vec();
        next_key.push(0x00);
        let seek_target = mvcc_encode(&next_key, 0);
        self.raw.seek_ge(&seek_target);
    }

    /// Move the forward iterator to the next visible (non-tombstoned) user key.
    fn next_forward(&mut self) {
        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        let curr_key = match self.current_user_key() {
            Some(k) => k,
            None => {
                self.is_valid = false;
                return;
            }
        };

        // Move past all versions of the current user key
        self.seek_next_user_key_forward(&curr_key);

        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        let next_key = match self.current_user_key() {
            Some(k) => k,
            None => {
                self.is_valid = false;
                return;
            }
        };

        // The next key must still have our store prefix
        if !next_key.starts_with(&self.prefix) {
            self.is_valid = false;
            return;
        }

        // Seek to the latest version <= target for this new user key
        let seek_key = mvcc_encode(&next_key, self.version + 1);
        self.seek_lt(&seek_key);

        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        let tmp_key = match self.current_user_key() {
            Some(k) => k,
            None => {
                self.is_valid = false;
                return;
            }
        };

        // seek_lt may have moved us back to the previous user key
        if tmp_key == curr_key {
            self.seek_next_user_key_forward(&curr_key);
            if !self.raw.valid() {
                self.is_valid = false;
                return;
            }
            self.next_forward();
            return;
        }

        // Verify version constraint
        let tmp_version = match self.current_key_version() {
            Some(v) => v,
            None => {
                self.is_valid = false;
                return;
            }
        };

        if tmp_version > self.version {
            self.next_forward();
            return;
        }

        self.is_valid = true;

        // Skip tombstoned entries
        if self.cursor_tombstoned() {
            self.next_forward();
        }
    }

    /// Move the reverse iterator to the previous visible (non-tombstoned) user key.
    fn next_reverse(&mut self) {
        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        let curr_key = match self.current_user_key() {
            Some(k) => k,
            None => {
                self.is_valid = false;
                return;
            }
        };

        // Seek before all versions of the current user key.
        let seek_key = mvcc_encode(&curr_key, 0);
        self.seek_lt(&seek_key);

        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        let next_key = match self.current_user_key() {
            Some(k) => k,
            None => {
                self.is_valid = false;
                return;
            }
        };

        // Must still be within the store prefix
        if !next_key.starts_with(&self.prefix) {
            self.is_valid = false;
            return;
        }

        // Seek to the latest version <= target for this user key
        let seek_key = mvcc_encode(&next_key, self.version + 1);
        self.seek_lt(&seek_key);

        if !self.raw.valid() {
            self.is_valid = false;
            return;
        }

        // Verify version constraint
        let tmp_version = match self.current_key_version() {
            Some(v) => v,
            None => {
                self.is_valid = false;
                return;
            }
        };

        if tmp_version > self.version {
            self.next_reverse();
            return;
        }

        self.is_valid = true;

        // Skip tombstoned entries
        if self.cursor_tombstoned() {
            self.next_reverse();
        }
    }
}

impl DbIterator for MvccIterator {
    fn domain(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        (self.start.as_deref(), self.end.as_deref())
    }

    fn valid(&self) -> bool {
        self.is_valid
    }

    fn next(&mut self) {
        if self.reverse {
            self.next_reverse();
        } else {
            self.next_forward();
        }

        if self.is_valid {
            self.update_cache();
        }
    }

    fn key(&self) -> &[u8] {
        &self.cached_key
    }

    fn value(&self) -> &[u8] {
        &self.cached_value
    }

    fn error(&self) -> Option<&SeiDbError> {
        self.cached_error.as_ref()
    }

    fn close(&mut self) -> Result<()> {
        self.is_valid = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::RocksDbEngine,
        mvcc::{
            comparator::{mvcc_comparator_name, mvcc_compare_fn},
            encoding::mvcc_encode_value,
        },
    };
    use seidb_traits::types::WriteOptions;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Open a RocksDB engine with the MVCC comparator for testing.
    fn open_test_engine(dir: &std::path::Path) -> Arc<dyn KvEngine> {
        let comparator_fn = Some((
            mvcc_comparator_name().to_string(),
            Box::new(mvcc_compare_fn)
                as Box<dyn Fn(&[u8], &[u8]) -> std::cmp::Ordering + Send + Sync>,
        ));
        let engine = RocksDbEngine::open_mvcc(dir, comparator_fn).unwrap();
        Arc::new(engine)
    }

    /// Write an MVCC key/value directly into the engine.
    fn write_mvcc(engine: &dyn KvEngine, store: &str, key: &[u8], val: &[u8], ver: i64) {
        let k = mvcc_encode(&prepend_store_key(store, key), ver);
        let v = mvcc_encode_value(val, 0);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    /// Write an MVCC tombstone directly into the engine.
    fn write_tombstone(engine: &dyn KvEngine, store: &str, key: &[u8], ver: i64) {
        let k = mvcc_encode(&prepend_store_key(store, key), ver);
        let v = mvcc_encode_value(b"TOMBSTONE", ver);
        engine.set(&k, &v, &WriteOptions::default()).unwrap();
    }

    #[test]
    fn test_forward_iterator() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b", 1);
        write_mvcc(engine.as_ref(), "store", b"c", b"val_c", 1);

        let mut iter =
            MvccIterator::new(engine.as_ref(), "store", None, None, 1, 0, false).unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_forward_iterator_multi_version() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v1", 1);
        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v2", 2);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b_v1", 1);

        let mut iter =
            MvccIterator::new(engine.as_ref(), "store", None, None, 1, 0, false).unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a_v1");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b_v1");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_forward_iterator_tombstone_skip() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);
        write_tombstone(engine.as_ref(), "store", b"a", 2);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b", 1);

        let mut iter =
            MvccIterator::new(engine.as_ref(), "store", None, None, 2, 0, false).unwrap();

        // "a" is tombstoned at version 2, so only "b" should be visible
        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_reverse_iterator() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b", 1);
        write_mvcc(engine.as_ref(), "store", b"c", b"val_c", 1);

        let mut iter = MvccIterator::new(engine.as_ref(), "store", None, None, 1, 0, true).unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_reverse_iterator_multi_version() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v1", 1);
        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v2", 2);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b_v1", 1);

        let mut iter = MvccIterator::new(engine.as_ref(), "store", None, None, 1, 0, true).unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b_v1");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a_v1");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_bounds() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b", 1);
        write_mvcc(engine.as_ref(), "store", b"c", b"val_c", 1);
        write_mvcc(engine.as_ref(), "store", b"d", b"val_d", 1);

        // start=b, end=d -> should yield b, c (end is exclusive via upper bound)
        let mut iter =
            MvccIterator::new(engine.as_ref(), "store", Some(b"b"), Some(b"d"), 1, 0, false)
                .unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"val_b");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_empty_range() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b", 1);

        // Range [x, z) has no keys
        let iter = MvccIterator::new(engine.as_ref(), "store", Some(b"x"), Some(b"z"), 1, 0, false)
            .unwrap();

        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_close_idempotent() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);

        let mut iter =
            MvccIterator::new(engine.as_ref(), "store", None, None, 1, 0, false).unwrap();

        assert!(iter.valid());
        iter.close().unwrap();
        assert!(!iter.valid());
        // Close again — should not panic
        iter.close().unwrap();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_version_below_earliest() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);

        // earliest_version=5, requested version=1 -> invalid immediately
        let iter = MvccIterator::new(engine.as_ref(), "store", None, None, 1, 5, false).unwrap();

        assert!(!iter.valid());
    }

    #[test]
    fn test_forward_iterator_latest_version() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v1", 1);
        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v2", 2);
        write_mvcc(engine.as_ref(), "store", b"a", b"val_a_v3", 3);

        // At version 2, should see val_a_v2
        let iter = MvccIterator::new(engine.as_ref(), "store", None, None, 2, 0, false).unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a_v2");
    }

    #[test]
    fn test_reverse_iterator_tombstone_skip() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        write_mvcc(engine.as_ref(), "store", b"a", b"val_a", 1);
        write_mvcc(engine.as_ref(), "store", b"b", b"val_b", 1);
        write_tombstone(engine.as_ref(), "store", b"b", 2);
        write_mvcc(engine.as_ref(), "store", b"c", b"val_c", 1);

        // Reverse at version 2: b is tombstoned, should see c, a
        let mut iter = MvccIterator::new(engine.as_ref(), "store", None, None, 2, 0, true).unwrap();

        assert!(iter.valid());
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"val_c");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"val_a");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_domain() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        let iter =
            MvccIterator::new(engine.as_ref(), "store", Some(b"start"), Some(b"end"), 1, 0, false)
                .unwrap();

        let (start, end) = iter.domain();
        assert_eq!(start, Some(b"start".as_slice()));
        assert_eq!(end, Some(b"end".as_slice()));
    }

    #[test]
    fn test_domain_no_bounds() {
        let dir = TempDir::new().unwrap();
        let engine = open_test_engine(dir.path());

        let iter = MvccIterator::new(engine.as_ref(), "store", None, None, 1, 0, false).unwrap();

        let (start, end) = iter.domain();
        assert!(start.is_none());
        assert!(end.is_none());
    }
}
