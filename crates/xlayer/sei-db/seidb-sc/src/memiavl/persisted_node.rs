use crate::memiavl::layout::{Leaves, Nodes};
use std::sync::Arc;

/// Shared backing data that `PersistedNode` instances reference.
///
/// Holds the raw bytes for branch nodes, leaf nodes, and key-value pairs.
/// Typically populated from mmap-ed snapshot files. All `PersistedNode` instances
/// created from the same snapshot share one `Arc<SnapshotData>`.
#[derive(Clone)]
pub struct SnapshotData {
    /// Serialized branch nodes (`NODE_SIZE` bytes each).
    pub(crate) nodes_buf: Vec<u8>,
    /// Serialized leaf nodes (`LEAF_SIZE` bytes each).
    pub(crate) leaves_buf: Vec<u8>,
    /// Key-value data. Format per entry: `[key_len_u32_le][key][value_len_u32_le][value]`.
    /// Leaf `key_offset` points to the `key_len_u32_le` prefix.
    pub(crate) kvs_buf: Vec<u8>,
}

impl SnapshotData {
    /// Creates a new `SnapshotData` from raw buffers.
    pub fn new(nodes_buf: Vec<u8>, leaves_buf: Vec<u8>, kvs_buf: Vec<u8>) -> Self {
        Self { nodes_buf, leaves_buf, kvs_buf }
    }

    /// Returns a `Nodes` accessor over the branch buffer.
    fn nodes(&self) -> Nodes<'_> {
        Nodes(&self.nodes_buf)
    }

    /// Returns a `Leaves` accessor over the leaf buffer.
    fn leaves(&self) -> Leaves<'_> {
        Leaves(&self.leaves_buf)
    }

    /// Returns the key bytes for the leaf at the given index.
    ///
    /// KVS layout at `key_offset`:
    ///   `[key_len: u32 LE][key bytes][value_len: u32 LE][value bytes]`
    ///
    /// The leaf's stored `key_offset` points to `key_len`. We skip the 4-byte
    /// length prefix to get the actual key.
    pub fn leaf_key(&self, index: u32) -> &[u8] {
        let leaf = self.leaves().get(index);
        let offset = leaf.key_offset() as usize + 4; // skip the key_len prefix in kvs
        let key_len = leaf.key_len() as usize;
        &self.kvs_buf[offset..offset + key_len]
    }

    /// Returns `(key, value)` for the leaf at the given index.
    pub fn leaf_key_value(&self, index: u32) -> (&[u8], &[u8]) {
        let leaf = self.leaves().get(index);
        let mut offset = leaf.key_offset() as usize + 4; // skip key_len prefix
        let key_len = leaf.key_len() as usize;
        let key = &self.kvs_buf[offset..offset + key_len];
        offset += key_len;
        // Read value length (u32 LE) then value bytes.
        let value_len =
            u32::from_le_bytes(self.kvs_buf[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let value = &self.kvs_buf[offset..offset + value_len];
        (key, value)
    }
}

// ---------------------------------------------------------------------------
// PersistedNode
// ---------------------------------------------------------------------------

/// A node in a persisted (mmap-backed) IAVL tree snapshot.
///
/// This is a lightweight handle: it stores an `Arc` to the shared data plus a
/// boolean distinguishing branch from leaf and an index into the corresponding
/// array. Navigation methods (`left`, `right`, `get`) create new `PersistedNode`
/// values cheaply via `Arc::clone`.
///
/// The index arithmetic for `left` / `right` mirrors the Go implementation in
/// `persisted_node.go` and relies on the post-order layout produced by the
/// snapshot writer.
#[derive(Clone)]
pub struct PersistedNode {
    pub(crate) data: Arc<SnapshotData>,
    pub(crate) is_leaf: bool,
    /// Index into `data.nodes_buf` (branch) or `data.leaves_buf` (leaf).
    pub(crate) index: u32,
}

impl PersistedNode {
    /// Creates a new `PersistedNode`.
    pub fn new(data: Arc<SnapshotData>, is_leaf: bool, index: u32) -> Self {
        Self { data, is_leaf, index }
    }

    /// Whether this is a leaf node.
    #[inline]
    pub fn is_leaf(&self) -> bool {
        self.is_leaf
    }

    /// Height of the node. Leaves always have height 0.
    #[inline]
    pub fn height(&self) -> u8 {
        if self.is_leaf {
            return 0;
        }
        self.data.nodes().get(self.index).height()
    }

    /// Version at which this node was last modified.
    #[inline]
    pub fn version(&self) -> u32 {
        if self.is_leaf {
            self.data.leaves().get(self.index).version()
        } else {
            self.data.nodes().get(self.index).version()
        }
    }

    /// Number of leaves in this subtree. Leaves return 1.
    #[inline]
    pub fn size(&self) -> i64 {
        if self.is_leaf {
            return 1;
        }
        self.data.nodes().get(self.index).size() as i64
    }

    /// Returns the key associated with this node.
    ///
    /// For leaf nodes this is the leaf's own key. For branch nodes it is the
    /// key of the `key_leaf` — the smallest key in the right subtree.
    pub fn key(&self) -> &[u8] {
        if self.is_leaf {
            self.data.leaf_key(self.index)
        } else {
            let key_leaf = self.data.nodes().get(self.index).key_leaf();
            self.data.leaf_key(key_leaf)
        }
    }

    /// Returns the value for a leaf node, or `None` for a branch node.
    pub fn value(&self) -> Option<&[u8]> {
        if !self.is_leaf {
            return None;
        }
        let (_, value) = self.data.leaf_key_value(self.index);
        Some(value)
    }

    /// The 32-byte hash stored in the snapshot for this node.
    pub fn hash(&self) -> &[u8] {
        use crate::memiavl::layout::{
            LEAF_SIZE, NODE_SIZE, OFFSET_HASH, OFFSET_LEAF_HASH, SIZE_HASH,
        };
        if self.is_leaf {
            let off = self.index as usize * LEAF_SIZE;
            &self.data.leaves_buf[off + OFFSET_LEAF_HASH..off + OFFSET_LEAF_HASH + SIZE_HASH]
        } else {
            let off = self.index as usize * NODE_SIZE;
            &self.data.nodes_buf[off + OFFSET_HASH..off + OFFSET_HASH + SIZE_HASH]
        }
    }

    /// Returns the left child. Panics on leaf nodes.
    ///
    /// Index arithmetic (from Go `persisted_node.go`):
    ///   start_leaf = index + 2 - size + pre_trees
    ///   If start_leaf + 1 == key_leaf  →  left child is a single leaf at start_leaf
    ///   Otherwise                      →  left child is a branch at key_leaf - pre_trees - 2
    pub fn left(&self) -> PersistedNode {
        assert!(!self.is_leaf, "cannot call left() on a leaf node");
        let node = self.data.nodes().get(self.index);
        let pre_trees = node.pre_trees() as u32;
        let size = node.size();
        let key_leaf = node.key_leaf();
        let start_leaf = get_start_leaf(self.index, size, pre_trees);

        if start_leaf + 1 == key_leaf {
            // Left child is a single leaf.
            PersistedNode::new(Arc::clone(&self.data), true, start_leaf)
        } else {
            PersistedNode::new(Arc::clone(&self.data), false, get_left_branch(key_leaf, pre_trees))
        }
    }

    /// Returns the right child. Panics on leaf nodes.
    ///
    /// Index arithmetic:
    ///   If key_leaf == end_leaf  →  right child is a single leaf at key_leaf
    ///   Otherwise               →  right child is the branch at index - 1
    pub fn right(&self) -> PersistedNode {
        assert!(!self.is_leaf, "cannot call right() on a leaf node");
        let node = self.data.nodes().get(self.index);
        let key_leaf = node.key_leaf();
        let pre_trees = node.pre_trees() as u32;
        let end_leaf = get_end_leaf(self.index, pre_trees);

        if key_leaf == end_leaf {
            // Right child is a single leaf.
            PersistedNode::new(Arc::clone(&self.data), true, key_leaf)
        } else {
            PersistedNode::new(Arc::clone(&self.data), false, self.index - 1)
        }
    }

    /// Binary-searches the leaf array for `key`.
    ///
    /// Returns `Some((value, leaf_index_within_subtree))` if found, or
    /// `None` with the insertion index if not found. The second element
    /// of the tuple is the relative leaf offset (like Go's `sort.Search` result).
    pub fn get(&self, key: &[u8]) -> (Option<Vec<u8>>, u32) {
        let (start, count) = if self.is_leaf {
            (self.index, 1u32)
        } else {
            let node = self.data.nodes().get(self.index);
            let pre_trees = node.pre_trees() as u32;
            let size = node.size();
            (get_start_leaf(self.index, size, pre_trees), size)
        };

        // Binary search: find smallest i where leaf_key(start + i) >= key
        let mut lo = 0u32;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let leaf_key = self.data.leaf_key(start + mid);
            if leaf_key < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        let i = lo;
        let leaf = i + start;
        if leaf >= start + count {
            return (None, i);
        }

        let (node_key, value) = self.data.leaf_key_value(leaf);
        if node_key != key {
            return (None, i);
        }

        (Some(value.to_vec()), i)
    }

    /// Returns `(key, value)` for the leaf at the given relative index within
    /// this node's subtree. Returns `None` if the index is out of range.
    pub fn get_by_index(&self, leaf_index: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.is_leaf {
            if leaf_index != 0 {
                return None;
            }
            let (k, v) = self.data.leaf_key_value(self.index);
            return Some((k.to_vec(), v.to_vec()));
        }
        let node = self.data.nodes().get(self.index);
        let pre_trees = node.pre_trees() as u32;
        let start_leaf = get_start_leaf(self.index, node.size(), pre_trees);
        let end_leaf = get_end_leaf(self.index, pre_trees);

        let i = start_leaf + leaf_index;
        if i > end_leaf {
            return None;
        }
        let (k, v) = self.data.leaf_key_value(i);
        Some((k.to_vec(), v.to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Index arithmetic helpers (match Go's getStartLeaf / getEndLeaf / getLeftBranch)
// ---------------------------------------------------------------------------

/// First leaf index in the subtree rooted at `index`.
///
///   start_leaf = index + 2 - size + pre_trees
#[inline]
fn get_start_leaf(index: u32, size: u32, pre_trees: u32) -> u32 {
    index + 2 - size + pre_trees
}

/// Last leaf index in the subtree rooted at `index`.
///
///   end_leaf = index + 1 + pre_trees
#[inline]
fn get_end_leaf(index: u32, pre_trees: u32) -> u32 {
    index + pre_trees + 1
}

/// Branch index of the left child.
///
///   left_branch = key_leaf - pre_trees - 2
#[inline]
fn get_left_branch(key_leaf: u32, pre_trees: u32) -> u32 {
    key_leaf - pre_trees - 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memiavl::layout::{
        LEAF_SIZE, NODE_SIZE, OFFSET_HASH, OFFSET_HEIGHT, OFFSET_KEY_LEAF, OFFSET_LEAF_HASH,
        OFFSET_LEAF_KEY_LEN, OFFSET_LEAF_KEY_OFFSET, OFFSET_LEAF_VERSION, OFFSET_PRE_TREES,
        OFFSET_SIZE, OFFSET_VERSION, SIZE_HASH,
    };

    // -- helpers to build raw buffers --

    fn make_branch(
        height: u8,
        pre_trees: u8,
        version: u32,
        size: u32,
        key_leaf: u32,
        hash: [u8; 32],
    ) -> [u8; NODE_SIZE] {
        let mut buf = [0u8; NODE_SIZE];
        buf[OFFSET_HEIGHT] = height;
        buf[OFFSET_PRE_TREES] = pre_trees;
        buf[OFFSET_VERSION..OFFSET_VERSION + 4].copy_from_slice(&version.to_le_bytes());
        buf[OFFSET_SIZE..OFFSET_SIZE + 4].copy_from_slice(&size.to_le_bytes());
        buf[OFFSET_KEY_LEAF..OFFSET_KEY_LEAF + 4].copy_from_slice(&key_leaf.to_le_bytes());
        buf[OFFSET_HASH..OFFSET_HASH + SIZE_HASH].copy_from_slice(&hash);
        buf
    }

    fn make_leaf(version: u32, key_len: u32, key_offset: u64, hash: [u8; 32]) -> [u8; LEAF_SIZE] {
        let mut buf = [0u8; LEAF_SIZE];
        buf[OFFSET_LEAF_VERSION..OFFSET_LEAF_VERSION + 4].copy_from_slice(&version.to_le_bytes());
        buf[OFFSET_LEAF_KEY_LEN..OFFSET_LEAF_KEY_LEN + 4].copy_from_slice(&key_len.to_le_bytes());
        buf[OFFSET_LEAF_KEY_OFFSET..OFFSET_LEAF_KEY_OFFSET + 8]
            .copy_from_slice(&key_offset.to_le_bytes());
        buf[OFFSET_LEAF_HASH..OFFSET_LEAF_HASH + SIZE_HASH].copy_from_slice(&hash);
        buf
    }

    /// Build a kvs buffer entry: [key_len_u32_le][key][value_len_u32_le][value].
    fn make_kv_entry(key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(key);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(value);
        buf
    }

    #[test]
    fn test_persisted_node_leaf() {
        // KVS: a single entry "abc" => "xyz"
        let kvs = make_kv_entry(b"abc", b"xyz");
        // Leaf points to offset 0 in kvs, key_len = 3
        let leaf = make_leaf(5, 3, 0, [0xAA; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf);

        let sd = Arc::new(SnapshotData::new(vec![], leaves_buf, kvs));
        let pn = PersistedNode::new(sd, true, 0);

        assert!(pn.is_leaf());
        assert_eq!(pn.height(), 0);
        assert_eq!(pn.version(), 5);
        assert_eq!(pn.size(), 1);
        assert_eq!(pn.key(), b"abc");
        assert_eq!(pn.value(), Some(b"xyz".as_ref()));
        assert_eq!(pn.hash(), &[0xAA; 32]);
    }

    #[test]
    fn test_persisted_node_branch_navigation() {
        // Build a small tree:
        //
        //       branch(idx=1)
        //      /              \
        //   leaf0("aa")     leaf1("bb")
        //
        // Post-order layout: leaves=[leaf0, leaf1], nodes=[branch]
        // Branch at index 0 in nodes array:
        //   height=1, pre_trees=0, version=1, size=2, key_leaf=1
        //
        // Verify left/right child navigation.

        let kvs_leaf0 = make_kv_entry(b"aa", b"v0");
        let kvs_leaf1 = make_kv_entry(b"bb", b"v1");
        let mut kvs = Vec::new();
        let off0 = 0u64;
        kvs.extend_from_slice(&kvs_leaf0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs_leaf1);

        let leaf0 = make_leaf(1, 2, off0, [0x11; 32]);
        let leaf1 = make_leaf(1, 2, off1, [0x22; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf0);
        leaves_buf.extend_from_slice(&leaf1);

        // Branch: index=0, height=1, pre_trees=0, version=1, size=2, key_leaf=1
        //
        // Checking left():
        //   start_leaf = index + 2 - size + pre_trees = 0 + 2 - 2 + 0 = 0
        //   key_leaf = 1
        //   start_leaf + 1 == key_leaf  →  left is leaf at index 0  ✓
        //
        // Checking right():
        //   end_leaf = index + 1 + pre_trees = 0 + 1 + 0 = 1
        //   key_leaf == end_leaf  →  right is leaf at index 1  ✓
        let branch = make_branch(1, 0, 1, 2, 1, [0x33; 32]);
        let mut nodes_buf = Vec::new();
        nodes_buf.extend_from_slice(&branch);

        let sd = Arc::new(SnapshotData::new(nodes_buf, leaves_buf, kvs));
        let root = PersistedNode::new(sd, false, 0);

        assert!(!root.is_leaf());
        assert_eq!(root.height(), 1);
        assert_eq!(root.version(), 1);
        assert_eq!(root.size(), 2);
        assert_eq!(root.key(), b"bb"); // key_leaf=1 → leaf1's key
        assert_eq!(root.hash(), &[0x33; 32]);

        let left = root.left();
        assert!(left.is_leaf());
        assert_eq!(left.key(), b"aa");
        assert_eq!(left.value(), Some(b"v0".as_ref()));

        let right = root.right();
        assert!(right.is_leaf());
        assert_eq!(right.key(), b"bb");
        assert_eq!(right.value(), Some(b"v1".as_ref()));
    }

    #[test]
    fn test_persisted_node_get_found() {
        // Two leaves: "aa" and "bb"
        let kvs_leaf0 = make_kv_entry(b"aa", b"v0");
        let kvs_leaf1 = make_kv_entry(b"bb", b"v1");
        let mut kvs = Vec::new();
        let off0 = 0u64;
        kvs.extend_from_slice(&kvs_leaf0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs_leaf1);

        let leaf0 = make_leaf(1, 2, off0, [0; 32]);
        let leaf1 = make_leaf(1, 2, off1, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf0);
        leaves_buf.extend_from_slice(&leaf1);

        let branch = make_branch(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_buf = Vec::new();
        nodes_buf.extend_from_slice(&branch);

        let sd = Arc::new(SnapshotData::new(nodes_buf, leaves_buf, kvs));
        let root = PersistedNode::new(sd, false, 0);

        let (val, idx) = root.get(b"aa");
        assert_eq!(val.as_deref(), Some(b"v0".as_ref()));
        assert_eq!(idx, 0);

        let (val, idx) = root.get(b"bb");
        assert_eq!(val.as_deref(), Some(b"v1".as_ref()));
        assert_eq!(idx, 1);
    }

    #[test]
    fn test_persisted_node_get_not_found() {
        let kvs_leaf0 = make_kv_entry(b"bb", b"v0");
        let kvs_leaf1 = make_kv_entry(b"dd", b"v1");
        let mut kvs = Vec::new();
        let off0 = 0u64;
        kvs.extend_from_slice(&kvs_leaf0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs_leaf1);

        let leaf0 = make_leaf(1, 2, off0, [0; 32]);
        let leaf1 = make_leaf(1, 2, off1, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf0);
        leaves_buf.extend_from_slice(&leaf1);

        let branch = make_branch(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_buf = Vec::new();
        nodes_buf.extend_from_slice(&branch);

        let sd = Arc::new(SnapshotData::new(nodes_buf, leaves_buf, kvs));
        let root = PersistedNode::new(sd, false, 0);

        // "aa" < "bb" → not found, insertion index 0
        let (val, idx) = root.get(b"aa");
        assert!(val.is_none());
        assert_eq!(idx, 0);

        // "cc" between "bb" and "dd" → not found, insertion index 1
        let (val, idx) = root.get(b"cc");
        assert!(val.is_none());
        assert_eq!(idx, 1);

        // "zz" > all → not found, insertion index 2
        let (val, idx) = root.get(b"zz");
        assert!(val.is_none());
        assert_eq!(idx, 2);
    }

    #[test]
    fn test_persisted_node_get_on_leaf() {
        let kvs = make_kv_entry(b"key", b"val");
        let leaf = make_leaf(1, 3, 0, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf);

        let sd = Arc::new(SnapshotData::new(vec![], leaves_buf, kvs));
        let pn = PersistedNode::new(sd, true, 0);

        let (val, idx) = pn.get(b"key");
        assert_eq!(val.as_deref(), Some(b"val".as_ref()));
        assert_eq!(idx, 0);

        let (val, _) = pn.get(b"other");
        assert!(val.is_none());
    }

    #[test]
    fn test_persisted_node_get_by_index() {
        let kvs_leaf0 = make_kv_entry(b"aa", b"v0");
        let kvs_leaf1 = make_kv_entry(b"bb", b"v1");
        let mut kvs = Vec::new();
        let off0 = 0u64;
        kvs.extend_from_slice(&kvs_leaf0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs_leaf1);

        let leaf0 = make_leaf(1, 2, off0, [0; 32]);
        let leaf1 = make_leaf(1, 2, off1, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf0);
        leaves_buf.extend_from_slice(&leaf1);

        let branch = make_branch(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_buf = Vec::new();
        nodes_buf.extend_from_slice(&branch);

        let sd = Arc::new(SnapshotData::new(nodes_buf, leaves_buf, kvs));
        let root = PersistedNode::new(sd, false, 0);

        let kv = root.get_by_index(0).unwrap();
        assert_eq!(kv.0, b"aa");
        assert_eq!(kv.1, b"v0");

        let kv = root.get_by_index(1).unwrap();
        assert_eq!(kv.0, b"bb");
        assert_eq!(kv.1, b"v1");

        assert!(root.get_by_index(2).is_none());
    }

    #[test]
    #[should_panic(expected = "cannot call left() on a leaf node")]
    fn test_left_on_leaf_panics() {
        let kvs = make_kv_entry(b"k", b"v");
        let leaf = make_leaf(1, 1, 0, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf);
        let sd = Arc::new(SnapshotData::new(vec![], leaves_buf, kvs));
        let pn = PersistedNode::new(sd, true, 0);
        let _ = pn.left();
    }

    #[test]
    #[should_panic(expected = "cannot call right() on a leaf node")]
    fn test_right_on_leaf_panics() {
        let kvs = make_kv_entry(b"k", b"v");
        let leaf = make_leaf(1, 1, 0, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf);
        let sd = Arc::new(SnapshotData::new(vec![], leaves_buf, kvs));
        let pn = PersistedNode::new(sd, true, 0);
        let _ = pn.right();
    }

    #[test]
    fn test_branch_value_returns_none() {
        let kvs = make_kv_entry(b"k", b"v");
        let leaf = make_leaf(1, 1, 0, [0; 32]);
        let mut leaves_buf = Vec::new();
        leaves_buf.extend_from_slice(&leaf);

        let branch = make_branch(1, 0, 1, 1, 0, [0; 32]);
        let mut nodes_buf = Vec::new();
        nodes_buf.extend_from_slice(&branch);

        let sd = Arc::new(SnapshotData::new(nodes_buf, leaves_buf, kvs));
        let pn = PersistedNode::new(sd, false, 0);
        assert!(pn.value().is_none());
    }
}
