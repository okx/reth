use mptdb_common::error::{MptDbError, Result};
use mptdb_traits::{kv::KvIterator, types::IterOptions};
use rocksdb::{DBRawIteratorWithThreadMode, ReadOptions};
use std::sync::Arc;

/// RocksDB iterator wrapping a `DBRawIteratorWithThreadMode`.
///
/// Holds an `Arc<DB>` to guarantee the database outlives the raw iterator.
/// The raw iterator is created with a transmuted `'static` lifetime, which is
/// sound because the `Arc<DB>` prevents the DB from being dropped while this
/// struct is alive.
pub struct RocksDbIterator {
    /// Prevent the DB from being dropped while the raw iterator is alive.
    _db: Arc<rocksdb::DB>,
    /// Raw iterator over the DB. The actual lifetime is tied to `_db`, but we
    /// use `'static` via transmute since we guarantee `_db` outlives `raw`.
    raw: DBRawIteratorWithThreadMode<'static, rocksdb::DB>,
    valid: bool,
    err: Option<MptDbError>,
}

// SAFETY: RocksDB's raw iterator is thread-safe for read-only operations when
// the underlying DB is kept alive via Arc. The DB itself is Send + Sync.
unsafe impl Send for RocksDbIterator {}

impl RocksDbIterator {
    /// Create a new iterator over the given DB with the specified bounds.
    pub fn new(db: Arc<rocksdb::DB>, opts: &IterOptions) -> Result<Self> {
        let mut read_opts = ReadOptions::default();
        if let Some(ref lb) = opts.lower_bound {
            read_opts.set_iterate_lower_bound(lb.clone());
        }
        if let Some(ref ub) = opts.upper_bound {
            read_opts.set_iterate_upper_bound(ub.clone());
        }

        // SAFETY: The raw iterator borrows &DB immutably. We hold an Arc<DB>
        // in the struct so the DB cannot be dropped before the iterator.
        let raw = db.raw_iterator_opt(read_opts);
        let raw: DBRawIteratorWithThreadMode<'static, rocksdb::DB> =
            unsafe { std::mem::transmute(raw) };

        Ok(Self { _db: db, raw, valid: false, err: None })
    }

    /// Update `self.valid` from the raw iterator status and capture any error.
    fn check_valid(&mut self) -> bool {
        self.valid = self.raw.valid();
        if !self.valid &&
            let Err(e) = self.raw.status()
        {
            self.err = Some(MptDbError::RocksDb(e.to_string()));
        }
        self.valid
    }
}

impl KvIterator for RocksDbIterator {
    fn first(&mut self) -> bool {
        self.raw.seek_to_first();
        self.check_valid()
    }

    fn last(&mut self) -> bool {
        self.raw.seek_to_last();
        self.check_valid()
    }

    fn valid(&self) -> bool {
        self.valid && self.raw.valid()
    }

    fn seek_ge(&mut self, key: &[u8]) -> bool {
        self.raw.seek(key);
        self.check_valid()
    }

    fn seek_lt(&mut self, key: &[u8]) -> bool {
        // seek_for_prev positions at the last key <= key.
        // We need strictly < key, so if we land on an exact match, step back.
        self.raw.seek_for_prev(key);
        if self.raw.valid() &&
            let Some(k) = self.raw.key() &&
            k == key
        {
            self.raw.prev();
        }
        self.check_valid()
    }

    fn next(&mut self) -> bool {
        self.raw.next();
        self.check_valid()
    }

    fn next_prefix(&mut self) -> bool {
        // At the non-MVCC level, next_prefix is equivalent to next.
        self.raw.next();
        self.check_valid()
    }

    fn prev(&mut self) -> bool {
        self.raw.prev();
        self.check_valid()
    }

    fn key(&self) -> &[u8] {
        self.raw.key().unwrap_or_default()
    }

    fn value(&self) -> &[u8] {
        self.raw.value().unwrap_or_default()
    }

    fn error(&self) -> Option<&MptDbError> {
        self.err.as_ref()
    }

    fn close(&mut self) -> Result<()> {
        self.valid = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mptdb_traits::{
        kv::KvEngine,
        types::{IterOptions, WriteOptions},
    };
    use tempfile::TempDir;

    fn tmp_engine() -> (crate::engine::RocksDbEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = crate::engine::RocksDbEngine::open_plain(dir.path()).unwrap();
        (engine, dir)
    }

    #[test]
    fn test_iterator_forward() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"a", b"1", &wo).unwrap();
        engine.set(b"b", b"2", &wo).unwrap();
        engine.set(b"c", b"3", &wo).unwrap();

        let mut iter = engine.new_iter(&IterOptions::default()).unwrap();
        assert!(iter.first());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"1");

        assert!(iter.next());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"2");

        assert!(iter.next());
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"3");

        assert!(!iter.next());
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_reverse() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"a", b"1", &wo).unwrap();
        engine.set(b"b", b"2", &wo).unwrap();
        engine.set(b"c", b"3", &wo).unwrap();

        let mut iter = engine.new_iter(&IterOptions::default()).unwrap();
        assert!(iter.last());
        assert_eq!(iter.key(), b"c");
        assert_eq!(iter.value(), b"3");

        assert!(iter.prev());
        assert_eq!(iter.key(), b"b");
        assert_eq!(iter.value(), b"2");

        assert!(iter.prev());
        assert_eq!(iter.key(), b"a");
        assert_eq!(iter.value(), b"1");

        assert!(!iter.prev());
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_seek_ge() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"a", b"1", &wo).unwrap();
        engine.set(b"b", b"2", &wo).unwrap();
        engine.set(b"d", b"4", &wo).unwrap();

        let mut iter = engine.new_iter(&IterOptions::default()).unwrap();

        // Exact match
        assert!(iter.seek_ge(b"b"));
        assert_eq!(iter.key(), b"b");

        // No exact match — lands on next key
        assert!(iter.seek_ge(b"c"));
        assert_eq!(iter.key(), b"d");

        // Past all keys
        assert!(!iter.seek_ge(b"e"));
    }

    #[test]
    fn test_iterator_seek_lt() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"a", b"1", &wo).unwrap();
        engine.set(b"b", b"2", &wo).unwrap();
        engine.set(b"d", b"4", &wo).unwrap();

        let mut iter = engine.new_iter(&IterOptions::default()).unwrap();

        // seek_lt("b") should land on "a" (strictly less than "b")
        assert!(iter.seek_lt(b"b"));
        assert_eq!(iter.key(), b"a");

        // seek_lt("c") should land on "b"
        assert!(iter.seek_lt(b"c"));
        assert_eq!(iter.key(), b"b");

        // seek_lt("a") — nothing before "a"
        assert!(!iter.seek_lt(b"a"));
    }

    #[test]
    fn test_iterator_bounds() {
        let (engine, _dir) = tmp_engine();
        let wo = WriteOptions::default();
        engine.set(b"a", b"1", &wo).unwrap();
        engine.set(b"b", b"2", &wo).unwrap();
        engine.set(b"c", b"3", &wo).unwrap();
        engine.set(b"d", b"4", &wo).unwrap();
        engine.set(b"e", b"5", &wo).unwrap();

        // lower_bound inclusive "b", upper_bound exclusive "d"
        let opts =
            IterOptions { lower_bound: Some(b"b".to_vec()), upper_bound: Some(b"d".to_vec()) };
        let mut iter = engine.new_iter(&opts).unwrap();

        assert!(iter.first());
        assert_eq!(iter.key(), b"b");

        assert!(iter.next());
        assert_eq!(iter.key(), b"c");

        // "d" is excluded by upper bound
        assert!(!iter.next());
        assert!(!iter.valid());
    }
}
