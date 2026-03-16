use mptdb_common::error::Result;

/// Write-ahead log trait: append-only log with offset-based access
/// and replay support.
pub trait Wal<T: Send>: Send + Sync {
    /// Append an entry to the log.
    fn write(&self, entry: T) -> Result<()>;

    /// Truncate all entries before the given offset.
    fn truncate_before(&self, offset: u64) -> Result<()>;

    /// Truncate all entries after the given offset.
    fn truncate_after(&self, offset: u64) -> Result<()>;

    /// Read the entry at the given offset.
    fn read_at(&self, offset: u64) -> Result<T>;

    /// Return the first (lowest) offset in the log.
    fn first_offset(&self) -> Result<u64>;

    /// Return the last (highest) offset in the log.
    fn last_offset(&self) -> Result<u64>;

    /// Replay entries in the range `[start, end]`, invoking `f` for each.
    fn replay(&self, start: u64, end: u64, f: &mut dyn FnMut(u64, T) -> Result<()>) -> Result<()>;

    /// Close the WAL and release resources.
    fn close(&mut self) -> Result<()>;
}

/// Processes WAL entries, typically running in a background thread.
pub trait WalProcessor<T: Send>: Send {
    /// Start the processor (e.g. spawn a background thread).
    fn start(&mut self);

    /// Process a single entry.
    fn process_entry(&mut self, entry: T) -> Result<()>;

    /// Stop the processor and release resources.
    fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trait_object_safety() {
        fn _assert_wal<T: Send + 'static>(_: Box<dyn Wal<T>>) {}
        fn _assert_wal_processor<T: Send + 'static>(_: Box<dyn WalProcessor<T>>) {}
    }
}
