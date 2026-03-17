use mptdb_common::error::{MptDbError, Result};

/// A higher-level iterator used by the database layer (e.g. state-store, state-commitment).
pub trait DbIterator: Send {
    /// Return the lower and upper bounds of the iterator's domain.
    fn domain(&self) -> (Option<&[u8]>, Option<&[u8]>);

    /// Returns true if the iterator is positioned at a valid entry.
    fn valid(&self) -> bool;

    /// Advance the iterator to the next entry.
    fn next(&mut self);

    /// Return the current key. Only valid when `valid()` is true.
    fn key(&self) -> &[u8];

    /// Return the current value. Only valid when `valid()` is true.
    fn value(&self) -> &[u8];

    /// Return the current error, if any.
    fn error(&self) -> Option<&MptDbError>;

    /// Close the iterator, releasing resources.
    fn close(&mut self) -> Result<()>;
}
