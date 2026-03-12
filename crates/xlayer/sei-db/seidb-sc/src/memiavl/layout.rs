// Fixed-size on-disk layout for IAVL branch and leaf nodes.
//
// All integers are little-endian. The format matches the Go memiavl implementation.
//
// Branch node (48 bytes):
//   [0]    height    : u8
//   [1]    preTrees  : u8
//   [2..4] _padding  : 2 bytes
//   [4..8] version   : u32 LE
//   [8..12] size     : u32 LE  (number of leaves in subtree)
//   [12..16] keyLeaf : u32 LE  (leaf index of smallest key in right subtree)
//   [16..48] hash    : 32 bytes (SHA-256)
//
// Leaf node (48 bytes):
//   [0..4]  version    : u32 LE
//   [4..8]  keyLen     : u32 LE
//   [8..16] keyOffset  : u64 LE  (byte offset into kvs file)
//   [16..48] hash      : 32 bytes (SHA-256)

// -- Branch node constants --
pub const OFFSET_HEIGHT: usize = 0;
pub const OFFSET_PRE_TREES: usize = 1;
pub const OFFSET_VERSION: usize = 4;
pub const OFFSET_SIZE: usize = 8;
pub const OFFSET_KEY_LEAF: usize = 12;
pub const OFFSET_HASH: usize = 16;
pub const SIZE_HASH: usize = 32;

/// Size of a serialized branch node in bytes.
pub const NODE_SIZE: usize = OFFSET_HASH + SIZE_HASH; // 48

// -- Leaf node constants --
pub const OFFSET_LEAF_VERSION: usize = 0;
pub const OFFSET_LEAF_KEY_LEN: usize = 4;
pub const OFFSET_LEAF_KEY_OFFSET: usize = 8;
pub const OFFSET_LEAF_HASH: usize = 16;

/// Size of a serialized leaf node in bytes.
pub const LEAF_SIZE: usize = OFFSET_LEAF_HASH + SIZE_HASH; // 48

// ---------------------------------------------------------------------------
// NodeLayout — zero-copy accessor for a 48-byte branch record
// ---------------------------------------------------------------------------

/// Zero-copy view into a 48-byte branch node record.
#[derive(Clone, Copy)]
pub struct NodeLayout<'a>(pub(crate) &'a [u8; NODE_SIZE]);

impl<'a> NodeLayout<'a> {
    /// Height of this branch node (always >= 1).
    #[inline]
    pub fn height(&self) -> u8 {
        self.0[OFFSET_HEIGHT]
    }

    /// Number of pre-order subtrees before this node's subtree.
    #[inline]
    pub fn pre_trees(&self) -> u8 {
        self.0[OFFSET_PRE_TREES]
    }

    /// Version at which this node was last modified.
    #[inline]
    pub fn version(&self) -> u32 {
        u32::from_le_bytes(self.0[OFFSET_VERSION..OFFSET_VERSION + 4].try_into().unwrap())
    }

    /// Number of leaf nodes in this subtree.
    #[inline]
    pub fn size(&self) -> u32 {
        u32::from_le_bytes(self.0[OFFSET_SIZE..OFFSET_SIZE + 4].try_into().unwrap())
    }

    /// Leaf index of the smallest key in the right subtree.
    #[inline]
    pub fn key_leaf(&self) -> u32 {
        u32::from_le_bytes(self.0[OFFSET_KEY_LEAF..OFFSET_KEY_LEAF + 4].try_into().unwrap())
    }

    /// The 32-byte SHA-256 hash of this node.
    #[inline]
    pub fn hash(&self) -> &[u8] {
        &self.0[OFFSET_HASH..OFFSET_HASH + SIZE_HASH]
    }
}

// ---------------------------------------------------------------------------
// LeafLayout — zero-copy accessor for a 48-byte leaf record
// ---------------------------------------------------------------------------

/// Zero-copy view into a 48-byte leaf node record.
#[derive(Clone, Copy)]
pub struct LeafLayout<'a>(pub(crate) &'a [u8; LEAF_SIZE]);

impl<'a> LeafLayout<'a> {
    /// Version at which this leaf was last modified.
    #[inline]
    pub fn version(&self) -> u32 {
        u32::from_le_bytes(self.0[OFFSET_LEAF_VERSION..OFFSET_LEAF_VERSION + 4].try_into().unwrap())
    }

    /// Length of the key in bytes.
    #[inline]
    pub fn key_len(&self) -> u32 {
        u32::from_le_bytes(self.0[OFFSET_LEAF_KEY_LEN..OFFSET_LEAF_KEY_LEN + 4].try_into().unwrap())
    }

    /// Byte offset of the key inside the kvs buffer.
    #[inline]
    pub fn key_offset(&self) -> u64 {
        u64::from_le_bytes(
            self.0[OFFSET_LEAF_KEY_OFFSET..OFFSET_LEAF_KEY_OFFSET + 8].try_into().unwrap(),
        )
    }

    /// The 32-byte SHA-256 hash of this leaf.
    #[inline]
    pub fn hash(&self) -> &[u8] {
        &self.0[OFFSET_LEAF_HASH..OFFSET_LEAF_HASH + SIZE_HASH]
    }
}

// ---------------------------------------------------------------------------
// Nodes — array accessor over a contiguous buffer of branch records
// ---------------------------------------------------------------------------

/// A contiguous slice of serialized branch nodes.
pub struct Nodes<'a>(pub &'a [u8]);

impl<'a> Nodes<'a> {
    /// Returns the branch node at the given index. Panics if out of bounds.
    #[inline]
    pub fn get(&self, index: u32) -> NodeLayout<'a> {
        let off = index as usize * NODE_SIZE;
        NodeLayout(self.0[off..off + NODE_SIZE].try_into().unwrap())
    }

    /// Number of branch nodes in this buffer.
    #[inline]
    pub fn len(&self) -> u32 {
        (self.0.len() / NODE_SIZE) as u32
    }

    /// Returns `true` if there are no branch nodes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Leaves — array accessor over a contiguous buffer of leaf records
// ---------------------------------------------------------------------------

/// A contiguous slice of serialized leaf nodes.
pub struct Leaves<'a>(pub &'a [u8]);

impl<'a> Leaves<'a> {
    /// Returns the leaf node at the given index. Panics if out of bounds.
    #[inline]
    pub fn get(&self, index: u32) -> LeafLayout<'a> {
        let off = index as usize * LEAF_SIZE;
        LeafLayout(self.0[off..off + LEAF_SIZE].try_into().unwrap())
    }

    /// Number of leaf nodes in this buffer.
    #[inline]
    pub fn len(&self) -> u32 {
        (self.0.len() / LEAF_SIZE) as u32
    }

    /// Returns `true` if there are no leaf nodes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a 48-byte branch node buffer.
    fn make_node_buf(
        height: u8,
        pre_trees: u8,
        version: u32,
        size: u32,
        key_leaf: u32,
    ) -> [u8; NODE_SIZE] {
        let mut buf = [0u8; NODE_SIZE];
        buf[OFFSET_HEIGHT] = height;
        buf[OFFSET_PRE_TREES] = pre_trees;
        // bytes [2..4] are padding
        buf[OFFSET_VERSION..OFFSET_VERSION + 4].copy_from_slice(&version.to_le_bytes());
        buf[OFFSET_SIZE..OFFSET_SIZE + 4].copy_from_slice(&size.to_le_bytes());
        buf[OFFSET_KEY_LEAF..OFFSET_KEY_LEAF + 4].copy_from_slice(&key_leaf.to_le_bytes());
        // fill hash with a recognizable pattern
        for (i, b) in buf[OFFSET_HASH..OFFSET_HASH + SIZE_HASH].iter_mut().enumerate() {
            *b = i as u8;
        }
        buf
    }

    /// Helper: build a 48-byte leaf node buffer.
    fn make_leaf_buf(version: u32, key_len: u32, key_offset: u64) -> [u8; LEAF_SIZE] {
        let mut buf = [0u8; LEAF_SIZE];
        buf[OFFSET_LEAF_VERSION..OFFSET_LEAF_VERSION + 4].copy_from_slice(&version.to_le_bytes());
        buf[OFFSET_LEAF_KEY_LEN..OFFSET_LEAF_KEY_LEN + 4].copy_from_slice(&key_len.to_le_bytes());
        buf[OFFSET_LEAF_KEY_OFFSET..OFFSET_LEAF_KEY_OFFSET + 8]
            .copy_from_slice(&key_offset.to_le_bytes());
        for (i, b) in buf[OFFSET_LEAF_HASH..OFFSET_LEAF_HASH + SIZE_HASH].iter_mut().enumerate() {
            *b = (0xAA ^ i) as u8;
        }
        buf
    }

    #[test]
    fn test_node_layout_access() {
        let buf = make_node_buf(3, 1, 42, 100, 55);
        let node = NodeLayout(&buf);
        assert_eq!(node.height(), 3);
        assert_eq!(node.pre_trees(), 1);
        assert_eq!(node.version(), 42);
        assert_eq!(node.size(), 100);
        assert_eq!(node.key_leaf(), 55);
        assert_eq!(node.hash().len(), 32);
        assert_eq!(node.hash()[0], 0);
        assert_eq!(node.hash()[31], 31);
    }

    #[test]
    fn test_leaf_layout_access() {
        let buf = make_leaf_buf(7, 10, 0x1234_5678_9ABC_DEF0);
        let leaf = LeafLayout(&buf);
        assert_eq!(leaf.version(), 7);
        assert_eq!(leaf.key_len(), 10);
        assert_eq!(leaf.key_offset(), 0x1234_5678_9ABC_DEF0);
        assert_eq!(leaf.hash().len(), 32);
    }

    #[test]
    fn test_nodes_array_get() {
        let n0 = make_node_buf(1, 0, 10, 2, 0);
        let n1 = make_node_buf(2, 1, 20, 4, 3);
        let mut data = Vec::new();
        data.extend_from_slice(&n0);
        data.extend_from_slice(&n1);

        let nodes = Nodes(&data);
        assert_eq!(nodes.len(), 2);
        assert!(!nodes.is_empty());
        assert_eq!(nodes.get(0).height(), 1);
        assert_eq!(nodes.get(0).version(), 10);
        assert_eq!(nodes.get(1).height(), 2);
        assert_eq!(nodes.get(1).version(), 20);
    }

    #[test]
    fn test_leaves_array_get() {
        let l0 = make_leaf_buf(1, 3, 100);
        let l1 = make_leaf_buf(2, 5, 200);
        let l2 = make_leaf_buf(3, 7, 300);
        let mut data = Vec::new();
        data.extend_from_slice(&l0);
        data.extend_from_slice(&l1);
        data.extend_from_slice(&l2);

        let leaves = Leaves(&data);
        assert_eq!(leaves.len(), 3);
        assert!(!leaves.is_empty());
        assert_eq!(leaves.get(0).version(), 1);
        assert_eq!(leaves.get(1).key_len(), 5);
        assert_eq!(leaves.get(2).key_offset(), 300);
    }

    #[test]
    fn test_empty_nodes_and_leaves() {
        let nodes = Nodes(&[]);
        assert_eq!(nodes.len(), 0);
        assert!(nodes.is_empty());

        let leaves = Leaves(&[]);
        assert_eq!(leaves.len(), 0);
        assert!(leaves.is_empty());
    }

    #[test]
    #[should_panic]
    fn test_nodes_out_of_bounds() {
        let nodes = Nodes(&[]);
        let _ = nodes.get(0);
    }
}
