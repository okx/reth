use crate::iterator::DbIterator;
use seidb_common::error::Result;
use seidb_proto::{CommitInfo, NamedChangeSet, TreeNameUpgrade};

/// A snapshot node for state-commitment trees (IAVL).
#[derive(Debug, Clone)]
pub struct ScSnapshotNode {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub version: i64,
    pub height: i8,
}

/// Top-level state-commitment trait: manages versioned IAVL trees,
/// changeset application, and snapshotting. Mirrors the Go `Committer`.
pub trait Committer: Send + Sync {
    /// Initialize the committer with the given store names.
    fn initialize(&mut self, initial_stores: &[String]);

    /// Commit all pending changes and return the new version.
    fn commit(&mut self) -> Result<i64>;

    /// Return the current working version.
    fn version(&self) -> i64;

    /// Return the latest committed version.
    fn get_latest_version(&self) -> Result<i64>;

    /// Return the earliest available version.
    fn get_earliest_version(&self) -> Result<i64>;

    /// Apply the given changesets to the working state.
    fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()>;

    /// Apply tree upgrades (renames / deletions).
    fn apply_upgrades(&mut self, upgrades: &[TreeNameUpgrade]) -> Result<()>;

    /// Return the commit info for the current working (uncommitted) state.
    fn working_commit_info(&self) -> CommitInfo;

    /// Return the commit info for the last committed version.
    fn last_commit_info(&self) -> CommitInfo;

    /// Load a specific version. When `read_only` is true the returned
    /// committer must not be used for writes.
    fn load_version(&self, version: i64, read_only: bool) -> Result<Box<dyn Committer>>;

    /// Roll back to the given version, discarding all later data.
    fn rollback(&mut self, version: i64) -> Result<()>;

    /// Set the initial version for a freshly-created committer.
    fn set_initial_version(&mut self, version: i64) -> Result<()>;

    /// Return a child store by name, if it exists.
    fn get_child_store_by_name(&self, name: &str) -> Option<Box<dyn CommitKvStore>>;

    /// Create an importer that can bulk-load snapshot nodes at the given version.
    fn importer(&self, version: i64) -> Result<Box<dyn Importer>>;

    /// Create an exporter that streams snapshot nodes for the given version.
    fn exporter(&self, version: i64) -> Result<Box<dyn Exporter>>;

    /// Close the committer and release resources.
    fn close(&mut self) -> Result<()>;
}

/// A single IAVL tree providing key-value access, iteration, and proofs.
pub trait CommitKvStore: Send + Sync {
    /// Get the value for a key.
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;

    /// Check whether a key exists.
    fn has(&self, key: &[u8]) -> bool;

    /// Set a key-value pair.
    fn set(&mut self, key: &[u8], value: &[u8]);

    /// Remove a key.
    fn remove(&mut self, key: &[u8]);

    /// Return the current version of the tree.
    fn version(&self) -> i64;

    /// Return the root hash of the tree.
    fn root_hash(&self) -> Vec<u8>;

    /// Create an iterator over the key range `[start, end)`.
    /// When `ascending` is false the iteration order is reversed.
    fn iterator(&self, start: &[u8], end: &[u8], ascending: bool) -> Box<dyn DbIterator>;

    /// Return an existence/absence proof for the given key.
    fn get_proof(&self, key: &[u8]) -> Result<Vec<u8>>;

    /// Close the store and release resources.
    fn close(&mut self) -> Result<()>;
}

/// Bulk-import interface for loading snapshot nodes into a committer.
pub trait Importer: Send {
    /// Begin importing a new module (sub-tree).
    fn add_module(&mut self, name: &str) -> Result<()>;

    /// Add a single snapshot node.
    fn add_node(&mut self, node: &ScSnapshotNode);

    /// Finalize the import and release resources.
    fn close(&mut self) -> Result<()>;
}

/// Streaming export interface for reading snapshot nodes from a committer.
pub trait Exporter: Send {
    /// Return the next snapshot node, or `None` when exhausted.
    fn next(&mut self) -> Result<Option<ScSnapshotNode>>;

    /// Close the exporter and release resources.
    fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trait_object_safety() {
        fn _assert_committer(_: Box<dyn Committer>) {}
        fn _assert_commit_kv_store(_: Box<dyn CommitKvStore>) {}
        fn _assert_importer(_: Box<dyn Importer>) {}
        fn _assert_exporter(_: Box<dyn Exporter>) {}
    }
}
