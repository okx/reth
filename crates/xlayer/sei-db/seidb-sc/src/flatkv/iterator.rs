//! FlatKV iterator — experimental, storage keys only.
//!
//! Reads ONLY from committed state (storage_db), NOT pending writes.
//! By default returns keys in internal format (no prefix).
//! When `convert_to_memiavl` is enabled, `key()` prepends the appropriate
//! EVM prefix byte so that returned keys match the memiavl on-disk format.

use crate::flatkv::{
    keys::{meta_key_lower_bound, prefix_end},
    store::CommitStore,
};
use seidb_common::{
    error::{Result, SeiDbError},
    evm_keys::{build_memiavl_evm_key, EvmKeyKind},
};
use seidb_traits::{iterator::DbIterator, kv::KvEngine, types::IterOptions};

/// Iterator over FlatKV storage keys, wrapping a low-level `KvIterator`.
///
/// Caches the current key/value so callers can borrow them without
/// going through the raw iterator on every access.
///
/// When `convert_to_memiavl` is true, `key()` returns the cached key
/// with the memiavl EVM prefix byte prepended (e.g. `0x03` for storage).
/// This enables transparent export of FlatKV data in the same key format
/// that MemIAVL uses, so upstream consumers (Exporter, CompositeCommitStore)
/// see a uniform keyspace regardless of the backend.
pub(crate) struct FlatKvDbIterator {
    inner: Box<dyn seidb_traits::kv::KvIterator>,
    is_valid: bool,
    cached_key: Vec<u8>,
    cached_value: Vec<u8>,
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
    /// When true, `key()` returns the cached key converted to memiavl format.
    convert_to_memiavl: bool,
    /// The EVM key kind used for memiavl prefix conversion.
    key_kind: EvmKeyKind,
    /// Holds the memiavl-format key when conversion is enabled.
    cached_memiavl_key: Vec<u8>,
}

/// An always-invalid iterator returned when no data matches the query
/// or the underlying DB is not open.
pub(crate) struct EmptyIterator;

impl FlatKvDbIterator {
    /// Cache the current key/value from the inner iterator.
    /// When memiavl conversion is enabled, also builds the prefixed key.
    fn cache_current(&mut self) {
        if self.inner.valid() {
            self.cached_key = self.inner.key().to_vec();
            self.cached_value = self.inner.value().to_vec();
            if self.convert_to_memiavl {
                self.cached_memiavl_key = build_memiavl_evm_key(self.key_kind, &self.cached_key);
            }
            self.is_valid = true;
        } else {
            self.is_valid = false;
        }
    }
}

impl DbIterator for FlatKvDbIterator {
    fn domain(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        (self.start.as_deref(), self.end.as_deref())
    }

    fn valid(&self) -> bool {
        self.is_valid
    }

    fn next(&mut self) {
        if !self.is_valid {
            return;
        }
        self.inner.next();
        self.cache_current();
    }

    fn key(&self) -> &[u8] {
        if self.convert_to_memiavl {
            &self.cached_memiavl_key
        } else {
            &self.cached_key
        }
    }

    fn value(&self) -> &[u8] {
        &self.cached_value
    }

    fn error(&self) -> Option<&SeiDbError> {
        self.inner.error()
    }

    fn close(&mut self) -> Result<()> {
        self.is_valid = false;
        Ok(())
    }
}

impl DbIterator for EmptyIterator {
    fn domain(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        (None, None)
    }

    fn valid(&self) -> bool {
        false
    }

    fn next(&mut self) {}

    fn key(&self) -> &[u8] {
        &[]
    }

    fn value(&self) -> &[u8] {
        &[]
    }

    fn error(&self) -> Option<&SeiDbError> {
        None
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

// SAFETY: FlatKvDbIterator holds a Send KvIterator and owned Vec buffers.
unsafe impl Send for FlatKvDbIterator {}

impl CommitStore {
    /// Returns an iterator over storage keys in the range `[start, end)`.
    ///
    /// Reads ONLY from committed state (storage_db), NOT pending writes.
    /// If `start` is empty, iteration begins after the metadata key.
    /// If `end` is empty, iteration continues to the end of the keyspace.
    pub fn iterator(&self, start: &[u8], end: &[u8]) -> Box<dyn DbIterator> {
        let storage_db = match &self.storage_db {
            Some(db) => db,
            None => return Box::new(EmptyIterator),
        };

        let lower_bound = if start.is_empty() { meta_key_lower_bound() } else { start.to_vec() };

        let upper_bound = if end.is_empty() { None } else { Some(end.to_vec()) };

        let opts = IterOptions {
            lower_bound: Some(lower_bound.clone()),
            upper_bound: upper_bound.clone(),
        };

        let mut inner = match storage_db.new_iter(&opts) {
            Ok(iter) => iter,
            Err(_) => return Box::new(EmptyIterator),
        };

        // Seek to first entry.
        inner.first();

        let mut it = FlatKvDbIterator {
            inner,
            is_valid: false,
            cached_key: Vec::new(),
            cached_value: Vec::new(),
            start: Some(lower_bound),
            end: upper_bound,
            convert_to_memiavl: false,
            key_kind: EvmKeyKind::Storage,
            cached_memiavl_key: Vec::new(),
        };

        it.cache_current();
        Box::new(it)
    }

    /// Returns an iterator over storage keys matching the given prefix.
    ///
    /// Reads ONLY from committed state (storage_db), NOT pending writes.
    /// If `prefix` is empty, iterates over all storage keys (excluding metadata).
    pub fn iterator_by_prefix(&self, prefix: &[u8]) -> Box<dyn DbIterator> {
        let storage_db = match &self.storage_db {
            Some(db) => db,
            None => return Box::new(EmptyIterator),
        };

        let lower = if prefix.is_empty() { meta_key_lower_bound() } else { prefix.to_vec() };

        let upper = prefix_end(prefix);

        let opts = IterOptions { lower_bound: Some(lower.clone()), upper_bound: upper.clone() };

        let mut inner = match storage_db.new_iter(&opts) {
            Ok(iter) => iter,
            Err(_) => return Box::new(EmptyIterator),
        };

        inner.first();

        let mut it = FlatKvDbIterator {
            inner,
            is_valid: false,
            cached_key: Vec::new(),
            cached_value: Vec::new(),
            start: Some(lower),
            end: upper,
            convert_to_memiavl: false,
            key_kind: EvmKeyKind::Storage,
            cached_memiavl_key: Vec::new(),
        };

        it.cache_current();
        Box::new(it)
    }

    /// Returns an iterator over storage keys in the range `[start, end)`,
    /// with keys converted to memiavl format (prefixed with the EVM kind byte).
    ///
    /// This is the same as [`iterator`](Self::iterator) but each key returned
    /// by `key()` will have the appropriate memiavl prefix prepended
    /// (e.g. `0x03` for storage keys). Used by the `FlatKvStore` trait impl
    /// so that CompositeCommitStore and Exporter see a uniform keyspace.
    pub fn iterator_memiavl(
        &self,
        start: &[u8],
        end: &[u8],
        key_kind: EvmKeyKind,
    ) -> Box<dyn DbIterator> {
        let storage_db = match &self.storage_db {
            Some(db) => db,
            None => return Box::new(EmptyIterator),
        };

        let lower_bound = if start.is_empty() { meta_key_lower_bound() } else { start.to_vec() };

        let upper_bound = if end.is_empty() { None } else { Some(end.to_vec()) };

        let opts = IterOptions {
            lower_bound: Some(lower_bound.clone()),
            upper_bound: upper_bound.clone(),
        };

        let mut inner = match storage_db.new_iter(&opts) {
            Ok(iter) => iter,
            Err(_) => return Box::new(EmptyIterator),
        };

        // Seek to first entry.
        inner.first();

        let mut it = FlatKvDbIterator {
            inner,
            is_valid: false,
            cached_key: Vec::new(),
            cached_value: Vec::new(),
            start: Some(lower_bound),
            end: upper_bound,
            convert_to_memiavl: true,
            key_kind,
            cached_memiavl_key: Vec::new(),
        };

        it.cache_current();
        Box::new(it)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::config::FlatKvConfig;
    use seidb_traits::types::WriteOptions;
    use tempfile::TempDir;

    /// Helper: create a CommitStore, open it, and return it with its temp dir.
    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    #[test]
    fn test_iterator_empty_db() {
        let (store, _dir) = open_store();
        let iter = store.iterator(b"", b"");
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_single_key() {
        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();
        db.set(b"\x00\x01key1", b"val1", &wo).unwrap();

        let mut iter = store.iterator(b"", b"");
        assert!(iter.valid());
        assert_eq!(iter.key(), b"\x00\x01key1");
        assert_eq!(iter.value(), b"val1");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_multiple_keys() {
        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();
        db.set(b"\x00\x01aaa", b"v1", &wo).unwrap();
        db.set(b"\x00\x01bbb", b"v2", &wo).unwrap();
        db.set(b"\x00\x01ccc", b"v3", &wo).unwrap();

        let mut iter = store.iterator(b"", b"");

        assert!(iter.valid());
        assert_eq!(iter.key(), b"\x00\x01aaa");
        assert_eq!(iter.value(), b"v1");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"\x00\x01bbb");
        assert_eq!(iter.value(), b"v2");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"\x00\x01ccc");
        assert_eq!(iter.value(), b"v3");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_by_prefix() {
        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();
        db.set(b"\x01aaa", b"v1", &wo).unwrap();
        db.set(b"\x01bbb", b"v2", &wo).unwrap();
        db.set(b"\x02aaa", b"v3", &wo).unwrap();

        let mut iter = store.iterator_by_prefix(b"\x01");

        assert!(iter.valid());
        assert_eq!(iter.key(), b"\x01aaa");
        assert_eq!(iter.value(), b"v1");

        iter.next();
        assert!(iter.valid());
        assert_eq!(iter.key(), b"\x01bbb");
        assert_eq!(iter.value(), b"v2");

        iter.next();
        assert!(!iter.valid(), "should not see keys with prefix \\x02");
    }

    #[test]
    fn test_empty_iterator() {
        let mut iter = EmptyIterator;
        assert!(!iter.valid());
        assert_eq!(iter.key(), b"" as &[u8]);
        assert_eq!(iter.value(), b"" as &[u8]);
        assert!(iter.error().is_none());
        assert_eq!(iter.domain(), (None, None));
        iter.next();
        assert!(!iter.valid());
        assert!(iter.close().is_ok());
    }

    #[test]
    fn test_iterator_memiavl_format() {
        use seidb_common::evm_keys::STATE_KEY_PREFIX;

        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();

        // Write internal-format keys (no prefix) to storage_db.
        let addr_slot_a = [0x01u8; 52]; // 20-byte addr + 32-byte slot
        let addr_slot_b = [0x02u8; 52];
        db.set(&addr_slot_a, b"val_a", &wo).unwrap();
        db.set(&addr_slot_b, b"val_b", &wo).unwrap();

        // iterator_memiavl should prepend 0x03 (STATE_KEY_PREFIX) to each key.
        let mut iter = store.iterator_memiavl(b"", b"", EvmKeyKind::Storage);

        assert!(iter.valid());
        let key_a = iter.key();
        assert_eq!(key_a.len(), 1 + 52, "memiavl key should be prefix + internal key");
        assert_eq!(key_a[0], STATE_KEY_PREFIX);
        assert_eq!(&key_a[1..], &addr_slot_a);
        assert_eq!(iter.value(), b"val_a");

        iter.next();
        assert!(iter.valid());
        let key_b = iter.key();
        assert_eq!(key_b[0], STATE_KEY_PREFIX);
        assert_eq!(&key_b[1..], &addr_slot_b);
        assert_eq!(iter.value(), b"val_b");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_memiavl_empty_db() {
        let (store, _dir) = open_store();
        let iter = store.iterator_memiavl(b"", b"", EvmKeyKind::Storage);
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_memiavl_nonce_kind() {
        use seidb_common::evm_keys::NONCE_KEY_PREFIX;

        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();

        // Even though this is storage_db, we can test Nonce kind prefix conversion.
        let addr = [0xABu8; 20];
        db.set(&addr, b"nonce_val", &wo).unwrap();

        let mut iter = store.iterator_memiavl(b"", b"", EvmKeyKind::Nonce);
        assert!(iter.valid());
        let key = iter.key();
        assert_eq!(key[0], NONCE_KEY_PREFIX);
        assert_eq!(&key[1..], &addr);
        assert_eq!(iter.value(), b"nonce_val");

        iter.next();
        assert!(!iter.valid());
    }

    #[test]
    fn test_iterator_no_conversion_by_default() {
        let (store, _dir) = open_store();
        let db = store.storage_db.as_ref().unwrap();
        let wo = WriteOptions::default();

        let raw_key = [0x42u8; 10];
        db.set(&raw_key, b"raw_val", &wo).unwrap();

        // Default iterator (no memiavl conversion) returns raw keys.
        let iter = store.iterator(b"", b"");
        assert!(iter.valid());
        assert_eq!(iter.key(), &raw_key);
    }
}
