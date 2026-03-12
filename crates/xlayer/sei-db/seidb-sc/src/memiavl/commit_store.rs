use crate::memiavl::{db::DB, import_export::Exporter as TreeExporter, tree::Tree};
use seidb_common::{
    config::MemIavlConfig,
    error::{Result, SeiDbError},
    path::get_commit_store_path,
};
use seidb_proto::{CommitInfo, NamedChangeSet, TreeNameUpgrade};
use seidb_traits::sc::{Exporter, ScSnapshotNode};
use std::path::Path;

/// High-level commit store wrapping [`DB`] with lifecycle management.
///
/// Mirrors the Go `CommitStore` in `memiavl/store.go`. The store is created
/// in an unopened state and must be opened via [`load_version`] before use.
pub struct MemiavlCommitStore {
    db: Option<DB>,
    pub(crate) home_dir: String,
    pub(crate) config: MemIavlConfig,
    /// Initial store names to apply as tree upgrades after DB open.
    /// Stored here because `initialize` may be called before the DB is opened.
    pending_initial_stores: Vec<String>,
}

impl MemiavlCommitStore {
    /// Create a new unopened commit store.
    pub fn new(home_dir: &str, config: MemIavlConfig) -> Self {
        Self {
            db: None,
            home_dir: home_dir.to_string(),
            config,
            pending_initial_stores: Vec::new(),
        }
    }

    /// Set the initial store names. These are passed to DB via the
    /// initial tree upgrade mechanism.
    ///
    /// If the DB is already open, upgrades are applied immediately.
    /// Otherwise, the names are saved and applied when `load_version` opens the DB.
    pub fn initialize(&mut self, initial_stores: &[String]) {
        self.pending_initial_stores = initial_stores.to_vec();

        if let Some(ref mut db) = self.db {
            Self::apply_initial_stores(db, initial_stores);
        }
    }

    /// Apply initial stores as tree upgrades on an open DB.
    /// Mirrors Go's `OpenDB` behavior: apply upgrades only on a fresh DB (version 0).
    /// `ApplyUpgrades` is idempotent (skips existing trees), so this is safe.
    fn apply_initial_stores(db: &mut DB, stores: &[String]) {
        if stores.is_empty() || db.version() != 0 {
            return;
        }
        let upgrades: Vec<TreeNameUpgrade> = stores
            .iter()
            .map(|name| TreeNameUpgrade {
                name: name.clone(),
                rename_from: String::new(),
                delete: false,
            })
            .collect();
        // Best-effort: ignore errors during initialize (mirrors Go behavior).
        let _ = db.apply_upgrades(&upgrades);
    }

    /// Open the DB at the given version.
    ///
    /// If `read_only` is true, the DB is opened without acquiring an exclusive
    /// lock and writes are forbidden.
    ///
    /// After opening, any pending initial stores from [`initialize`] are applied
    /// as tree upgrades (matching Go `OpenDB` behavior).
    pub fn load_version(&mut self, target_version: i64, read_only: bool) -> Result<()> {
        // Close existing DB if open
        if let Some(ref mut db) = self.db {
            let _ = db.close();
            self.db = None;
        }

        let commit_db_path = get_commit_store_path(Path::new(&self.home_dir));
        let mut db = DB::open(&commit_db_path, target_version, &self.config, read_only)?;

        // Apply pending initial stores (mirrors Go's OpenDB behavior).
        if !read_only && !self.pending_initial_stores.is_empty() {
            Self::apply_initial_stores(&mut db, &self.pending_initial_stores);
        }

        self.db = Some(db);
        Ok(())
    }

    /// Commit all pending changes and return the new version.
    pub fn commit(&mut self) -> Result<i64> {
        let db =
            self.db.as_mut().ok_or_else(|| SeiDbError::Other("commit store not opened".into()))?;
        db.commit()
    }

    /// Return the current committed version.
    pub fn version(&self) -> i64 {
        match &self.db {
            Some(db) => db.version(),
            None => 0,
        }
    }

    /// Return the latest version without loading the entire DB.
    pub fn get_latest_version(&self) -> Result<i64> {
        let commit_db_path = get_commit_store_path(Path::new(&self.home_dir));
        DB::get_latest_version(&commit_db_path)
    }

    /// Return the earliest available version.
    pub fn get_earliest_version(&self) -> Result<i64> {
        let commit_db_path = get_commit_store_path(Path::new(&self.home_dir));
        DB::get_earliest_version(&commit_db_path)
    }

    /// Apply named change sets to the corresponding trees.
    pub fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()> {
        if cs.is_empty() {
            return Ok(());
        }
        let db =
            self.db.as_mut().ok_or_else(|| SeiDbError::Other("commit store not opened".into()))?;
        db.apply_change_sets(cs)
    }

    /// Apply tree name upgrades (add, delete, rename).
    pub fn apply_upgrades(&mut self, upgrades: &[TreeNameUpgrade]) -> Result<()> {
        if upgrades.is_empty() {
            return Ok(());
        }
        let db =
            self.db.as_mut().ok_or_else(|| SeiDbError::Other("commit store not opened".into()))?;
        db.apply_upgrades(upgrades)
    }

    /// Return the commit info for the current working (uncommitted) state.
    pub fn working_commit_info(&self) -> CommitInfo {
        match &self.db {
            Some(db) => db.working_commit_info(),
            None => CommitInfo::default(),
        }
    }

    /// Return the commit info for the last committed version.
    pub fn last_commit_info(&self) -> CommitInfo {
        match &self.db {
            Some(db) => db.last_commit_info().clone(),
            None => CommitInfo::default(),
        }
    }

    /// Look up a tree by name.
    pub fn get_child_store_by_name(&self, name: &str) -> Option<&Tree> {
        self.db.as_ref()?.tree_by_name(name)
    }

    /// Set the initial version for all trees.
    pub fn set_initial_version(&mut self, v: i64) -> Result<()> {
        let db =
            self.db.as_mut().ok_or_else(|| SeiDbError::Other("commit store not opened".into()))?;
        if v < 0 || v > u32::MAX as i64 {
            return Err(SeiDbError::Other(format!("initial version out of u32 range: {v}")));
        }
        db.set_initial_version(v as u32);
        Ok(())
    }

    /// Roll back to the given version by closing and reopening the DB
    /// with `LoadForOverwriting` semantics.
    pub fn rollback(&mut self, target_version: i64) -> Result<()> {
        // Close existing DB
        if let Some(ref mut db) = self.db {
            let _ = db.close();
            self.db = None;
        }

        let commit_db_path = get_commit_store_path(Path::new(&self.home_dir));
        // Reopen at target_version — the DB will truncate WAL beyond this version
        let db = DB::open(&commit_db_path, target_version, &self.config, false)?;
        self.db = Some(db);
        Ok(())
    }

    /// Close the DB and release all resources.
    pub fn close(&mut self) -> Result<()> {
        if let Some(ref mut db) = self.db {
            db.close()?;
            self.db = None;
        }
        Ok(())
    }

    /// Create an exporter that streams all tree nodes in post-order.
    ///
    /// All named trees are exported sequentially. Each tree's nodes are
    /// emitted via the single-tree [`TreeExporter`] (post-order DFS).
    pub fn exporter(&self, _version: i64) -> Result<Box<dyn Exporter>> {
        let db =
            self.db.as_ref().ok_or_else(|| SeiDbError::Other("commit store not opened".into()))?;
        let tree_exporters: Vec<(String, TreeExporter)> = db
            .trees()
            .iter()
            .map(|nt| (nt.name.clone(), TreeExporter::new(nt.tree.root_ref())))
            .collect();
        Ok(Box::new(CommitterExporter { tree_exporters, current_tree: 0 }))
    }
}

// ---------------------------------------------------------------------------
// CommitterExporter — exports all named trees sequentially
// ---------------------------------------------------------------------------

/// Exports all named trees from a [`MemiavlCommitStore`] as a flat stream of
/// [`ScSnapshotNode`]s.
///
/// Trees are exported in order (sorted by name, matching the [`MultiTree`]
/// ordering). Each tree's nodes are yielded in post-order via the underlying
/// single-tree [`TreeExporter`].
pub struct CommitterExporter {
    tree_exporters: Vec<(String, TreeExporter)>,
    current_tree: usize,
}

impl Exporter for CommitterExporter {
    fn next(&mut self) -> Result<Option<ScSnapshotNode>> {
        loop {
            if self.current_tree >= self.tree_exporters.len() {
                return Ok(None);
            }
            let (ref _name, ref mut exporter) = self.tree_exporters[self.current_tree];
            match Iterator::next(exporter) {
                Some(node) => {
                    return Ok(Some(ScSnapshotNode {
                        key: node.key,
                        value: node.value,
                        version: node.version,
                        height: node.height,
                    }));
                }
                None => {
                    self.current_tree += 1;
                }
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        // Drop remaining exporters by clearing the vec.
        self.tree_exporters.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_store_new() {
        let store = MemiavlCommitStore::new("/tmp/test", MemIavlConfig::default());
        assert_eq!(store.version(), 0);
        assert!(store.db.is_none());
    }

    #[test]
    fn test_commit_store_not_opened_errors() {
        let mut store = MemiavlCommitStore::new("/tmp/test", MemIavlConfig::default());
        assert!(store.commit().is_err());
        assert!(store.apply_change_sets(&[]).is_ok()); // empty is ok
        assert!(store.set_initial_version(1).is_err());
    }

    #[test]
    fn test_exporter_not_opened() {
        let store = MemiavlCommitStore::new("/tmp/test_exp", MemIavlConfig::default());
        let result = store.exporter(1);
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().unwrap());
        assert!(err_msg.contains("not opened"), "got: {err_msg}");
    }

    #[test]
    fn test_exporter_empty_db() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let mut store = MemiavlCommitStore::new(home, MemIavlConfig::default());
        store.load_version(0, false).unwrap();

        let mut exporter = store.exporter(0).unwrap();
        // No trees means no nodes
        assert!(exporter.next().unwrap().is_none());
        exporter.close().unwrap();
    }

    #[test]
    fn test_exporter_single_tree_with_data() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let mut store = MemiavlCommitStore::new(home, MemIavlConfig::default());
        store.load_version(0, false).unwrap();

        // Add a tree and some data
        let upgrades = vec![seidb_proto::TreeNameUpgrade {
            name: "bank".to_string(),
            rename_from: String::new(),
            delete: false,
        }];
        store.apply_upgrades(&upgrades).unwrap();

        let cs = vec![seidb_proto::NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(seidb_proto::ChangeSet {
                pairs: vec![
                    seidb_proto::KvPair {
                        key: b"alice".to_vec(),
                        value: b"100".to_vec(),
                        delete: false,
                    },
                    seidb_proto::KvPair {
                        key: b"bob".to_vec(),
                        value: b"200".to_vec(),
                        delete: false,
                    },
                ],
            }),
        }];
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();

        // Export
        let mut exporter = store.exporter(1).unwrap();
        let mut nodes = Vec::new();
        while let Some(node) = exporter.next().unwrap() {
            nodes.push(node);
        }
        exporter.close().unwrap();

        // 2 leaves + 1 branch = 3 nodes
        assert_eq!(nodes.len(), 3, "expected 3 nodes for 2-leaf tree, got {}", nodes.len());

        // Post-order: leaf alice, leaf bob, branch
        assert_eq!(nodes[0].height, 0);
        assert_eq!(nodes[0].key, b"alice");
        assert_eq!(nodes[0].value, b"100");

        assert_eq!(nodes[1].height, 0);
        assert_eq!(nodes[1].key, b"bob");
        assert_eq!(nodes[1].value, b"200");

        assert_eq!(nodes[2].height, 1);
        assert!(nodes[2].value.is_empty()); // branch has no value
    }

    #[test]
    fn test_exporter_multi_tree() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap();
        let mut store = MemiavlCommitStore::new(home, MemIavlConfig::default());
        store.load_version(0, false).unwrap();

        // Create two trees
        let upgrades = vec![
            seidb_proto::TreeNameUpgrade {
                name: "acc".to_string(),
                rename_from: String::new(),
                delete: false,
            },
            seidb_proto::TreeNameUpgrade {
                name: "bank".to_string(),
                rename_from: String::new(),
                delete: false,
            },
        ];
        store.apply_upgrades(&upgrades).unwrap();

        // Add data to both trees
        let cs = vec![
            seidb_proto::NamedChangeSet {
                name: "acc".to_string(),
                changeset: Some(seidb_proto::ChangeSet {
                    pairs: vec![seidb_proto::KvPair {
                        key: b"acc_key".to_vec(),
                        value: b"acc_val".to_vec(),
                        delete: false,
                    }],
                }),
            },
            seidb_proto::NamedChangeSet {
                name: "bank".to_string(),
                changeset: Some(seidb_proto::ChangeSet {
                    pairs: vec![seidb_proto::KvPair {
                        key: b"bank_key".to_vec(),
                        value: b"bank_val".to_vec(),
                        delete: false,
                    }],
                }),
            },
        ];
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();

        // Export all trees
        let mut exporter = store.exporter(1).unwrap();
        let mut nodes = Vec::new();
        while let Some(node) = exporter.next().unwrap() {
            nodes.push(node);
        }
        exporter.close().unwrap();

        // Each single-leaf tree yields 1 node, so 2 total
        assert_eq!(nodes.len(), 2, "expected 2 nodes for 2 single-leaf trees, got {}", nodes.len());

        // Trees are sorted by name: "acc" then "bank"
        assert_eq!(nodes[0].key, b"acc_key");
        assert_eq!(nodes[0].value, b"acc_val");
        assert_eq!(nodes[0].height, 0);

        assert_eq!(nodes[1].key, b"bank_key");
        assert_eq!(nodes[1].value, b"bank_val");
        assert_eq!(nodes[1].height, 0);
    }
}
