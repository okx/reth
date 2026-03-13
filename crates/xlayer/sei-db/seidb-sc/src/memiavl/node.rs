use crate::memiavl::{arena::NodeIdx, persisted_node::PersistedNode};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};

/// A reference-counted node pointer for O(1) CoW clone.
pub type NodeRef = Arc<Node>;

/// Enum wrapping the different node variants in the IAVL tree.
#[derive(Clone)]
pub enum Node {
    Mem(MemNode),
    Persisted(PersistedNode),
}

/// An in-memory IAVL tree node with lazy hash computation.
///
/// Leaf nodes have `height == 0`, `size == 1`, and store a key-value pair.
/// Branch nodes have `height > 0`, store only a key (the first key of the right subtree),
/// and point to left/right children.
///
/// Children are stored as `Option<NodeIdx>` (arena index) for the hot path.
/// The legacy `Option<NodeRef>` fields are kept for backward compatibility with
/// consumers (iterator, proof, snapshot_writer) that haven't been migrated yet.
///
/// The hash is computed lazily on first access via `OnceLock` and is cleared
/// whenever the node is mutated.
#[derive(Clone)]
pub struct MemNode {
    pub height: u8,
    pub version: u32,
    pub size: i64,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// Arena-based child references (hot path for set/remove/rebalance).
    pub left_idx: Option<NodeIdx>,
    pub right_idx: Option<NodeIdx>,
    /// Legacy Arc-based child references (used by iterator, proof, snapshot_writer).
    /// Populated lazily when needed by consumers that haven't migrated.
    pub left: Option<NodeRef>,
    pub right: Option<NodeRef>,
    pub(crate) hash: OnceLock<[u8; 32]>,
}

// ---------------------------------------------------------------------------
// MemNode constructors and helpers
// ---------------------------------------------------------------------------

impl MemNode {
    /// Create a new leaf node.
    pub fn new_leaf_node(key: Vec<u8>, value: Vec<u8>, version: u32) -> Self {
        Self {
            height: 0,
            version,
            size: 1,
            key,
            value,
            left_idx: None,
            right_idx: None,
            left: None,
            right: None,
            hash: OnceLock::new(),
        }
    }

    /// Create a new branch node from two children (legacy Arc-based API).
    ///
    /// Height, size, and key are derived from the children:
    /// - height = max(left.height, right.height) + 1
    /// - size   = left.size + right.size
    /// - key    = right child's leftmost key (i.e. right.key())
    pub fn new_branch_node(left: NodeRef, right: NodeRef, version: u32) -> Self {
        let height = left.height().max(right.height()) + 1;
        let size = left.size() + right.size();
        let key = right.key().to_vec();
        Self {
            height,
            version,
            size,
            key,
            value: Vec::new(),
            left_idx: None,
            right_idx: None,
            left: Some(left),
            right: Some(right),
            hash: OnceLock::new(),
        }
    }

    /// Create a new branch node from arena indices with pre-fetched metadata.
    ///
    /// This avoids dereferencing arena nodes to read height/size/key, which
    /// would require holding borrows across potential arena mutations.
    pub fn new_branch_node_idx(
        left: NodeIdx,
        right: NodeIdx,
        left_h: u8,
        right_h: u8,
        left_s: i64,
        right_s: i64,
        right_key: Vec<u8>,
        version: u32,
    ) -> Self {
        Self {
            height: left_h.max(right_h) + 1,
            version,
            size: left_s + right_s,
            key: right_key,
            value: Vec::new(),
            left_idx: Some(left),
            right_idx: Some(right),
            left: None,
            right: None,
            hash: OnceLock::new(),
        }
    }

    /// Recompute height and size from children (legacy Arc-based).
    pub fn update_height_size(&mut self) {
        let lh = self.left.as_ref().map_or(0, |n| n.height());
        let rh = self.right.as_ref().map_or(0, |n| n.height());
        self.height = lh.max(rh) + 1;

        let ls = self.left.as_ref().map_or(0, |n| n.size());
        let rs = self.right.as_ref().map_or(0, |n| n.size());
        self.size = ls + rs;
    }

    /// Recompute height and size from pre-fetched child metadata.
    ///
    /// Used by arena-based algorithms where children are accessed by index.
    pub fn update_height_size_with(&mut self, lh: u8, rh: u8, ls: i64, rs: i64) {
        self.height = lh.max(rh) + 1;
        self.size = ls + rs;
    }

    /// Calculate the balance factor: left.height - right.height (legacy Arc-based).
    pub fn calc_balance(&self) -> i8 {
        let lh = self.left.as_ref().map_or(0, |n| n.height()) as i8;
        let rh = self.right.as_ref().map_or(0, |n| n.height()) as i8;
        lh - rh
    }

    /// Calculate the balance factor from pre-fetched child heights.
    pub fn calc_balance_with(lh: u8, rh: u8) -> i8 {
        lh as i8 - rh as i8
    }
}

// ---------------------------------------------------------------------------
// Node enum — dispatch methods
// ---------------------------------------------------------------------------

impl Node {
    pub fn height(&self) -> u8 {
        match self {
            Node::Mem(m) => m.height,
            Node::Persisted(pn) => pn.height(),
        }
    }

    pub fn is_leaf(&self) -> bool {
        match self {
            Node::Mem(_) => self.height() == 0,
            Node::Persisted(pn) => pn.is_leaf(),
        }
    }

    pub fn size(&self) -> i64 {
        match self {
            Node::Mem(m) => m.size,
            Node::Persisted(pn) => pn.size(),
        }
    }

    pub fn version(&self) -> u32 {
        match self {
            Node::Mem(m) => m.version,
            Node::Persisted(pn) => pn.version(),
        }
    }

    pub fn key(&self) -> &[u8] {
        match self {
            Node::Mem(m) => &m.key,
            Node::Persisted(pn) => pn.key(),
        }
    }

    /// Returns the value for leaf nodes, empty slice for branch nodes.
    pub fn value(&self) -> &[u8] {
        match self {
            Node::Mem(m) => &m.value,
            Node::Persisted(pn) => pn.value().unwrap_or(&[]),
        }
    }

    pub fn left(&self) -> Option<&NodeRef> {
        match self {
            Node::Mem(m) => m.left.as_ref(),
            Node::Persisted(_) => None,
        }
    }

    pub fn right(&self) -> Option<&NodeRef> {
        match self {
            Node::Mem(m) => m.right.as_ref(),
            Node::Persisted(_) => None,
        }
    }

    /// Returns the SHA256 hash of this node, computing it lazily on first call.
    ///
    /// For branch nodes the hash covers: height, size, version, left_hash, right_hash.
    /// For leaf nodes the hash covers: height(0), size(1), version, key, SHA256(value).
    /// This matches the Go IAVL hash format.
    ///
    /// For `Persisted` nodes the hash is read directly from the mmap-backed snapshot.
    pub fn hash(&self) -> &[u8] {
        match self {
            Node::Mem(m) => m.hash.get_or_init(|| compute_hash(self)).as_slice(),
            Node::Persisted(pn) => pn.hash(),
        }
    }

    /// Returns a cloned copy of the hash (safe to retain independently).
    pub fn safe_hash(&self) -> Vec<u8> {
        self.hash().to_vec()
    }

    /// Look up a key in the tree rooted at this node.
    ///
    /// Returns `Some((value, index))` if found, where `index` is the in-order
    /// position of the key. Returns `None` if the key is not present.
    ///
    /// Mirrors the Go `Get(key) ([]byte, uint32)` interface, but uses `Option`
    /// to distinguish found vs not-found instead of returning a nil value.
    pub fn get(&self, key: &[u8]) -> Option<(Vec<u8>, u32)> {
        // Persisted nodes use their own efficient binary-search over the leaf array.
        if let Node::Persisted(pn) = self {
            let (val, idx) = pn.get(key);
            return val.map(|v| (v, idx));
        }

        if self.is_leaf() {
            return match self.key().cmp(key) {
                std::cmp::Ordering::Equal => Some((self.value().to_vec(), 0)),
                _ => None,
            };
        }

        if key < self.key() {
            // Search left subtree.
            if let Some(left) = self.left() {
                return left.get(key);
            }
            return None;
        }

        // Search right subtree and adjust index.
        if let Some(right) = self.right() &&
            let Some((value, idx)) = right.get(key)
        {
            let left_size = self.size() - right.size();
            return Some((value, idx + left_size as u32));
        }
        None
    }

    /// Look up a key-value pair by its in-order index.
    ///
    /// Returns `Some((key, value))` if the index is valid, `None` otherwise.
    pub fn get_by_index(&self, index: i64) -> Option<(Vec<u8>, Vec<u8>)> {
        // Persisted nodes use their own efficient index-based lookup.
        if let Node::Persisted(pn) = self {
            if index < 0 {
                return None;
            }
            return pn.get_by_index(index as u32);
        }

        if self.is_leaf() {
            if index == 0 {
                return Some((self.key().to_vec(), self.value().to_vec()));
            }
            return None;
        }

        if let Some(left) = self.left() {
            let left_size = left.size();
            if index < left_size {
                return left.get_by_index(index);
            }
            if let Some(right) = self.right() {
                return right.get_by_index(index - left_size);
            }
        }
        None
    }

    /// Convert a `NodeRef` into an owned `MemNode` for mutation (CoW).
    ///
    /// If the `Arc` has a single strong reference the inner node is unwrapped
    /// without cloning. Otherwise the node is cloned. In both cases the hash
    /// is cleared (set to a fresh `OnceCell`) and the version is updated.
    pub fn into_mem_node(node_ref: NodeRef, version: u32) -> MemNode {
        let mut mem = match Arc::try_unwrap(node_ref) {
            Ok(Node::Mem(m)) => m,
            Ok(Node::Persisted(pn)) => persisted_to_mem(&pn),
            Err(arc) => match arc.as_ref() {
                Node::Mem(m) => m.clone(),
                Node::Persisted(pn) => persisted_to_mem(pn),
            },
        };
        mem.hash = OnceLock::new();
        mem.version = version;
        mem
    }
}

// ---------------------------------------------------------------------------
// PersistedNode → MemNode conversion
// ---------------------------------------------------------------------------

/// Convert a `PersistedNode` into a `MemNode`.
///
/// Leaf nodes copy key and value from the mmap-backed snapshot.
/// Branch nodes copy key and create lazy `Node::Persisted` children so that
/// deeper subtrees are only converted on demand (CoW).
#[allow(clippy::arc_with_non_send_sync)]
pub fn persisted_to_mem(pn: &PersistedNode) -> MemNode {
    if pn.is_leaf() {
        MemNode {
            height: 0,
            version: pn.version(),
            size: 1,
            key: pn.key().to_vec(),
            value: pn.value().unwrap_or(&[]).to_vec(),
            left_idx: None,
            right_idx: None,
            left: None,
            right: None,
            hash: OnceLock::new(),
        }
    } else {
        MemNode {
            height: pn.height(),
            version: pn.version(),
            size: pn.size(),
            key: pn.key().to_vec(),
            value: Vec::new(),
            left_idx: None,
            right_idx: None,
            left: Some(Arc::new(Node::Persisted(pn.left()))),
            right: Some(Arc::new(Node::Persisted(pn.right()))),
            hash: OnceLock::new(),
        }
    }
}

/// Convert a `PersistedNode` into a `MemNode` for arena usage.
///
/// Returns the MemNode along with NodeIdx references for children that
/// need to be materialized into the arena by the caller.
/// For leaf nodes, no children are returned.
/// For branch nodes, returns (MemNode, Some(left_pn), Some(right_pn)).
pub fn persisted_to_mem_arena(
    pn: &PersistedNode,
) -> (MemNode, Option<PersistedNode>, Option<PersistedNode>) {
    if pn.is_leaf() {
        let mem = MemNode {
            height: 0,
            version: pn.version(),
            size: 1,
            key: pn.key().to_vec(),
            value: pn.value().unwrap_or(&[]).to_vec(),
            left_idx: None,
            right_idx: None,
            left: None,
            right: None,
            hash: OnceLock::new(),
        };
        (mem, None, None)
    } else {
        let left_pn = pn.left();
        let right_pn = pn.right();
        let mem = MemNode {
            height: pn.height(),
            version: pn.version(),
            size: pn.size(),
            key: pn.key().to_vec(),
            value: Vec::new(),
            left_idx: None,
            right_idx: None,
            left: None,
            right: None,
            hash: OnceLock::new(),
        };
        (mem, Some(left_pn), Some(right_pn))
    }
}

// ---------------------------------------------------------------------------
// Hash computation — matches Go IAVL SHA256 format
// ---------------------------------------------------------------------------

/// Encode a signed 64-bit integer as a protobuf-style signed varint (zigzag is NOT used;
/// Go's `binary.PutVarint` uses zigzag encoding).
///
/// Go's `binary.PutVarint` uses zigzag: encode (x << 1) ^ (x >> 63) as unsigned varint.
fn encode_varint_signed(value: i64, buf: &mut Vec<u8>) {
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    encode_varint_unsigned(zigzag, buf);
}

/// Encode an unsigned 64-bit integer as a varint (matching Go's `binary.PutUvarint`).
fn encode_varint_unsigned(mut value: u64, buf: &mut Vec<u8>) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Encode a length-prefixed byte slice (unsigned varint length + raw bytes).
/// Matches Go's `EncodeBytes`.
fn encode_bytes(bytes: &[u8], buf: &mut Vec<u8>) {
    encode_varint_unsigned(bytes.len() as u64, buf);
    buf.extend_from_slice(bytes);
}

/// Encode a signed varint into a fixed buffer. Returns bytes written.
fn encode_varint_signed_buf(value: i64, buf: &mut [u8]) -> usize {
    let zigzag = ((value << 1) ^ (value >> 63)) as u64;
    encode_varint_unsigned_buf(zigzag, buf)
}

/// Encode an unsigned varint into a fixed buffer. Returns bytes written.
fn encode_varint_unsigned_buf(mut value: u64, buf: &mut [u8]) -> usize {
    let mut i = 0;
    while value >= 0x80 {
        buf[i] = (value as u8) | 0x80;
        value >>= 7;
        i += 1;
    }
    buf[i] = value as u8;
    i + 1
}

/// Encode length-prefixed bytes into a fixed buffer. Returns bytes written.
fn encode_bytes_buf(bytes: &[u8], buf: &mut [u8]) -> usize {
    let n = encode_varint_unsigned_buf(bytes.len() as u64, buf);
    buf[n..n + bytes.len()].copy_from_slice(bytes);
    n + bytes.len()
}

/// Compute the SHA256 hash of a node matching the Go IAVL format.
///
/// Branch: `SHA256(varint(height) || varint(size) || varint(version) || encode_bytes(left_hash) ||
/// encode_bytes(right_hash))` Leaf:   `SHA256(varint(0) || varint(1) || varint(version) ||
/// encode_bytes(key) || encode_bytes(SHA256(value)))`
fn compute_hash(node: &Node) -> [u8; 32] {
    let mut data = Vec::with_capacity(128);

    encode_varint_signed(node.height() as i64, &mut data);
    encode_varint_signed(node.size(), &mut data);
    encode_varint_signed(node.version() as i64, &mut data);

    if node.is_leaf() {
        encode_bytes(node.key(), &mut data);
        let value_hash = Sha256::digest(node.value());
        encode_bytes(&value_hash, &mut data);
    } else {
        let left_hash = node.left().map(|l| l.hash()).unwrap_or(&[]);
        let right_hash = node.right().map(|r| r.hash()).unwrap_or(&[]);
        encode_bytes(left_hash, &mut data);
        encode_bytes(right_hash, &mut data);
    }

    Sha256::digest(&data).into()
}

/// Compute the hash of a MemNode using a resolver function to get children's hashes.
///
/// The resolver takes a NodeIdx and returns the 32-byte hash.
/// For leaf nodes, the resolver is not called.
pub fn compute_hash_arena<F>(node: &MemNode, resolve_hash: F) -> [u8; 32]
where
    F: Fn(NodeIdx) -> [u8; 32],
{
    // Use stack buffer to avoid heap allocation. Branch hashes need at most
    // ~76 bytes (3 varints + 2 length-prefixed 32-byte hashes). Leaf hashes
    // need 3 varints + length-prefixed key + length-prefixed 32-byte value
    // hash. For keys up to ~200 bytes, 256 is enough.
    let mut buf = [0u8; 256];
    let mut pos = 0;

    pos += encode_varint_signed_buf(node.height as i64, &mut buf[pos..]);
    pos += encode_varint_signed_buf(node.size, &mut buf[pos..]);
    pos += encode_varint_signed_buf(node.version as i64, &mut buf[pos..]);

    if node.height == 0 {
        // Leaf
        if pos + 10 + node.key.len() + 33 > 256 {
            // Key too large for stack buffer, fall back to Vec
            let mut data = Vec::with_capacity(pos + 10 + node.key.len() + 33);
            data.extend_from_slice(&buf[..pos]);
            encode_bytes(&node.key, &mut data);
            let value_hash = Sha256::digest(&node.value);
            encode_bytes(&value_hash, &mut data);
            return Sha256::digest(&data).into();
        }
        pos += encode_bytes_buf(&node.key, &mut buf[pos..]);
        let value_hash = Sha256::digest(&node.value);
        pos += encode_bytes_buf(&value_hash, &mut buf[pos..]);
    } else {
        // Branch — resolve children's hashes via arena
        let left_hash = node.left_idx.map(|idx| resolve_hash(idx));
        let right_hash = node.right_idx.map(|idx| resolve_hash(idx));
        let lh = left_hash.as_ref().map_or(&[][..], |h| h.as_slice());
        let rh = right_hash.as_ref().map_or(&[][..], |h| h.as_slice());
        pos += encode_bytes_buf(lh, &mut buf[pos..]);
        pos += encode_bytes_buf(rh, &mut buf[pos..]);
    }

    Sha256::digest(&buf[..pos]).into()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leaf_creation() {
        let leaf = MemNode::new_leaf_node(b"key1".to_vec(), b"val1".to_vec(), 1);
        assert_eq!(leaf.height, 0);
        assert_eq!(leaf.size, 1);
        assert_eq!(leaf.version, 1);
        assert_eq!(leaf.key, b"key1");
        assert_eq!(leaf.value, b"val1");
        assert!(leaf.left.is_none());
        assert!(leaf.right.is_none());
        assert!(leaf.left_idx.is_none());
        assert!(leaf.right_idx.is_none());
    }

    #[test]
    fn test_branch_creation() {
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"aaa".to_vec(), b"v1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"bbb".to_vec(), b"v2".to_vec(), 1)));
        let branch = MemNode::new_branch_node(left, right, 2);

        assert_eq!(branch.height, 1);
        assert_eq!(branch.size, 2);
        assert_eq!(branch.version, 2);
        // Key should be the right child's key (first key of right subtree).
        assert_eq!(branch.key, b"bbb");
        assert!(branch.value.is_empty());
        assert!(branch.left.is_some());
        assert!(branch.right.is_some());
    }

    #[test]
    fn test_hash_leaf() {
        let node = Node::Mem(MemNode::new_leaf_node(b"hello".to_vec(), b"world".to_vec(), 5));
        let h1 = node.hash();
        assert_eq!(h1.len(), 32); // SHA256 is 32 bytes

        // Hash must be deterministic.
        let node2 = Node::Mem(MemNode::new_leaf_node(b"hello".to_vec(), b"world".to_vec(), 5));
        assert_eq!(node.hash(), node2.hash());

        // Different value → different hash.
        let node3 = Node::Mem(MemNode::new_leaf_node(b"hello".to_vec(), b"other".to_vec(), 5));
        assert_ne!(node.hash(), node3.hash());
    }

    #[test]
    fn test_hash_branch() {
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"aaa".to_vec(), b"v1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"bbb".to_vec(), b"v2".to_vec(), 1)));
        let branch = Node::Mem(MemNode::new_branch_node(left, right, 2));
        let h = branch.hash();
        assert_eq!(h.len(), 32);

        // Recompute — must match.
        let left2 = Arc::new(Node::Mem(MemNode::new_leaf_node(b"aaa".to_vec(), b"v1".to_vec(), 1)));
        let right2 =
            Arc::new(Node::Mem(MemNode::new_leaf_node(b"bbb".to_vec(), b"v2".to_vec(), 1)));
        let branch2 = Node::Mem(MemNode::new_branch_node(left2, right2, 2));
        assert_eq!(branch.hash(), branch2.hash());
    }

    #[test]
    fn test_hash_cached() {
        let node = Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"val".to_vec(), 1));
        let h1 = node.hash();
        let h2 = node.hash();
        // Both calls return the same pointer (cached in OnceCell).
        assert!(std::ptr::eq(h1, h2));
    }

    #[test]
    fn test_into_mem_node_unique() {
        let node_ref: NodeRef =
            Arc::new(Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"val".to_vec(), 1)));
        // Force hash to be computed.
        let _ = node_ref.hash();

        // Single owner — should be unwrapped, not cloned.
        assert_eq!(Arc::strong_count(&node_ref), 1);
        let mem = Node::into_mem_node(node_ref, 5);
        assert_eq!(mem.version, 5);
        assert_eq!(mem.key, b"key");
        // Hash must be cleared after into_mem_node.
        assert!(mem.hash.get().is_none());
    }

    #[test]
    fn test_into_mem_node_shared() {
        let node_ref: NodeRef =
            Arc::new(Node::Mem(MemNode::new_leaf_node(b"key".to_vec(), b"val".to_vec(), 1)));
        // Force hash computation.
        let _ = node_ref.hash();

        // Create a second reference so refcount > 1.
        let _clone = Arc::clone(&node_ref);
        assert_eq!(Arc::strong_count(&node_ref), 2);

        let mem = Node::into_mem_node(node_ref, 10);
        assert_eq!(mem.version, 10);
        assert_eq!(mem.key, b"key");
        // Hash must be cleared after into_mem_node (even though it was cloned).
        assert!(mem.hash.get().is_none());
    }

    #[test]
    fn test_get_leaf() {
        let node = Node::Mem(MemNode::new_leaf_node(b"target".to_vec(), b"found_it".to_vec(), 1));

        // Exact match.
        let result = node.get(b"target");
        assert_eq!(result, Some((b"found_it".to_vec(), 0)));

        // Not found.
        assert!(node.get(b"other").is_none());
        assert!(node.get(b"aaa").is_none());
        assert!(node.get(b"zzz").is_none());
    }

    #[test]
    fn test_get_branch() {
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"aaa".to_vec(), b"v1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"bbb".to_vec(), b"v2".to_vec(), 1)));
        let branch = Node::Mem(MemNode::new_branch_node(Arc::clone(&left), Arc::clone(&right), 2));

        // Find left leaf.
        let result = branch.get(b"aaa");
        assert_eq!(result, Some((b"v1".to_vec(), 0)));

        // Find right leaf.
        let result = branch.get(b"bbb");
        assert_eq!(result, Some((b"v2".to_vec(), 1)));

        // Not found.
        assert!(branch.get(b"ccc").is_none());
        assert!(branch.get(b"000").is_none());
    }

    #[test]
    fn test_get_by_index() {
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"aaa".to_vec(), b"v1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"bbb".to_vec(), b"v2".to_vec(), 1)));
        let branch = Node::Mem(MemNode::new_branch_node(left, right, 2));

        assert_eq!(branch.get_by_index(0), Some((b"aaa".to_vec(), b"v1".to_vec())));
        assert_eq!(branch.get_by_index(1), Some((b"bbb".to_vec(), b"v2".to_vec())));
        assert!(branch.get_by_index(2).is_none());
        assert!(branch.get_by_index(-1).is_none());
    }

    #[test]
    fn test_node_enum_dispatch() {
        let leaf = Node::Mem(MemNode::new_leaf_node(b"k".to_vec(), b"v".to_vec(), 3));
        assert_eq!(leaf.height(), 0);
        assert!(leaf.is_leaf());
        assert_eq!(leaf.size(), 1);
        assert_eq!(leaf.version(), 3);
        assert_eq!(leaf.key(), b"k");
        assert_eq!(leaf.value(), b"v");
        assert!(leaf.left().is_none());
        assert!(leaf.right().is_none());
    }

    #[test]
    fn test_calc_balance() {
        // Balanced branch.
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"a".to_vec(), b"1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"b".to_vec(), b"2".to_vec(), 1)));
        let branch = MemNode::new_branch_node(left, right, 2);
        assert_eq!(branch.calc_balance(), 0);
    }

    #[test]
    fn test_update_height_size() {
        let left = Arc::new(Node::Mem(MemNode::new_leaf_node(b"a".to_vec(), b"1".to_vec(), 1)));
        let right = Arc::new(Node::Mem(MemNode::new_leaf_node(b"b".to_vec(), b"2".to_vec(), 1)));
        let mut branch = MemNode::new_branch_node(Arc::clone(&left), Arc::clone(&right), 2);
        // Manually corrupt height/size, then fix.
        branch.height = 0;
        branch.size = 0;
        branch.update_height_size();
        assert_eq!(branch.height, 1);
        assert_eq!(branch.size, 2);
    }

    #[test]
    fn test_safe_hash() {
        let node = Node::Mem(MemNode::new_leaf_node(b"k".to_vec(), b"v".to_vec(), 1));
        let h = node.safe_hash();
        assert_eq!(h.len(), 32);
        assert_eq!(&h, node.hash());
    }
}
