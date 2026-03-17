use crate::{iterator::DbIterator, types::SnapshotNode};
use crossbeam_channel::Receiver;
use mptdb_common::error::Result;
use mptdb_proto::ChangeSet;

/// State-store trait: versioned key-value storage with changeset application,
/// pruning, and import/export support.
pub trait StateStore: Send + Sync {
    /// Get the value for a key at a given version.
    fn get(&self, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Check whether a key exists at a given version.
    fn has(&self, version: i64, key: &[u8]) -> Result<bool>;

    /// Create a forward iterator over the key range `[start, end)` at the given version.
    fn iterator(&self, version: i64, start: &[u8], end: &[u8]) -> Result<Box<dyn DbIterator>>;

    /// Create a reverse iterator over the key range `[start, end)` at the given version.
    fn reverse_iterator(
        &self,
        version: i64,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn DbIterator>>;

    /// Iterate over all raw key/value/version triples.
    /// The callback returns `true` to continue, `false` to stop.
    #[allow(clippy::type_complexity)]
    fn raw_iterate(&self, f: &mut dyn FnMut(&[u8], &[u8], i64) -> bool) -> Result<bool>;

    /// Return the latest committed version.
    fn get_latest_version(&self) -> i64;

    /// Set the latest committed version.
    fn set_latest_version(&self, version: i64) -> Result<()>;

    /// Return the earliest available version.
    fn get_earliest_version(&self) -> i64;

    /// Set the earliest available version. When `ignore_version` is true the
    /// update is unconditional; otherwise it is only applied when the new
    /// version is earlier than the current one.
    fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()>;

    /// Apply a changeset synchronously, blocking until persisted.
    fn apply_changeset_sync(&self, version: i64, changeset: &ChangeSet) -> Result<()>;

    /// Apply a changeset asynchronously (fire-and-forget).
    fn apply_changeset_async(&self, version: i64, changeset: &ChangeSet) -> Result<()>;

    /// Prune all versions up to (and including) the given version.
    fn prune(&self, version: i64) -> Result<()>;

    /// Import snapshot nodes from a channel into the store at the given version.
    fn import(&self, version: i64, nodes: Receiver<SnapshotNode>) -> Result<()>;

    /// Close the state store and release resources.
    fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trait_object_safety() {
        fn _assert_state_store(_: Box<dyn StateStore>) {}
    }
}
