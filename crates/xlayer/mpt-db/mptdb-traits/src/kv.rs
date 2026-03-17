use crate::types::{IterOptions, WriteOptions};
use mptdb_common::error::{MptDbError, Result};

/// Core key-value storage engine trait used by the storage backends.
pub trait KvEngine: Send + Sync {
    /// Get the value associated with the given key, or `None` if absent.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Set a key to the given value.
    fn set(&self, key: &[u8], value: &[u8], opts: &WriteOptions) -> Result<()>;

    /// Delete the given key.
    fn delete(&self, key: &[u8], opts: &WriteOptions) -> Result<()>;

    /// Create a new iterator over the key space bounded by `opts`.
    fn new_iter(&self, opts: &IterOptions) -> Result<Box<dyn KvIterator>>;

    /// Create a new write batch.
    fn new_batch(&self) -> Box<dyn Batch>;

    /// Flush any buffered writes to stable storage.
    fn flush(&self) -> Result<()>;

    /// Close the engine, releasing resources.
    fn close(&mut self) -> Result<()>;
}

/// An atomic write batch that can accumulate mutations before committing.
pub trait Batch: Send {
    /// Queue a set operation.
    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Queue a delete operation.
    fn delete(&mut self, key: &[u8]) -> Result<()>;

    /// Atomically commit all queued operations.
    fn commit(&mut self, opts: &WriteOptions) -> Result<()>;

    /// Return the number of operations queued.
    fn len(&self) -> usize;

    /// Return true if no operations are queued.
    fn is_empty(&self) -> bool;

    /// Discard all queued operations without committing.
    fn reset(&mut self);

    /// Close the batch, releasing resources.
    fn close(&mut self) -> Result<()>;
}

/// A storage engine that supports creating on-disk checkpoints.
pub trait Checkpointable {
    /// Create a checkpoint of the current state at the given directory.
    fn checkpoint(&self, dest_dir: &std::path::Path) -> Result<()>;
}

/// A low-level iterator over raw key-value pairs in the storage engine.
pub trait KvIterator: Send {
    /// Seek to the first key. Returns true if the iterator is valid.
    fn first(&mut self) -> bool;

    /// Seek to the last key. Returns true if the iterator is valid.
    fn last(&mut self) -> bool;

    /// Returns true if the iterator is positioned at a valid entry.
    fn valid(&self) -> bool;

    /// Seek to the first key >= `key`. Returns true if the iterator is valid.
    fn seek_ge(&mut self, key: &[u8]) -> bool;

    /// Seek to the last key < `key`. Returns true if the iterator is valid.
    fn seek_lt(&mut self, key: &[u8]) -> bool;

    /// Advance to the next key. Returns true if the iterator is valid.
    fn next(&mut self) -> bool;

    /// Advance to the first key with a different prefix than the current key.
    fn next_prefix(&mut self) -> bool;

    /// Move to the previous key. Returns true if the iterator is valid.
    fn prev(&mut self) -> bool;

    /// Return the current key. Only valid when `valid()` is true.
    fn key(&self) -> &[u8];

    /// Return the current value. Only valid when `valid()` is true.
    fn value(&self) -> &[u8];

    /// Return the current error, if any.
    fn error(&self) -> Option<&MptDbError>;

    /// Close the iterator, releasing resources.
    fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iterator::DbIterator;

    /// Compile-time assertion that all five traits are object-safe.
    #[test]
    fn test_trait_object_safety() {
        fn _assert_kv_engine(_: Box<dyn KvEngine>) {}
        fn _assert_batch(_: Box<dyn Batch>) {}
        fn _assert_checkpointable(_: Box<dyn Checkpointable>) {}
        fn _assert_kv_iterator(_: Box<dyn KvIterator>) {}
        fn _assert_db_iterator(_: Box<dyn DbIterator>) {}
    }
}
