use crate::memiavl::{
    arena::{FrozenArena, MutableArena, NodeIdx},
    node::NodeRef,
    rate_limiter::RateLimiter,
    snapshot::Snapshot,
    snapshot_writer,
    tree_algo::{
        compute_hash_recursive, get_arena, idx_to_node_ref, remove_recursive_arena,
        set_recursive_arena,
    },
};
use seidb_common::error::Result;
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, thread::JoinHandle};

/// A single change: `(key, Some(value))` for insert/update, `(key, None)` for delete.
pub type Change = (Vec<u8>, Option<Vec<u8>>);

/// A batch of changes to apply atomically.
pub type ChangeSet = Vec<Change>;

/// An in-memory IAVL tree supporting copy-on-write (CoW) sharing.
///
/// Uses arena-based node storage for the hot path (set/remove) to eliminate
/// per-node Arc allocations. Legacy `NodeRef` fields are kept for backward
/// compatibility with consumers that haven't migrated.
pub struct Tree {
    version: u32,
    /// Arena-based root index (primary, used for set/remove/get).
    root_idx: Option<NodeIdx>,
    /// Legacy root (populated lazily for consumers needing NodeRef).
    root: Option<NodeRef>,
    snapshot: Option<Arc<Snapshot>>,
    /// Frozen arenas from previous copy() calls, shared via Arc.
    frozen_arenas: Vec<Arc<FrozenArena>>,
    /// Mutable arena for the current block's allocations.
    arena: MutableArena,
    /// Current generation counter for arena indexing.
    current_gen: u16,
    initial_version: u32,
    cow_version: u32,
    zero_copy: bool,
    /// Channel sender for dispatching changesets to the background worker thread.
    async_tx: Option<crossbeam_channel::Sender<ChangeSet>>,
    /// Handle for the background worker thread that applies changesets.
    async_handle: Option<JoinHandle<Tree>>,
}

impl Tree {
    /// Create an empty tree at an arbitrary version.
    pub fn new_empty(version: u32, initial_version: u32) -> Self {
        Self {
            version,
            root_idx: None,
            root: None,
            snapshot: None,
            frozen_arenas: Vec::new(),
            arena: MutableArena::new(),
            current_gen: 1, // gen 0 is reserved for persisted nodes
            initial_version,
            cow_version: 0,
            zero_copy: true,
            async_tx: None,
            async_handle: None,
        }
    }

    /// Create a tree from a persisted snapshot.
    ///
    /// The version is taken from the snapshot metadata. If the snapshot is
    /// non-empty, the root node is constructed as a persisted NodeIdx.
    /// `cow_version` is set to the snapshot version to protect existing nodes.
    pub fn new_from_snapshot(snapshot: Arc<Snapshot>) -> Self {
        let version = snapshot.version();
        let root_idx = snapshot.root_node().map(|pn| NodeIdx::persisted(pn.index, pn.is_leaf()));
        Self {
            version,
            root_idx,
            root: None,
            snapshot: Some(snapshot),
            frozen_arenas: Vec::new(),
            arena: MutableArena::new(),
            current_gen: 1,
            initial_version: 0,
            cow_version: version,
            zero_copy: false,
            async_tx: None,
            async_handle: None,
        }
    }

    /// Insert or update a key-value pair.
    ///
    /// Uses the arena-based hot path, eliminating Arc allocations.
    pub fn set(&mut self, key: &[u8], value: &[u8]) {
        let ver = self.next_version_u32();
        // Invalidate legacy root
        self.root = None;
        let (new_root, _updated) = set_recursive_arena(
            &mut self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            self.root_idx,
            key,
            value,
            ver,
            self.cow_version,
        );
        self.root_idx = Some(new_root);
    }

    /// Like [`set`] but takes owned key/value to avoid allocation when the
    /// caller already has `Vec<u8>` data.
    pub fn set_owned(&mut self, key: Vec<u8>, value: Vec<u8>) {
        // Arena-based set already takes &[u8], so we just pass references.
        self.set(&key, &value);
    }

    /// Remove a key from the tree.
    ///
    /// Returns `Some(value)` if the key was found and removed, `None` otherwise.
    pub fn remove(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let root = self.root_idx?;
        let ver = self.next_version_u32();
        // Invalidate legacy root
        self.root = None;
        let (value, new_root, _) = remove_recursive_arena(
            &mut self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            root,
            key,
            ver,
            self.cow_version,
        );
        self.root_idx = new_root;
        value
    }

    /// Apply a batch of changes: `Some(value)` inserts/updates, `None` removes.
    pub fn apply_change_set(&mut self, changes: &[(Vec<u8>, Option<Vec<u8>>)]) {
        for (key, value_opt) in changes {
            match value_opt {
                Some(value) => self.set(key, value),
                None => {
                    self.remove(key);
                }
            }
        }
    }

    /// Like [`apply_change_set`] but consumes the changeset, passing owned
    /// key/value data through to avoid cloning.
    pub fn apply_change_set_owned(&mut self, changes: Vec<(Vec<u8>, Option<Vec<u8>>)>) {
        for (key, value_opt) in changes {
            match value_opt {
                Some(value) => self.set_owned(key, value),
                None => {
                    self.remove(&key);
                }
            }
        }
    }

    /// Apply changes directly from protobuf `KvPair` slice, avoiding
    /// the intermediate `Vec<(Vec<u8>, Option<Vec<u8>>)>` allocation.
    pub fn apply_kvpairs(&mut self, pairs: &[seidb_proto::KvPair]) {
        for pair in pairs {
            if pair.delete {
                self.remove(&pair.key);
            } else {
                self.set(&pair.key, &pair.value);
            }
        }
    }

    /// Like [`apply_kvpairs`] but accepts a slice of references (used by
    /// parallel multi-tree apply where pairs are grouped by tree).
    pub fn apply_kvpair_refs(&mut self, pairs: &[&seidb_proto::KvPair]) {
        for pair in pairs {
            if pair.delete {
                self.remove(&pair.key);
            } else {
                self.set(&pair.key, &pair.value);
            }
        }
    }

    /// Send a changeset to the background worker thread for concurrent processing.
    pub fn apply_change_set_async(&mut self, changes: ChangeSet) {
        if self.async_tx.is_none() {
            self.start_background_write();
        }
        self.async_tx
            .as_ref()
            .expect("async_tx must be set after start_background_write")
            .send(changes)
            .expect("background worker thread should be alive");
    }

    /// Spawn a background thread that will process changesets sent via
    /// [`apply_change_set_async`].
    pub fn start_background_write(&mut self) {
        if self.async_tx.is_some() {
            return; // already started
        }
        let (tx, rx) = crossbeam_channel::bounded::<ChangeSet>(1000);
        let mut worker_tree = self.take_for_async();

        let handle = std::thread::spawn(move || {
            for changes in rx {
                worker_tree.apply_change_set_owned(changes);
                let _ = worker_tree.save_version(false);
            }
            worker_tree
        });

        self.async_tx = Some(tx);
        self.async_handle = Some(handle);
    }

    /// Close the channel and join the background worker thread, merging the
    /// resulting tree state back into `self`.
    pub fn wait_to_complete_async_write(&mut self) {
        if let Some(tx) = self.async_tx.take() {
            drop(tx);
        }
        if let Some(handle) = self.async_handle.take() {
            let worker_tree = handle.join().expect("background worker thread panicked");
            self.merge_from_async(worker_tree);
        }
    }

    /// Move the tree's mutable state into a new `Tree` for the background
    /// worker, leaving `self` in an empty placeholder state.
    fn take_for_async(&mut self) -> Tree {
        Tree {
            version: self.version,
            root_idx: self.root_idx.take(),
            root: self.root.take(),
            snapshot: self.snapshot.clone(),
            frozen_arenas: self.frozen_arenas.clone(),
            arena: std::mem::replace(&mut self.arena, MutableArena::new()),
            current_gen: self.current_gen,
            initial_version: self.initial_version,
            cow_version: self.cow_version,
            zero_copy: self.zero_copy,
            async_tx: None,
            async_handle: None,
        }
    }

    /// Restore tree state from the worker tree after background processing
    /// completes.
    fn merge_from_async(&mut self, other: Tree) {
        self.version = other.version;
        self.root_idx = other.root_idx;
        self.root = other.root;
        self.frozen_arenas = other.frozen_arenas;
        self.arena = other.arena;
        self.current_gen = other.current_gen;
        self.initial_version = other.initial_version;
        self.cow_version = other.cow_version;
    }

    /// Increment the version and optionally compute the root hash.
    ///
    /// Returns `(root_hash, new_version)`. If `update_hash` is false the
    /// returned hash vec is empty.
    pub fn save_version(&mut self, update_hash: bool) -> Result<(Vec<u8>, i64)> {
        let hash = if update_hash { self.root_hash() } else { Vec::new() };
        self.version = self.next_version_u32();
        Ok((hash, self.version as i64))
    }

    /// Return an O(1) copy of the tree that shares all nodes.
    ///
    /// Freezes the current mutable arena into an Arc<FrozenArena>,
    /// bumps the generation, and creates a new empty MutableArena.
    /// The copy shares frozen arenas and snapshot via Arc.
    pub fn copy(&mut self) -> Self {
        if self.root_idx.is_some() {
            self.cow_version = self.version;
        }

        // Freeze current arena and share it
        let frozen = Arc::new(std::mem::replace(&mut self.arena, MutableArena::new()).freeze());
        self.frozen_arenas.push(frozen.clone());

        let copy = Self {
            version: self.version,
            root_idx: self.root_idx,
            root: None,
            snapshot: self.snapshot.clone(),
            frozen_arenas: self.frozen_arenas.clone(),
            arena: MutableArena::new(),
            current_gen: self.current_gen + 1, // copy gets a new gen
            initial_version: self.initial_version,
            cow_version: self.version,
            zero_copy: self.zero_copy,
            async_tx: None,
            async_handle: None,
        };

        // Original tree also bumps generation since old arena is now frozen
        self.current_gen += 1;

        // Invalidate legacy root
        self.root = None;

        copy
    }

    /// Return an O(1) read-only copy of the tree.
    ///
    /// Unlike [`copy`], this does NOT freeze the arena. Instead it creates
    /// a frozen snapshot of the current arena for the copy. The original
    /// tree is NOT modified (no gen bump needed since this is read-only).
    pub fn snapshot_copy(&self) -> Self {
        // For snapshot_copy, we need to make the current arena's data
        // available to the copy. We create a frozen copy of the arena.
        // Since we can't mutate self, the copy needs its own frozen arenas
        // that include a frozen version of our current mutable arena.
        let mut copy_frozen = self.frozen_arenas.clone();
        // Clone current mutable arena's nodes into a frozen arena for the copy.
        let mut temp_arena = MutableArena::new();
        for i in 0..self.arena.len() {
            temp_arena.alloc(self.arena.get(i as u32).clone());
        }
        let has_mutable_nodes = !temp_arena.is_empty();
        if has_mutable_nodes {
            copy_frozen.push(Arc::new(temp_arena.freeze()));
        }

        Self {
            version: self.version,
            root_idx: self.root_idx,
            root: None,
            snapshot: self.snapshot.clone(),
            frozen_arenas: copy_frozen,
            arena: MutableArena::new(),
            // If we added a frozen arena for current gen, the copy's gen should be current_gen+1
            // so it doesn't try to read from its own (empty) mutable arena
            current_gen: if has_mutable_nodes { self.current_gen + 1 } else { self.current_gen },
            initial_version: self.initial_version,
            cow_version: self.version,
            zero_copy: self.zero_copy,
            async_tx: None,
            async_handle: None,
        }
    }

    /// Look up a key and return its value.
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        get_arena(
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            self.root_idx,
            key,
        )
        .map(|(value, _index)| value)
    }

    /// Returns `true` if the key exists in the tree.
    pub fn has(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Look up a key and return `(index, value)`.
    ///
    /// Returns `(-1, None)` if the key is not found.
    pub fn get_with_index(&self, key: &[u8]) -> (i64, Option<Vec<u8>>) {
        match get_arena(
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            self.root_idx,
            key,
        ) {
            Some((value, index)) => (index as i64, Some(value)),
            None => (-1, None),
        }
    }

    /// Look up a key-value pair by its in-order index.
    pub fn get_by_index(&self, index: i64) -> Option<(Vec<u8>, Vec<u8>)> {
        // Delegate to legacy NodeRef for now (index-based lookup is complex)
        let root_ref = self.ensure_root_ref()?;
        root_ref.get_by_index(index)
    }

    /// Compute and return the root hash.
    ///
    /// For an empty tree this returns SHA-256 of the empty string.
    pub fn root_hash(&self) -> Vec<u8> {
        match self.root_idx {
            None => Sha256::digest([]).to_vec(),
            Some(idx) => {
                let hash = compute_hash_recursive(
                    &self.arena,
                    &self.frozen_arenas,
                    &self.snapshot,
                    self.current_gen,
                    idx,
                );
                hash.to_vec()
            }
        }
    }

    /// The current tree version.
    pub fn version(&self) -> i64 {
        self.version as i64
    }

    /// Returns `true` if the tree has no nodes.
    pub fn is_empty(&self) -> bool {
        self.root_idx.is_none()
    }

    /// Returns a reference to the root node, or `None` if the tree is empty.
    ///
    /// Lazily constructs a NodeRef from the arena-based root for backward
    /// compatibility with consumers (iterator, proof, snapshot_writer).
    pub fn root_ref(&self) -> Option<&NodeRef> {
        // Can't lazily build here since we need &mut self.
        // Return cached legacy root if available.
        self.root.as_ref()
    }

    /// Ensure the legacy root NodeRef is populated and return it.
    /// This is used by methods that need NodeRef compatibility (iterator, proof).
    pub fn ensure_root_ref(&self) -> Option<NodeRef> {
        let idx = self.root_idx?;
        Some(idx_to_node_ref(
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            idx,
        ))
    }

    /// Build the legacy NodeRef root and cache it. Needed before operations
    /// that use the old Arc-based API (write_snapshot, iterator, proof).
    pub fn materialize_root_ref(&mut self) {
        if self.root.is_some() {
            return;
        }
        if let Some(idx) = self.root_idx {
            self.root = Some(idx_to_node_ref(
                &self.arena,
                &self.frozen_arenas,
                &self.snapshot,
                self.current_gen,
                idx,
            ));
        }
    }

    /// Set the initial version (used for stores created mid-chain).
    pub fn set_initial_version(&mut self, v: u32) {
        self.initial_version = v;
    }

    /// Toggle zero-copy mode for get/iterator operations.
    pub fn set_zero_copy(&mut self, zc: bool) {
        self.zero_copy = zc;
    }

    /// Release references to the root node and snapshot.
    pub fn close(&mut self) -> Result<()> {
        self.root_idx = None;
        self.root = None;
        self.snapshot = None;
        Ok(())
    }

    /// Write the current tree state to a snapshot directory.
    pub fn write_snapshot(&self, dir: &Path) -> Result<()> {
        snapshot_writer::write_snapshot_arena(
            dir,
            self.version,
            self.root_idx,
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
        )
    }

    /// Write the current tree state to a snapshot directory with optional rate limiting.
    pub fn write_snapshot_with_limiter(
        &self,
        dir: &Path,
        limiter: Option<&RateLimiter>,
    ) -> Result<()> {
        snapshot_writer::write_snapshot_arena_with_limiter(
            dir,
            self.version,
            self.root_idx,
            &self.arena,
            &self.frozen_arenas,
            &self.snapshot,
            self.current_gen,
            limiter,
        )
    }

    /// Compute the next version, compatible with Go's `nextVersionU32`.
    fn next_version_u32(&self) -> u32 {
        if self.version == 0 && self.initial_version > 1 {
            self.initial_version
        } else {
            self.version + 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_empty() {
        let tree = Tree::new_empty(0, 0);
        assert!(tree.is_empty());
        assert_eq!(tree.version(), 0);
        assert!(tree.get(b"any").is_none());
        assert!(!tree.has(b"any"));
        // Empty tree root hash is SHA-256 of empty input.
        let expected_empty_hash = Sha256::digest([]).to_vec();
        assert_eq!(tree.root_hash(), expected_empty_hash);
    }

    #[test]
    fn test_tree_set_get() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"hello", b"world");
        assert!(!tree.is_empty());
        assert_eq!(tree.get(b"hello"), Some(b"world".to_vec()));
        assert!(tree.has(b"hello"));
        assert!(tree.get(b"missing").is_none());
    }

    #[test]
    fn test_tree_set_update() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"key", b"val1");
        assert_eq!(tree.get(b"key"), Some(b"val1".to_vec()));
        tree.set(b"key", b"val2");
        assert_eq!(tree.get(b"key"), Some(b"val2".to_vec()));
    }

    #[test]
    fn test_tree_remove() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"aaa", b"v1");
        tree.set(b"bbb", b"v2");
        assert!(tree.has(b"aaa"));

        let removed = tree.remove(b"aaa");
        assert_eq!(removed, Some(b"v1".to_vec()));
        assert!(!tree.has(b"aaa"));
        assert!(tree.has(b"bbb"));

        // Removing non-existent key returns None.
        let removed = tree.remove(b"zzz");
        assert!(removed.is_none());

        // Removing from empty tree returns None.
        let mut empty = Tree::new_empty(0, 0);
        assert!(empty.remove(b"x").is_none());
    }

    #[test]
    fn test_tree_save_version() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"k", b"v");

        let (hash, ver) = tree.save_version(true).unwrap();
        assert_eq!(ver, 1);
        assert_eq!(hash.len(), 32);
        assert_eq!(tree.version(), 1);

        // Save again without updating hash.
        tree.set(b"k2", b"v2");
        let (hash2, ver2) = tree.save_version(false).unwrap();
        assert_eq!(ver2, 2);
        assert!(hash2.is_empty());
    }

    #[test]
    fn test_tree_save_version_initial_version() {
        let mut tree = Tree::new_empty(0, 5);
        tree.set(b"k", b"v");

        // First save should jump to initial_version (5) since version==0 and initial_version > 1.
        let (_hash, ver) = tree.save_version(false).unwrap();
        assert_eq!(ver, 5);

        // Second save increments normally.
        let (_hash, ver) = tree.save_version(false).unwrap();
        assert_eq!(ver, 6);
    }

    #[test]
    fn test_tree_root_hash_deterministic() {
        let mut tree1 = Tree::new_empty(0, 0);
        tree1.set(b"aaa", b"111");
        tree1.set(b"bbb", b"222");
        tree1.set(b"ccc", b"333");

        let mut tree2 = Tree::new_empty(0, 0);
        tree2.set(b"aaa", b"111");
        tree2.set(b"bbb", b"222");
        tree2.set(b"ccc", b"333");

        assert_eq!(tree1.root_hash(), tree2.root_hash());
        assert_eq!(tree1.root_hash().len(), 32);

        // Different data produces different hash.
        let mut tree3 = Tree::new_empty(0, 0);
        tree3.set(b"aaa", b"111");
        tree3.set(b"bbb", b"999");
        tree3.set(b"ccc", b"333");
        assert_ne!(tree1.root_hash(), tree3.root_hash());
    }

    #[test]
    fn test_tree_copy_cow() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"key1", b"val1");
        tree.set(b"key2", b"val2");
        tree.save_version(true).unwrap();

        // Copy the tree.
        let copy = tree.copy();
        assert_eq!(copy.get(b"key1"), Some(b"val1".to_vec()));
        assert_eq!(copy.get(b"key2"), Some(b"val2".to_vec()));

        // Modify the original — copy should be unaffected.
        tree.set(b"key1", b"modified");
        tree.set(b"key3", b"val3");

        assert_eq!(copy.get(b"key1"), Some(b"val1".to_vec()));
        assert!(copy.get(b"key3").is_none());
        assert_eq!(tree.get(b"key1"), Some(b"modified".to_vec()));
        assert_eq!(tree.get(b"key3"), Some(b"val3".to_vec()));
    }

    #[test]
    fn test_tree_from_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Build a tree, write snapshot, then load it back.
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"alpha", b"one");
        tree.set(b"beta", b"two");
        tree.set(b"gamma", b"three");
        tree.save_version(true).unwrap();

        let original_hash = tree.root_hash();
        tree.write_snapshot(d).unwrap();

        // Open snapshot and create a new tree from it.
        let snapshot = Snapshot::open(d).unwrap();
        let loaded = Tree::new_from_snapshot(snapshot);

        assert_eq!(loaded.version(), 1);
        assert!(!loaded.is_empty());
        assert_eq!(loaded.root_hash(), original_hash);

        // Verify key lookups through persisted nodes.
        assert_eq!(loaded.get(b"alpha"), Some(b"one".to_vec()));
        assert_eq!(loaded.get(b"beta"), Some(b"two".to_vec()));
        assert_eq!(loaded.get(b"gamma"), Some(b"three".to_vec()));
        assert!(loaded.get(b"missing").is_none());
    }

    #[test]
    fn test_tree_apply_change_set() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"exist", b"old");

        let changes = vec![
            (b"new_key".to_vec(), Some(b"new_val".to_vec())),
            (b"exist".to_vec(), Some(b"updated".to_vec())),
            (b"to_delete".to_vec(), None),
        ];
        tree.apply_change_set(&changes);

        assert_eq!(tree.get(b"new_key"), Some(b"new_val".to_vec()));
        assert_eq!(tree.get(b"exist"), Some(b"updated".to_vec()));
        assert!(!tree.has(b"to_delete"));
    }

    #[test]
    fn test_tree_get_by_index() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"aaa", b"v1");
        tree.set(b"bbb", b"v2");
        tree.set(b"ccc", b"v3");

        assert_eq!(tree.get_by_index(0), Some((b"aaa".to_vec(), b"v1".to_vec())));
        assert_eq!(tree.get_by_index(1), Some((b"bbb".to_vec(), b"v2".to_vec())));
        assert_eq!(tree.get_by_index(2), Some((b"ccc".to_vec(), b"v3".to_vec())));
        assert!(tree.get_by_index(3).is_none());
        assert!(tree.get_by_index(-1).is_none());

        // Empty tree.
        let empty = Tree::new_empty(0, 0);
        assert!(empty.get_by_index(0).is_none());
    }

    #[test]
    fn test_tree_get_with_index() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"aaa", b"v1");
        tree.set(b"bbb", b"v2");

        let (idx, val) = tree.get_with_index(b"aaa");
        assert_eq!(idx, 0);
        assert_eq!(val, Some(b"v1".to_vec()));

        let (idx, val) = tree.get_with_index(b"bbb");
        assert_eq!(idx, 1);
        assert_eq!(val, Some(b"v2".to_vec()));

        let (idx, val) = tree.get_with_index(b"missing");
        assert_eq!(idx, -1);
        assert!(val.is_none());
    }

    #[test]
    fn test_tree_write_snapshot_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Build a larger tree.
        let mut tree = Tree::new_empty(0, 0);
        for i in 0..50u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            tree.set(key.as_bytes(), val.as_bytes());
        }
        tree.save_version(true).unwrap();

        let original_hash = tree.root_hash();
        tree.write_snapshot(d).unwrap();

        // Reload from snapshot.
        let snapshot = Snapshot::open(d).unwrap();
        let loaded = Tree::new_from_snapshot(snapshot);

        assert_eq!(loaded.version(), 1);
        assert_eq!(loaded.root_hash(), original_hash);

        // Verify all keys.
        for i in 0..50u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            assert_eq!(
                loaded.get(key.as_bytes()),
                Some(val.into_bytes()),
                "key {} mismatch after snapshot roundtrip",
                key
            );
        }
    }

    #[test]
    fn test_tree_close() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"k", b"v");
        tree.close().unwrap();
        assert!(tree.is_empty());
        assert!(tree.get(b"k").is_none());
    }

    #[test]
    fn test_async_change_set() {
        let mut tree = Tree::new_empty(0, 0);

        // Buffer a changeset asynchronously.
        let changes = vec![
            (b"alpha".to_vec(), Some(b"one".to_vec())),
            (b"beta".to_vec(), Some(b"two".to_vec())),
        ];
        tree.apply_change_set_async(changes);

        // Data should NOT be visible yet.
        assert!(tree.get(b"alpha").is_none());
        assert!(tree.get(b"beta").is_none());

        // After waiting, data becomes visible.
        tree.wait_to_complete_async_write();
        assert_eq!(tree.get(b"alpha"), Some(b"one".to_vec()));
        assert_eq!(tree.get(b"beta"), Some(b"two".to_vec()));

        // Version should have been bumped by the save_version call inside wait.
        assert_eq!(tree.version(), 1);
    }

    #[test]
    fn test_async_multiple() {
        let mut tree = Tree::new_empty(0, 0);

        // Buffer several batches.
        tree.apply_change_set_async(vec![
            (b"k1".to_vec(), Some(b"v1".to_vec())),
            (b"k2".to_vec(), Some(b"v2".to_vec())),
        ]);
        tree.apply_change_set_async(vec![
            (b"k3".to_vec(), Some(b"v3".to_vec())),
            (b"k1".to_vec(), None), // delete k1 in second batch
        ]);
        tree.apply_change_set_async(vec![(b"k4".to_vec(), Some(b"v4".to_vec()))]);

        // start_background_write is a no-op when already started.
        tree.start_background_write();

        // Nothing visible yet.
        assert!(tree.get(b"k1").is_none());

        tree.wait_to_complete_async_write();

        // k1 was inserted then deleted.
        assert!(tree.get(b"k1").is_none());
        assert_eq!(tree.get(b"k2"), Some(b"v2".to_vec()));
        assert_eq!(tree.get(b"k3"), Some(b"v3".to_vec()));
        assert_eq!(tree.get(b"k4"), Some(b"v4".to_vec()));

        // Three batches → three save_version calls → version 3.
        assert_eq!(tree.version(), 3);

        // No-op when no background worker is active.
        tree.wait_to_complete_async_write();
        assert_eq!(tree.version(), 3); // no change
    }

    #[test]
    fn test_async_change_set_concurrent() {
        // Verify that the background thread actually processes changesets.
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"pre_existing", b"yes");
        tree.save_version(false).unwrap();
        assert_eq!(tree.version(), 1);

        // Send changes to background worker.
        tree.apply_change_set_async(vec![
            (b"concurrent_key".to_vec(), Some(b"concurrent_val".to_vec())),
            (b"pre_existing".to_vec(), Some(b"updated".to_vec())),
        ]);

        // Main tree root was moved to worker — reads return None.
        assert!(tree.get(b"pre_existing").is_none());

        // Wait for worker to finish and merge back.
        tree.wait_to_complete_async_write();

        assert_eq!(tree.get(b"concurrent_key"), Some(b"concurrent_val".to_vec()));
        assert_eq!(tree.get(b"pre_existing"), Some(b"updated".to_vec()));
        // Version 1 (initial) + 1 (async batch) = 2.
        assert_eq!(tree.version(), 2);
    }

    #[test]
    fn test_async_multiple_concurrent() {
        // Multiple batches processed in a single background worker session.
        let mut tree = Tree::new_empty(0, 0);

        for i in 0..100u32 {
            let changes = vec![(
                format!("key_{:04}", i).into_bytes(),
                Some(format!("val_{:04}", i).into_bytes()),
            )];
            tree.apply_change_set_async(changes);
        }

        tree.wait_to_complete_async_write();

        // All 100 keys should be present.
        for i in 0..100u32 {
            let key = format!("key_{:04}", i);
            let val = format!("val_{:04}", i);
            assert_eq!(tree.get(key.as_bytes()), Some(val.into_bytes()), "missing key {}", key);
        }
        // 100 batches → 100 save_version calls.
        assert_eq!(tree.version(), 100);
    }

    #[test]
    fn test_async_wait_returns_data() {
        // After wait, all data from the background worker is accessible.
        let mut tree = Tree::new_empty(0, 0);

        tree.apply_change_set_async(vec![
            (b"a".to_vec(), Some(b"1".to_vec())),
            (b"b".to_vec(), Some(b"2".to_vec())),
        ]);
        tree.apply_change_set_async(vec![
            (b"c".to_vec(), Some(b"3".to_vec())),
            (b"a".to_vec(), None), // delete a
        ]);

        tree.wait_to_complete_async_write();

        assert!(tree.get(b"a").is_none());
        assert_eq!(tree.get(b"b"), Some(b"2".to_vec()));
        assert_eq!(tree.get(b"c"), Some(b"3".to_vec()));

        // Hash should be computable after merge.
        let hash = tree.root_hash();
        assert_eq!(hash.len(), 32);
        assert_eq!(tree.version(), 2);
    }

    #[test]
    fn test_multitree_wal_replay_parallel() {
        // Simulate WAL replay with multiple trees processing concurrently.
        // Each tree gets its own background worker.
        use std::time::Instant;

        const NUM_TREES: usize = 4;
        const BATCHES_PER_TREE: usize = 200;
        const KEYS_PER_BATCH: usize = 50;

        // Create trees.
        let mut trees: Vec<Tree> = (0..NUM_TREES).map(|_| Tree::new_empty(0, 0)).collect();

        let start = Instant::now();

        // Dispatch changesets to all trees (each tree processes in its own thread).
        for batch_idx in 0..BATCHES_PER_TREE {
            for (tree_idx, tree) in trees.iter_mut().enumerate() {
                let changes: ChangeSet = (0..KEYS_PER_BATCH)
                    .map(|k| {
                        let key = format!("t{}_b{:04}_k{:04}", tree_idx, batch_idx, k).into_bytes();
                        let val = format!("v{}", batch_idx * KEYS_PER_BATCH + k).into_bytes();
                        (key, Some(val))
                    })
                    .collect();
                tree.apply_change_set_async(changes);
            }
        }

        // Wait for all trees to finish.
        for tree in &mut trees {
            tree.wait_to_complete_async_write();
        }

        let elapsed = start.elapsed();

        // Verify each tree has the correct data.
        for (tree_idx, tree) in trees.iter().enumerate() {
            assert_eq!(tree.version() as usize, BATCHES_PER_TREE);
            // Spot-check a few keys.
            let key = format!("t{}_b0000_k0000", tree_idx);
            assert!(tree.get(key.as_bytes()).is_some(), "missing key {}", key);
            let last_key =
                format!("t{}_b{:04}_k{:04}", tree_idx, BATCHES_PER_TREE - 1, KEYS_PER_BATCH - 1);
            assert!(tree.get(last_key.as_bytes()).is_some(), "missing key {}", last_key);
        }

        // Log timing for manual inspection (not a hard assertion).
        eprintln!(
            "Parallel WAL replay: {} trees x {} batches x {} keys/batch in {:?}",
            NUM_TREES, BATCHES_PER_TREE, KEYS_PER_BATCH, elapsed
        );
    }
}
