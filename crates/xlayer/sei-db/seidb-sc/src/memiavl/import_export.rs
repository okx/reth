//! Tree import (stack-based construction) and export (post-order traversal).
//!
//! The `TreeImporter` reconstructs an IAVL tree from a stream of `ExportNode`s
//! produced in post-order. Leaf nodes (height == 0) are pushed onto a stack;
//! branch nodes (height > 0) pop their two children from the stack.
//!
//! The `Exporter` performs iterative post-order DFS traversal of an existing
//! in-memory tree, yielding `ExportNode`s that can be fed back into a
//! `TreeImporter` for snapshot creation.

use crate::memiavl::{
    node::{MemNode, Node, NodeRef},
    snapshot_writer,
};
use seidb_common::error::{Result, SeiDbError};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// Snapshot node for import/export (matches Go `SnapshotNode`).
pub struct ExportNode {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub version: i64,
    pub height: i8,
}

// ---------------------------------------------------------------------------
// TreeImporter — stack-based tree construction from post-order stream
// ---------------------------------------------------------------------------

/// Imports a stream of `ExportNode`s and writes a snapshot to disk.
///
/// Nodes must be provided in post-order (left leaves, right leaves, then their
/// parent branch). The importer maintains a stack of `NodeRef`s and writes the
/// final tree via `snapshot_writer::write_snapshot`.
pub struct TreeImporter {
    dir: PathBuf,
    version: u32,
    stack: Vec<NodeRef>,
}

impl TreeImporter {
    /// Create a new importer that will write a snapshot to `dir`.
    pub fn new(dir: &Path, version: i64) -> Self {
        Self { dir: dir.to_path_buf(), version: version as u32, stack: Vec::new() }
    }

    /// Add a node from the post-order stream.
    ///
    /// Leaf nodes (height == 0) are pushed directly. Branch nodes (height > 0)
    /// pop right then left children from the stack, construct a branch, and
    /// push the result.
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn add(&mut self, node: ExportNode) {
        let version = node.version as u32;

        if node.height == 0 {
            // Leaf node
            let mem = MemNode::new_leaf_node(node.key, node.value, version);
            self.stack.push(Arc::new(Node::Mem(mem)));
        } else {
            // Branch node — pop right then left
            let right = self.stack.pop().expect("branch node requires right child on stack");
            let left = self.stack.pop().expect("branch node requires left child on stack");

            // Build branch. new_branch_node derives height/size/key from children,
            // and version comes from the export stream.
            let mem = MemNode::new_branch_node(left, right, version);
            self.stack.push(Arc::new(Node::Mem(mem)));
        }
    }

    /// Finalize the import and write the snapshot to disk.
    ///
    /// The stack must contain exactly 0 (empty tree) or 1 (root) element.
    pub fn close(self) -> Result<()> {
        match self.stack.len() {
            0 => snapshot_writer::write_snapshot(&self.dir, self.version, None),
            1 => {
                let root = &self.stack[0];
                snapshot_writer::write_snapshot(&self.dir, self.version, Some(root.as_ref()))
            }
            n => Err(SeiDbError::Other(format!(
                "invalid node structure: stack has {} elements after import (expected 0 or 1)",
                n
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Exporter — iterative post-order DFS traversal
// ---------------------------------------------------------------------------

/// Stack entry for iterative post-order traversal.
struct StackEntry {
    node: NodeRef,
    expanded: bool,
}

/// Exports an IAVL tree as a stream of `ExportNode`s in post-order.
///
/// This matches Go's `ScanPostOrder` algorithm: branch nodes are first pushed
/// unexpanded, then their children are pushed. When a branch is visited the
/// second time (after children have been yielded), it is yielded itself.
pub struct Exporter {
    stack: Vec<StackEntry>,
}

impl Exporter {
    /// Create a new exporter for the given root node.
    ///
    /// If `root` is `None` the exporter immediately yields nothing.
    pub fn new(root: Option<&NodeRef>) -> Self {
        let stack = match root {
            Some(r) => vec![StackEntry { node: Arc::clone(r), expanded: false }],
            None => Vec::new(),
        };
        Self { stack }
    }

    /// Consume the exporter, dropping all remaining state.
    pub fn close(self) {
        // stack is dropped
    }
}

impl Iterator for Exporter {
    type Item = ExportNode;

    /// Return the next `ExportNode` in post-order, or `None` when done.
    fn next(&mut self) -> Option<ExportNode> {
        loop {
            let entry = self.stack.last_mut()?;

            if entry.node.is_leaf() || entry.expanded {
                // Yield this node.
                let node = self.stack.pop().unwrap().node;
                return Some(ExportNode {
                    key: node.key().to_vec(),
                    value: if node.is_leaf() { node.value().to_vec() } else { Vec::new() },
                    version: node.version() as i64,
                    height: node.height() as i8,
                });
            }

            // Branch not yet expanded — push right then left so left is
            // processed first (top of stack).
            entry.expanded = true;
            let right = entry.node.right().expect("branch must have right child").clone();
            let left = entry.node.left().expect("branch must have left child").clone();
            self.stack.push(StackEntry { node: right, expanded: false });
            self.stack.push(StackEntry { node: left, expanded: false });
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memiavl::snapshot::Snapshot;

    /// Build a leaf `NodeRef`.
    fn leaf(key: &[u8], value: &[u8], version: u32) -> NodeRef {
        Arc::new(Node::Mem(MemNode::new_leaf_node(key.to_vec(), value.to_vec(), version)))
    }

    /// Build a branch `NodeRef` from two children.
    fn branch(left: NodeRef, right: NodeRef, version: u32) -> NodeRef {
        Arc::new(Node::Mem(MemNode::new_branch_node(left, right, version)))
    }

    #[test]
    fn test_import_single_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("tree");

        let mut imp = TreeImporter::new(&d, 5);
        imp.add(ExportNode {
            key: b"hello".to_vec(),
            value: b"world".to_vec(),
            version: 5,
            height: 0,
        });
        imp.close().unwrap();

        // Verify snapshot
        let snap = Snapshot::open(&d).unwrap();
        assert_eq!(snap.version(), 5);
        assert_eq!(snap.leaf_count(), 1);
        assert_eq!(snap.node_count(), 0);

        let root = snap.root_node().unwrap();
        assert!(root.is_leaf());
        assert_eq!(root.key(), b"hello");
        assert_eq!(root.value(), Some(b"world".as_ref()));
    }

    #[test]
    fn test_import_small_tree() {
        // Import 3 leaves + 2 branches in post-order:
        //   leaf_a, leaf_b, branch(a,b), leaf_c, branch(ab,c)
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("tree");

        let mut imp = TreeImporter::new(&d, 10);

        // leaf a
        imp.add(ExportNode { key: b"aaa".to_vec(), value: b"v_a".to_vec(), version: 1, height: 0 });
        // leaf b
        imp.add(ExportNode { key: b"bbb".to_vec(), value: b"v_b".to_vec(), version: 1, height: 0 });
        // branch(a, b)
        imp.add(ExportNode { key: b"bbb".to_vec(), value: Vec::new(), version: 2, height: 1 });
        // leaf c
        imp.add(ExportNode { key: b"ccc".to_vec(), value: b"v_c".to_vec(), version: 1, height: 0 });
        // branch(ab, c)
        imp.add(ExportNode { key: b"ccc".to_vec(), value: Vec::new(), version: 3, height: 2 });

        imp.close().unwrap();

        let snap = Snapshot::open(&d).unwrap();
        assert_eq!(snap.version(), 10);
        assert_eq!(snap.leaf_count(), 3);
        assert_eq!(snap.node_count(), 2);

        // Verify leaf keys in order.
        assert_eq!(snap.leaf_key(0), b"aaa");
        assert_eq!(snap.leaf_key(1), b"bbb");
        assert_eq!(snap.leaf_key(2), b"ccc");

        let (k, v) = snap.leaf_key_value(0);
        assert_eq!(k, b"aaa");
        assert_eq!(v, b"v_a");
    }

    #[test]
    fn test_export_single_leaf() {
        let root = leaf(b"mykey", b"myval", 7);

        let mut exp = Exporter::new(Some(&root));
        let node = exp.next().unwrap();
        assert_eq!(node.height, 0);
        assert_eq!(node.key, b"mykey");
        assert_eq!(node.value, b"myval");
        assert_eq!(node.version, 7);

        // No more nodes.
        assert!(exp.next().is_none());
    }

    #[test]
    fn test_export_small_tree() {
        // 3-leaf tree:
        //        branch(v=3)
        //       /            \
        //   branch(v=2)    leaf_c(v=1)
        //   /       \
        // leaf_a    leaf_b
        let leaf_a = leaf(b"aaa", b"v_a", 1);
        let leaf_b = leaf(b"bbb", b"v_b", 1);
        let leaf_c = leaf(b"ccc", b"v_c", 1);

        let inner = branch(leaf_a, leaf_b, 2);
        let root = branch(inner, leaf_c, 3);

        let exp = Exporter::new(Some(&root));
        let nodes: Vec<_> = exp.collect();

        // Post-order: leaf_a, leaf_b, branch(a,b), leaf_c, branch(ab,c)
        assert_eq!(nodes.len(), 5);

        assert_eq!(nodes[0].height, 0);
        assert_eq!(nodes[0].key, b"aaa");
        assert_eq!(nodes[0].value, b"v_a");

        assert_eq!(nodes[1].height, 0);
        assert_eq!(nodes[1].key, b"bbb");
        assert_eq!(nodes[1].value, b"v_b");

        assert_eq!(nodes[2].height, 1);
        assert_eq!(nodes[2].key, b"bbb");
        assert!(nodes[2].value.is_empty());

        assert_eq!(nodes[3].height, 0);
        assert_eq!(nodes[3].key, b"ccc");
        assert_eq!(nodes[3].value, b"v_c");

        assert_eq!(nodes[4].height, 2);
        assert_eq!(nodes[4].key, b"ccc");
        assert!(nodes[4].value.is_empty());
    }

    #[test]
    fn test_import_export_roundtrip() {
        // Build a tree in memory, export it, import it, compare root hashes.
        let leaf_a = leaf(b"alpha", b"one", 1);
        let leaf_b = leaf(b"beta", b"two", 1);
        let leaf_c = leaf(b"gamma", b"three", 1);
        let leaf_d = leaf(b"delta", b"four", 1);

        let left = branch(leaf_a, leaf_b, 2);
        let right = branch(leaf_c, leaf_d, 2);
        let root = branch(left, right, 3);

        let original_hash = root.safe_hash();

        // Export
        let export_nodes: Vec<_> = Exporter::new(Some(&root)).collect();

        // Import into snapshot
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("roundtrip");

        let mut imp = TreeImporter::new(&d, 99);
        for n in export_nodes {
            imp.add(n);
        }
        imp.close().unwrap();

        // Open snapshot and verify root hash matches.
        let snap = Arc::new(Snapshot::open(&d).unwrap());
        assert_eq!(snap.version(), 99);
        assert_eq!(snap.leaf_count(), 4);
        assert_eq!(snap.root_hash(), original_hash);
    }

    #[test]
    fn test_import_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("empty");

        let imp = TreeImporter::new(&d, 1);
        imp.close().unwrap();

        let snap = Snapshot::open(&d).unwrap();
        assert!(snap.is_empty());
        assert_eq!(snap.version(), 1);
    }

    #[test]
    fn test_export_empty_tree() {
        let mut exp = Exporter::new(None);
        assert!(exp.next().is_none());
    }
}
