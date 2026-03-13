use crate::memiavl::{rate_limiter::RateLimiter, snapshot::Snapshot, tree::Tree};
use prost::Message;
use rayon::prelude::*;
use seidb_common::{
    error::{join_errors, Result, SeiDbError},
    version::next_version,
};
use seidb_proto::{
    CommitId, CommitInfo, MultiTreeMetadata, NamedChangeSet, StoreInfo, TreeNameUpgrade,
};
use std::{collections::HashMap, fs, io::Write, path::Path, sync::Mutex};

pub const METADATA_FILE_NAME: &str = "__metadata";

pub struct NamedTree {
    pub name: String,
    pub tree: Tree,
}

/// Default number of parallel threads for snapshot writing (Phase 2).
/// Matches Go's SnapshotWriterLimit default of 4.
const DEFAULT_SNAPSHOT_WRITER_LIMIT: usize = 4;

/// Manages multiple named IAVL trees with coordinated version and commit.
///
/// All trees share the same latest version, and snapshots are always created
/// at the same version across all trees.
pub struct MultiTree {
    initial_version: u32,
    zero_copy: bool,
    trees: Vec<NamedTree>,
    trees_by_name: HashMap<String, usize>,
    last_commit_info: CommitInfo,
    /// Maximum number of parallel writer threads for snapshot Phase 2.
    /// Defaults to [`DEFAULT_SNAPSHOT_WRITER_LIMIT`] (4).
    snapshot_writer_limit: usize,
    /// Optional global rate limiter shared across all snapshot file writers
    /// to prevent page cache eviction.
    snapshot_rate_limiter: Option<RateLimiter>,
    /// When true, apply_change_sets dispatches each tree's changeset to its
    /// own background thread (via `Tree::apply_change_set_async`), matching
    /// Go's `asyncCommit` behaviour. Callers must call `save_version` to
    /// join all workers before reading tree state.
    async_apply: bool,
}

impl MultiTree {
    /// Create a new empty `MultiTree` with the given initial version.
    pub fn new_empty(initial_version: u32) -> Self {
        Self {
            initial_version,
            zero_copy: true,
            trees: Vec::new(),
            trees_by_name: HashMap::new(),
            last_commit_info: CommitInfo { version: 0, store_infos: Vec::new() },
            snapshot_writer_limit: DEFAULT_SNAPSHOT_WRITER_LIMIT,
            snapshot_rate_limiter: None,
            async_apply: false,
        }
    }

    /// Load a `MultiTree` from a snapshot directory.
    ///
    /// Each subdirectory (except `__metadata`) is treated as a named tree's
    /// snapshot. The metadata file contains protobuf-encoded `MultiTreeMetadata`
    /// with `CommitInfo` and `initial_version`.
    pub fn load(dir: &Path) -> Result<Self> {
        let metadata = read_metadata(dir)?;

        let entries = fs::read_dir(dir)?;
        let mut tree_map: HashMap<String, Tree> = HashMap::new();
        let mut tree_names: Vec<String> = Vec::new();

        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|os| SeiDbError::Other(format!("non-utf8 directory name: {os:?}")))?;
            let snapshot = Snapshot::open(&dir.join(&name))?;
            let tree = Tree::new_from_snapshot(snapshot);
            tree_names.push(name.clone());
            tree_map.insert(name, tree);
        }

        tree_names.sort();

        let mut trees = Vec::with_capacity(tree_names.len());
        let mut trees_by_name = HashMap::with_capacity(tree_names.len());
        for (i, name) in tree_names.iter().enumerate() {
            let tree = tree_map.remove(name).unwrap();
            trees.push(NamedTree { name: name.clone(), tree });
            trees_by_name.insert(name.clone(), i);
        }

        let commit_info = metadata.commit_info.unwrap_or_default();
        let initial_version = metadata.initial_version as u32;

        let mut mt = Self {
            initial_version,
            zero_copy: false,
            trees,
            trees_by_name,
            last_commit_info: commit_info,
            snapshot_writer_limit: DEFAULT_SNAPSHOT_WRITER_LIMIT,
            snapshot_rate_limiter: None,
            async_apply: false,
        };
        mt.set_initial_version_internal(initial_version);
        Ok(mt)
    }

    /// Look up a tree by name, returning an immutable reference.
    pub fn tree_by_name(&self, name: &str) -> Option<&Tree> {
        self.trees_by_name.get(name).map(|&i| &self.trees[i].tree)
    }

    /// Look up a tree by name, returning a mutable reference.
    pub fn tree_by_name_mut(&mut self, name: &str) -> Option<&mut Tree> {
        self.trees_by_name.get(name).copied().map(move |i| &mut self.trees[i].tree)
    }

    /// Return all named trees, ordered by name.
    pub fn trees(&self) -> &[NamedTree] {
        &self.trees
    }

    /// The current committed version, taken from `last_commit_info`.
    pub fn version(&self) -> i64 {
        self.last_commit_info.version
    }

    /// Apply change sets to the corresponding named trees.
    ///
    /// Each `NamedChangeSet` targets a tree by name. If the tree is not found,
    /// the change set is silently skipped (non-existent module).
    pub fn apply_change_sets(&mut self, change_sets: &[NamedChangeSet]) -> Result<()> {
        if self.async_apply {
            // Parallel mode: group changesets by tree index, then use
            // std::thread::scope to apply each tree's work in parallel.
            // This matches Go's per-tree goroutine dispatch.
            let mut per_tree: Vec<Vec<&seidb_proto::KvPair>> = vec![Vec::new(); self.trees.len()];
            for cs in change_sets {
                let Some(&idx) = self.trees_by_name.get(&cs.name) else {
                    continue;
                };
                if let Some(changeset) = &cs.changeset {
                    per_tree[idx].extend(changeset.pairs.iter());
                }
            }

            // Only spawn threads if there are multiple non-empty trees.
            let non_empty: usize = per_tree.iter().filter(|v| !v.is_empty()).count();
            if non_empty <= 1 {
                // Single tree — no benefit from parallelism.
                for (idx, pairs) in per_tree.iter().enumerate() {
                    if !pairs.is_empty() {
                        self.trees[idx].tree.apply_kvpair_refs(pairs);
                    }
                }
            } else {
                // Use rayon for parallel tree apply. Each tree gets its own
                // set of KvPair references to apply independently.
                let trees = &mut self.trees;
                per_tree.par_iter().zip(trees.par_iter_mut()).for_each(|(pairs, entry)| {
                    if !pairs.is_empty() {
                        entry.tree.apply_kvpair_refs(pairs);
                    }
                });
            }
        } else {
            // Sync mode: apply directly on the calling thread.
            for cs in change_sets {
                let Some(&idx) = self.trees_by_name.get(&cs.name) else {
                    continue;
                };
                if let Some(changeset) = &cs.changeset {
                    self.trees[idx].tree.apply_kvpairs(&changeset.pairs);
                }
            }
        }
        Ok(())
    }

    /// Apply store name upgrades: add, delete, or rename trees.
    ///
    /// After all upgrades, the tree list is re-sorted by name and the index
    /// map is rebuilt.
    pub fn apply_upgrades(&mut self, upgrades: &[TreeNameUpgrade]) -> Result<()> {
        if upgrades.is_empty() {
            return Ok(());
        }

        // Invalidate the name map; we rebuild it at the end.
        self.trees_by_name.clear();

        for upgrade in upgrades {
            if upgrade.delete {
                let pos = self.trees.iter().position(|t| t.name == upgrade.name);
                match pos {
                    Some(i) => {
                        self.trees.remove(i);
                    }
                    None => {
                        return Err(SeiDbError::Other(format!(
                            "unknown tree name {}",
                            upgrade.name
                        )));
                    }
                }
            } else if !upgrade.rename_from.is_empty() {
                let pos = self.trees.iter().position(|t| t.name == upgrade.rename_from);
                match pos {
                    Some(i) => {
                        self.trees[i].name = upgrade.name.clone();
                    }
                    None => {
                        return Err(SeiDbError::Other(format!(
                            "unknown tree name {}",
                            upgrade.rename_from
                        )));
                    }
                }
            } else {
                // Add a new empty tree
                let v = next_version(self.version(), self.initial_version);
                if v < 0 || v > u32::MAX as i64 {
                    return Err(SeiDbError::Other(format!("version overflows uint32: {v}")));
                }
                let tree = Tree::new_empty(0, v as u32);
                self.trees.push(NamedTree { name: upgrade.name.clone(), tree });
            }
        }

        self.trees.sort_by(|a, b| a.name.cmp(&b.name));

        // Rebuild name map, checking for conflicts.
        self.trees_by_name = HashMap::with_capacity(self.trees.len());
        for (i, t) in self.trees.iter().enumerate() {
            if self.trees_by_name.contains_key(&t.name) {
                return Err(SeiDbError::Other(format!("memiavl tree name conflicts: {}", t.name)));
            }
            self.trees_by_name.insert(t.name.clone(), i);
        }

        Ok(())
    }

    /// Bump the version of all trees and optionally update the commit info.
    ///
    /// Returns `(new_version, commit_info)`.
    pub fn save_version(&mut self, update_commit_info: bool) -> Result<(i64, CommitInfo)> {
        self.last_commit_info.version =
            next_version(self.last_commit_info.version, self.initial_version);

        for entry in &mut self.trees {
            entry.tree.save_version(update_commit_info)?;
        }

        if update_commit_info {
            self.update_commit_info();
        } else {
            self.last_commit_info.store_infos.clear();
        }

        Ok((self.last_commit_info.version, self.last_commit_info.clone()))
    }

    /// Return an O(1) copy of the `MultiTree` that shares all tree nodes via `Arc`.
    pub fn copy(&mut self) -> Self {
        let mut trees = Vec::with_capacity(self.trees.len());
        let mut trees_by_name = HashMap::with_capacity(self.trees.len());
        for (i, entry) in self.trees.iter_mut().enumerate() {
            let tree_copy = entry.tree.copy();
            trees.push(NamedTree { name: entry.name.clone(), tree: tree_copy });
            trees_by_name.insert(entry.name.clone(), i);
        }

        Self {
            initial_version: self.initial_version,
            zero_copy: self.zero_copy,
            trees,
            trees_by_name,
            last_commit_info: self.last_commit_info.clone(),
            snapshot_writer_limit: self.snapshot_writer_limit,
            snapshot_rate_limiter: self.snapshot_rate_limiter.clone(),
            async_apply: self.async_apply,
        }
    }

    /// Return a reference to the last saved commit info.
    pub fn last_commit_info(&self) -> &CommitInfo {
        &self.last_commit_info
    }

    /// Build commit info from the current (possibly uncommitted) tree state.
    pub fn working_commit_info(&self) -> CommitInfo {
        let version = next_version(self.last_commit_info.version, self.initial_version);
        self.build_commit_info(version)
    }

    /// Close all trees, releasing resources.
    pub fn close(&mut self) -> Result<()> {
        let mut errs = Vec::new();
        for entry in &mut self.trees {
            if let Err(e) = entry.tree.close() {
                errs.push(e);
            }
        }
        self.trees.clear();
        self.trees_by_name.clear();
        self.last_commit_info = CommitInfo { version: 0, store_infos: Vec::new() };
        if let Some(err) = join_errors(errs) {
            Err(err)
        } else {
            Ok(())
        }
    }

    /// Set the initial version. Propagates to all trees.
    pub fn set_initial_version(&mut self, v: u32) {
        self.set_initial_version_internal(v);
    }

    /// Toggle zero-copy mode on all trees.
    pub fn set_zero_copy(&mut self, zc: bool) {
        self.zero_copy = zc;
        for entry in &mut self.trees {
            entry.tree.set_zero_copy(zc);
        }
    }

    /// Set the maximum number of parallel writer threads for snapshot Phase 2.
    ///
    /// Must be at least 1. Values of 0 are clamped to 1.
    pub fn set_snapshot_writer_limit(&mut self, limit: usize) {
        self.snapshot_writer_limit = limit.max(1);
    }

    /// Enable or disable per-tree async apply, matching Go's `asyncCommit`.
    ///
    /// When enabled, [`apply_change_sets`] dispatches each tree's changeset to
    /// its own background thread. [`save_version`] joins all workers before
    /// computing hashes. This parallelises the AVL insertion work across trees.
    pub fn set_async_apply(&mut self, enabled: bool) {
        self.async_apply = enabled;
    }

    /// Set the global snapshot write rate limiter.
    ///
    /// Created from [`MemIavlConfig::snapshot_write_rate_mbps`] via
    /// [`RateLimiter::new`]. When set, all snapshot file writers share this
    /// limiter so aggregate disk throughput is bounded.
    pub fn set_snapshot_rate_limiter(&mut self, limiter: Option<RateLimiter>) {
        self.snapshot_rate_limiter = limiter;
    }

    /// Write a snapshot of all trees to `dir` using a two-phase approach.
    ///
    /// **Phase 1**: Write the "evm" tree first (serial). EVM is typically the
    /// largest tree and benefits from sequential I/O without contention.
    ///
    /// **Phase 2**: Write all remaining trees in parallel using a dedicated
    /// rayon thread pool bounded by [`snapshot_writer_limit`](Self::set_snapshot_writer_limit)
    /// (default 4).
    ///
    /// Each tree is written to a subdirectory named after the tree. A metadata
    /// file containing protobuf-encoded `MultiTreeMetadata` is also written.
    pub fn write_snapshot(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;

        let limiter = self.snapshot_rate_limiter.as_ref();

        // Phase 1: Write EVM tree first (serial, for I/O locality)
        for entry in &self.trees {
            if entry.name == "evm" {
                entry.tree.write_snapshot_with_limiter(&dir.join(&entry.name), limiter)?;
                break;
            }
        }

        // Phase 2: Write remaining trees in parallel using a bounded thread pool
        let non_evm: Vec<&NamedTree> = self.trees.iter().filter(|nt| nt.name != "evm").collect();

        if !non_evm.is_empty() {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(self.snapshot_writer_limit)
                .build()
                .map_err(|e| SeiDbError::Other(format!("failed to build rayon pool: {e}")))?;

            let errors: Mutex<Vec<SeiDbError>> = Mutex::new(Vec::new());
            pool.install(|| {
                non_evm.par_iter().for_each(|nt| {
                    let tree_dir = dir.join(&nt.name);
                    if let Err(e) = nt.tree.write_snapshot_with_limiter(&tree_dir, limiter) {
                        errors.lock().unwrap().push(e);
                    }
                });
            });

            let errs = errors.into_inner().unwrap();
            if let Some(err) = join_errors(errs) {
                return Err(err);
            }
        }

        // Write metadata
        let metadata = MultiTreeMetadata {
            commit_info: Some(self.last_commit_info.clone()),
            initial_version: self.initial_version as i64,
        };
        let buf = metadata.encode_to_vec();
        write_file_sync(&dir.join(METADATA_FILE_NAME), &buf)?;

        Ok(())
    }

    // --- internal helpers ---

    fn set_initial_version_internal(&mut self, v: u32) {
        self.initial_version = v;
        for entry in &mut self.trees {
            entry.tree.set_initial_version(v);
        }
    }

    fn update_commit_info(&mut self) {
        self.last_commit_info = self.build_commit_info(self.last_commit_info.version);
    }

    fn build_commit_info(&self, version: i64) -> CommitInfo {
        let store_infos = self
            .trees
            .iter()
            .map(|entry| StoreInfo {
                name: entry.name.clone(),
                commit_id: Some(CommitId {
                    version: entry.tree.version(),
                    hash: entry.tree.root_hash(),
                }),
            })
            .collect();
        CommitInfo { version, store_infos }
    }
}

/// Convert a `NamedChangeSet` into the `(key, Option<value>)` format expected
/// by `Tree::apply_change_set`.
fn changeset_to_pairs(cs: &NamedChangeSet) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let Some(changeset) = &cs.changeset else {
        return Vec::new();
    };
    changeset
        .pairs
        .iter()
        .map(|pair| {
            if pair.delete {
                (pair.key.clone(), None)
            } else {
                (pair.key.clone(), Some(pair.value.clone()))
            }
        })
        .collect()
}

/// Read and decode the `MultiTreeMetadata` protobuf from the metadata file.
pub fn read_metadata(dir: &Path) -> Result<MultiTreeMetadata> {
    let bz = fs::read(dir.join(METADATA_FILE_NAME))?;
    let metadata = MultiTreeMetadata::decode(bz.as_slice())?;
    Ok(metadata)
}

/// Write a `MultiTreeMetadata` protobuf to the metadata file in `dir`.
///
/// This is used by [`MultiTreeImporter`] to finalize a snapshot import.
/// It reads all subdirectories, opens each as a snapshot to get the root hash,
/// and writes the aggregated metadata (matching Go's `updateMetadataFile`).
pub fn write_metadata(dir: &Path, version: i64) -> Result<()> {
    let entries = fs::read_dir(dir)?;
    let mut store_infos: Vec<StoreInfo> = Vec::new();

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|os| SeiDbError::Other(format!("non-utf8 directory name: {os:?}")))?;
        let snapshot = crate::memiavl::snapshot::Snapshot::open(&dir.join(&name))?;
        store_infos.push(StoreInfo {
            name,
            commit_id: Some(CommitId { version, hash: snapshot.root_hash() }),
        });
    }

    // Sort store infos by name for deterministic output
    store_infos.sort_by(|a, b| a.name.cmp(&b.name));

    let metadata = MultiTreeMetadata {
        commit_info: Some(CommitInfo { version, store_infos }),
        // initial_version should correspond to the first rlog entry (version + 1)
        initial_version: version + 1,
    };
    let buf = metadata.encode_to_vec();
    write_file_sync(&dir.join(METADATA_FILE_NAME), &buf)
}

/// Write data to a file and fsync before closing.
pub fn write_file_sync(path: &Path, data: &[u8]) -> Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_proto::{ChangeSet, KvPair};

    fn make_upgrade_add(name: &str) -> TreeNameUpgrade {
        TreeNameUpgrade { name: name.to_string(), rename_from: String::new(), delete: false }
    }

    fn make_upgrade_delete(name: &str) -> TreeNameUpgrade {
        TreeNameUpgrade { name: name.to_string(), rename_from: String::new(), delete: true }
    }

    fn make_upgrade_rename(new_name: &str, old_name: &str) -> TreeNameUpgrade {
        TreeNameUpgrade {
            name: new_name.to_string(),
            rename_from: old_name.to_string(),
            delete: false,
        }
    }

    fn make_named_changeset(name: &str, pairs: Vec<KvPair>) -> NamedChangeSet {
        NamedChangeSet { name: name.to_string(), changeset: Some(ChangeSet { pairs }) }
    }

    fn make_kv_pair(key: &[u8], value: &[u8]) -> KvPair {
        KvPair { delete: false, key: key.to_vec(), value: value.to_vec() }
    }

    fn make_delete_pair(key: &[u8]) -> KvPair {
        KvPair { delete: true, key: key.to_vec(), value: Vec::new() }
    }

    #[test]
    fn test_multitree_empty() {
        let mt = MultiTree::new_empty(0);
        assert_eq!(mt.version(), 0);
        assert!(mt.trees().is_empty());
        assert!(mt.tree_by_name("anything").is_none());
        assert_eq!(mt.last_commit_info().version, 0);
    }

    #[test]
    fn test_multitree_add_trees() {
        let mut mt = MultiTree::new_empty(0);
        let upgrades =
            vec![make_upgrade_add("bank"), make_upgrade_add("acc"), make_upgrade_add("staking")];
        mt.apply_upgrades(&upgrades).unwrap();

        assert_eq!(mt.trees().len(), 3);
        // Trees should be sorted by name
        assert_eq!(mt.trees()[0].name, "acc");
        assert_eq!(mt.trees()[1].name, "bank");
        assert_eq!(mt.trees()[2].name, "staking");

        assert!(mt.tree_by_name("bank").is_some());
        assert!(mt.tree_by_name("acc").is_some());
        assert!(mt.tree_by_name("staking").is_some());
        assert!(mt.tree_by_name("missing").is_none());
    }

    #[test]
    fn test_multitree_apply_changesets() {
        let mut mt = MultiTree::new_empty(0);
        mt.apply_upgrades(&[make_upgrade_add("bank"), make_upgrade_add("acc")]).unwrap();

        let changesets = vec![
            make_named_changeset("bank", vec![make_kv_pair(b"balance", b"100")]),
            make_named_changeset("acc", vec![make_kv_pair(b"addr1", b"info")]),
        ];
        mt.apply_change_sets(&changesets).unwrap();

        assert_eq!(mt.tree_by_name("bank").unwrap().get(b"balance"), Some(b"100".to_vec()));
        assert_eq!(mt.tree_by_name("acc").unwrap().get(b"addr1"), Some(b"info".to_vec()));

        // Applying to nonexistent tree is silently skipped
        let cs2 = vec![make_named_changeset("nonexistent", vec![make_kv_pair(b"k", b"v")])];
        mt.apply_change_sets(&cs2).unwrap();

        // Test delete via changeset
        let cs3 = vec![make_named_changeset("bank", vec![make_delete_pair(b"balance")])];
        mt.apply_change_sets(&cs3).unwrap();
        assert!(mt.tree_by_name("bank").unwrap().get(b"balance").is_none());
    }

    #[test]
    fn test_multitree_save_version() {
        let mut mt = MultiTree::new_empty(0);
        mt.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();

        let changesets = vec![make_named_changeset("bank", vec![make_kv_pair(b"key1", b"val1")])];
        mt.apply_change_sets(&changesets).unwrap();

        let (ver, info) = mt.save_version(true).unwrap();
        assert_eq!(ver, 1);
        assert_eq!(info.version, 1);
        assert_eq!(info.store_infos.len(), 1);
        assert_eq!(info.store_infos[0].name, "bank");
        let commit_id = info.store_infos[0].commit_id.as_ref().unwrap();
        assert_eq!(commit_id.version, 1);
        assert_eq!(commit_id.hash.len(), 32);

        // Second save
        let (ver2, _) = mt.save_version(true).unwrap();
        assert_eq!(ver2, 2);
        assert_eq!(mt.version(), 2);
    }

    #[test]
    fn test_multitree_copy() {
        let mut mt = MultiTree::new_empty(0);
        mt.apply_upgrades(&[make_upgrade_add("bank")]).unwrap();
        mt.apply_change_sets(&[make_named_changeset("bank", vec![make_kv_pair(b"k", b"v1")])])
            .unwrap();
        mt.save_version(true).unwrap();

        let copy = mt.copy();

        // Modify original
        mt.apply_change_sets(&[make_named_changeset("bank", vec![make_kv_pair(b"k", b"v2")])])
            .unwrap();

        // Copy should be unaffected
        assert_eq!(copy.tree_by_name("bank").unwrap().get(b"k"), Some(b"v1".to_vec()));
        // Original should reflect mutation
        assert_eq!(mt.tree_by_name("bank").unwrap().get(b"k"), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_multitree_delete_tree() {
        let mut mt = MultiTree::new_empty(0);
        mt.apply_upgrades(&[
            make_upgrade_add("alpha"),
            make_upgrade_add("beta"),
            make_upgrade_add("gamma"),
        ])
        .unwrap();
        assert_eq!(mt.trees().len(), 3);

        mt.apply_upgrades(&[make_upgrade_delete("beta")]).unwrap();
        assert_eq!(mt.trees().len(), 2);
        assert!(mt.tree_by_name("beta").is_none());
        assert!(mt.tree_by_name("alpha").is_some());
        assert!(mt.tree_by_name("gamma").is_some());

        // Trees remain sorted
        assert_eq!(mt.trees()[0].name, "alpha");
        assert_eq!(mt.trees()[1].name, "gamma");
    }

    #[test]
    fn test_multitree_rename_tree() {
        let mut mt = MultiTree::new_empty(0);
        mt.apply_upgrades(&[make_upgrade_add("old_name")]).unwrap();
        mt.apply_change_sets(&[make_named_changeset("old_name", vec![make_kv_pair(b"k", b"v")])])
            .unwrap();

        mt.apply_upgrades(&[make_upgrade_rename("new_name", "old_name")]).unwrap();

        assert!(mt.tree_by_name("old_name").is_none());
        assert_eq!(mt.tree_by_name("new_name").unwrap().get(b"k"), Some(b"v".to_vec()));
    }

    #[test]
    fn test_multitree_snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snapshot");

        let mut mt = MultiTree::new_empty(0);
        mt.apply_upgrades(&[make_upgrade_add("bank"), make_upgrade_add("acc")]).unwrap();

        mt.apply_change_sets(&[
            make_named_changeset("bank", vec![make_kv_pair(b"bal", b"100")]),
            make_named_changeset("acc", vec![make_kv_pair(b"addr", b"data")]),
        ])
        .unwrap();
        mt.save_version(true).unwrap();

        mt.write_snapshot(&snap_dir).unwrap();

        // Reload
        let loaded = MultiTree::load(&snap_dir).unwrap();
        assert_eq!(loaded.version(), 1);
        assert_eq!(loaded.trees().len(), 2);
        assert_eq!(loaded.trees()[0].name, "acc");
        assert_eq!(loaded.trees()[1].name, "bank");

        assert_eq!(loaded.tree_by_name("bank").unwrap().get(b"bal"), Some(b"100".to_vec()));
        assert_eq!(loaded.tree_by_name("acc").unwrap().get(b"addr"), Some(b"data".to_vec()));
    }
}
