use crate::memiavl::{
    layout::{LeafLayout, Leaves, NodeLayout, Nodes, LEAF_SIZE, NODE_SIZE},
    mmap_file::MmapFile,
    persisted_node::{PersistedNode, SnapshotData},
};
use seidb_common::error::{Result, SeiDbError};
use sha2::{Digest, Sha256};
use std::{fs, path::Path, sync::Arc};

/// Magic bytes: little-endian encoding of ASCII "IAVL" (0x4C564149).
pub const SNAPSHOT_MAGIC: u32 = 0x4C56_4149;

/// The initial (and currently only) snapshot format version.
pub const SNAPSHOT_FORMAT: u32 = 0;

/// Size of the metadata file in bytes: magic(4) + format(4) + version(4).
pub const METADATA_SIZE: usize = 12;

const FILE_NAME_NODES: &str = "nodes";
const FILE_NAME_LEAVES: &str = "leaves";
const FILE_NAME_KVS: &str = "kvs";
const FILE_NAME_METADATA: &str = "metadata";

/// A read-only IAVL tree snapshot backed by 3 memory-mapped files (nodes,
/// leaves, kvs) plus a small metadata file that stores magic, format, and
/// version.
///
/// `Snapshot` does **not** store a root `PersistedNode` internally to avoid
/// circular references. Instead, callers use [`root_node`](Self::root_node)
/// to construct one on demand.
pub struct Snapshot {
    nodes_mmap: MmapFile,
    leaves_mmap: MmapFile,
    kvs_mmap: MmapFile,
    version: u32,
    node_count: u32,
    leaf_count: u32,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("version", &self.version)
            .field("node_count", &self.node_count)
            .field("leaf_count", &self.leaf_count)
            .finish()
    }
}

impl Snapshot {
    /// Opens a snapshot from the directory `dir`, which must contain files
    /// `metadata`, `nodes`, `leaves`, and `kvs`.
    ///
    /// The metadata file is read fully (12 bytes) and validated. The other
    /// three files are memory-mapped read-only with `MADV_RANDOM`.
    pub fn open(dir: &Path) -> Result<Arc<Self>> {
        // --- metadata ---
        let meta_bytes = fs::read(dir.join(FILE_NAME_METADATA))?;
        if meta_bytes.len() != METADATA_SIZE {
            return Err(SeiDbError::Other(format!(
                "wrong metadata file size, expected: {METADATA_SIZE}, found: {}",
                meta_bytes.len()
            )));
        }

        let magic = u32::from_le_bytes(
            meta_bytes[0..4]
                .try_into()
                .map_err(|_| SeiDbError::Other("invalid metadata layout (magic)".into()))?,
        );
        if magic != SNAPSHOT_MAGIC {
            return Err(SeiDbError::Other(format!("invalid metadata file magic: {magic}")));
        }
        let format = u32::from_le_bytes(
            meta_bytes[4..8]
                .try_into()
                .map_err(|_| SeiDbError::Other("invalid metadata layout (format)".into()))?,
        );
        if format != SNAPSHOT_FORMAT {
            return Err(SeiDbError::Other(format!("unknown snapshot format: {format}")));
        }
        let version = u32::from_le_bytes(
            meta_bytes[8..12]
                .try_into()
                .map_err(|_| SeiDbError::Other("invalid metadata layout (version)".into()))?,
        );

        // --- mmap files ---
        let nodes_mmap = MmapFile::open(&dir.join(FILE_NAME_NODES))?;
        let leaves_mmap = MmapFile::open(&dir.join(FILE_NAME_LEAVES))?;
        let kvs_mmap = MmapFile::open(&dir.join(FILE_NAME_KVS))?;

        // --- validate sizes ---
        if nodes_mmap.len() % NODE_SIZE != 0 {
            return Err(SeiDbError::Other(format!(
                "corrupted snapshot, nodes file size {} is not a multiple of {NODE_SIZE}",
                nodes_mmap.len()
            )));
        }
        if leaves_mmap.len() % LEAF_SIZE != 0 {
            return Err(SeiDbError::Other(format!(
                "corrupted snapshot, leaves file size {} is not a multiple of {LEAF_SIZE}",
                leaves_mmap.len()
            )));
        }

        let node_count = (nodes_mmap.len() / NODE_SIZE) as u32;
        let leaf_count = (leaves_mmap.len() / LEAF_SIZE) as u32;

        // Validate relationship: branches + 1 == leaves (or both zero)
        if (leaf_count > 0 && node_count + 1 != leaf_count) || (leaf_count == 0 && node_count != 0)
        {
            return Err(SeiDbError::Other(format!(
                "corrupted snapshot, branch nodes size {node_count} don't match leaves size {leaf_count}"
            )));
        }

        Ok(Arc::new(Self { nodes_mmap, leaves_mmap, kvs_mmap, version, node_count, leaf_count }))
    }

    /// Creates an empty snapshot (no nodes, leaves, or key-value data) with the
    /// given version number.
    pub fn new_empty(version: u32) -> Arc<Self> {
        Arc::new(Self {
            nodes_mmap: MmapFile::empty(),
            leaves_mmap: MmapFile::empty(),
            kvs_mmap: MmapFile::empty(),
            version,
            node_count: 0,
            leaf_count: 0,
        })
    }

    /// Returns the branch node layout at the given index.
    ///
    /// # Panics
    /// Panics if `index >= node_count`.
    #[inline]
    pub fn node(&self, index: u32) -> NodeLayout<'_> {
        Nodes(self.nodes_mmap.data()).get(index)
    }

    /// Returns the leaf node layout at the given index.
    ///
    /// # Panics
    /// Panics if `index >= leaf_count`.
    #[inline]
    pub fn leaf(&self, index: u32) -> LeafLayout<'_> {
        Leaves(self.leaves_mmap.data()).get(index)
    }

    /// Returns the root `PersistedNode`, or `None` for an empty snapshot.
    ///
    /// This method constructs a `SnapshotData` from the mmap buffers and wraps
    /// it in a `PersistedNode`. The `SnapshotData` is shared (via `Arc`) by all
    /// child nodes created during tree traversal.
    pub fn root_node(self: &Arc<Self>) -> Option<PersistedNode> {
        if self.is_empty() {
            return None;
        }

        let data = Arc::new(SnapshotData::new(
            self.nodes_mmap.data().to_vec(),
            self.leaves_mmap.data().to_vec(),
            self.kvs_mmap.data().to_vec(),
        ));

        if self.leaf_count == 1 && self.node_count == 0 {
            // Single leaf tree: root is the only leaf.
            Some(PersistedNode::new(data, true, 0))
        } else {
            // Root is the last branch node (post-order layout).
            Some(PersistedNode::new(data, false, self.node_count - 1))
        }
    }

    /// Returns the root hash.
    ///
    /// For an empty tree this returns the SHA-256 hash of the empty byte
    /// string (the canonical "empty IAVL hash"). For non-empty trees this
    /// delegates to the root node's stored hash.
    pub fn root_hash(self: &Arc<Self>) -> Vec<u8> {
        match self.root_node() {
            None => Sha256::digest([]).to_vec(),
            Some(node) => node.hash().to_vec(),
        }
    }

    /// Returns a zero-copy slice of the key at the given KVS `offset`.
    ///
    /// KVS layout at `offset`: `[key_len: u32 LE][key bytes]...`
    pub fn key(&self, offset: u64) -> &[u8] {
        let kvs = self.kvs_mmap.data();
        let off = offset as usize;
        let key_len = u32::from_le_bytes(kvs[off..off + 4].try_into().unwrap()) as usize;
        &kvs[off + 4..off + 4 + key_len]
    }

    /// Returns zero-copy `(key, value)` slices starting at the given KVS
    /// `offset`.
    ///
    /// KVS layout: `[key_len: u32 LE][key][value_len: u32 LE][value]`
    pub fn key_value(&self, offset: u64) -> (&[u8], &[u8]) {
        let kvs = self.kvs_mmap.data();
        let mut off = offset as usize;

        let key_len = u32::from_le_bytes(kvs[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        let key = &kvs[off..off + key_len];
        off += key_len;

        let value_len = u32::from_le_bytes(kvs[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        let value = &kvs[off..off + value_len];

        (key, value)
    }

    /// Returns the key for the leaf at the given `index`, reading directly
    /// from the mmap without constructing a `PersistedNode`.
    pub fn leaf_key(&self, index: u32) -> &[u8] {
        let leaf = self.leaf(index);
        let off = leaf.key_offset() as usize + 4; // skip the key_len prefix
        &self.kvs_mmap.data()[off..off + leaf.key_len() as usize]
    }

    /// Returns `(key, value)` for the leaf at the given `index`, reading
    /// directly from the mmap.
    pub fn leaf_key_value(&self, index: u32) -> (&[u8], &[u8]) {
        let leaf = self.leaf(index);
        let kvs = self.kvs_mmap.data();
        let mut off = leaf.key_offset() as usize + 4; // skip key_len prefix
        let key_len = leaf.key_len() as usize;
        let key = &kvs[off..off + key_len];
        off += key_len;
        let value_len = u32::from_le_bytes(kvs[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        let value = &kvs[off..off + value_len];
        (key, value)
    }

    /// Iterates all nodes in snapshot order (leaves first, then branches).
    ///
    /// The callback receives `(is_leaf, index)` and returns `true` to stop
    /// early.
    pub fn scan_nodes(&self, callback: &mut dyn FnMut(bool, u32) -> bool) {
        for i in 0..self.leaf_count {
            if callback(true, i) {
                return;
            }
        }
        for i in 0..self.node_count {
            if callback(false, i) {
                return;
            }
        }
    }

    /// Returns `true` if the snapshot contains no nodes or leaves.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.node_count == 0 && self.leaf_count == 0
    }

    /// The version stored in the snapshot metadata.
    #[inline]
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Number of leaf nodes in this snapshot.
    #[inline]
    pub fn leaf_count(&self) -> u32 {
        self.leaf_count
    }

    /// Number of branch nodes in this snapshot.
    #[inline]
    pub fn node_count(&self) -> u32 {
        self.node_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memiavl::layout::{
        LEAF_SIZE, NODE_SIZE, OFFSET_HASH, OFFSET_HEIGHT, OFFSET_KEY_LEAF, OFFSET_LEAF_HASH,
        OFFSET_LEAF_KEY_LEN, OFFSET_LEAF_KEY_OFFSET, OFFSET_LEAF_VERSION, OFFSET_PRE_TREES,
        OFFSET_SIZE, OFFSET_VERSION, SIZE_HASH,
    };
    use std::io::Write;

    // -- helpers --

    fn write_metadata(dir: &Path, version: u32) {
        let mut buf = [0u8; METADATA_SIZE];
        buf[0..4].copy_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&SNAPSHOT_FORMAT.to_le_bytes());
        buf[8..12].copy_from_slice(&version.to_le_bytes());
        let mut f = fs::File::create(dir.join("metadata")).unwrap();
        f.write_all(&buf).unwrap();
    }

    fn make_branch_buf(
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

    fn make_leaf_buf(
        version: u32,
        key_len: u32,
        key_offset: u64,
        hash: [u8; 32],
    ) -> [u8; LEAF_SIZE] {
        let mut buf = [0u8; LEAF_SIZE];
        buf[OFFSET_LEAF_VERSION..OFFSET_LEAF_VERSION + 4].copy_from_slice(&version.to_le_bytes());
        buf[OFFSET_LEAF_KEY_LEN..OFFSET_LEAF_KEY_LEN + 4].copy_from_slice(&key_len.to_le_bytes());
        buf[OFFSET_LEAF_KEY_OFFSET..OFFSET_LEAF_KEY_OFFSET + 8]
            .copy_from_slice(&key_offset.to_le_bytes());
        buf[OFFSET_LEAF_HASH..OFFSET_LEAF_HASH + SIZE_HASH].copy_from_slice(&hash);
        buf
    }

    fn make_kv_entry(key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(key);
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(value);
        buf
    }

    fn write_file(dir: &Path, name: &str, data: &[u8]) {
        let mut f = fs::File::create(dir.join(name)).unwrap();
        f.write_all(data).unwrap();
    }

    // -- tests --

    #[test]
    fn test_snapshot_new_empty() {
        let snap = Snapshot::new_empty(42);
        assert!(snap.is_empty());
        assert_eq!(snap.version(), 42);
        assert_eq!(snap.node_count(), 0);
        assert_eq!(snap.leaf_count(), 0);
        assert!(snap.root_node().is_none());
    }

    #[test]
    fn test_snapshot_open_single_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // KVS: "hello" => "world"
        let kvs = make_kv_entry(b"hello", b"world");

        // Leaf: version=1, key_len=5, key_offset=0
        let leaf = make_leaf_buf(1, 5, 0, [0xAB; 32]);
        let mut leaves_data = Vec::new();
        leaves_data.extend_from_slice(&leaf);

        write_metadata(d, 7);
        write_file(d, "nodes", &[]);
        write_file(d, "leaves", &leaves_data);
        write_file(d, "kvs", &kvs);

        let snap = Snapshot::open(d).unwrap();
        assert!(!snap.is_empty());
        assert_eq!(snap.version(), 7);
        assert_eq!(snap.node_count(), 0);
        assert_eq!(snap.leaf_count(), 1);

        // Root node should be a leaf
        let root = snap.root_node().unwrap();
        assert!(root.is_leaf());
        assert_eq!(root.key(), b"hello");
        assert_eq!(root.value(), Some(b"world".as_ref()));
        assert_eq!(root.hash(), &[0xAB; 32]);
    }

    #[test]
    fn test_snapshot_open_two_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let kvs0 = make_kv_entry(b"aa", b"v0");
        let kvs1 = make_kv_entry(b"bb", b"v1");
        let mut kvs = Vec::new();
        let off0 = 0u64;
        kvs.extend_from_slice(&kvs0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs1);

        let leaf0 = make_leaf_buf(1, 2, off0, [0x11; 32]);
        let leaf1 = make_leaf_buf(1, 2, off1, [0x22; 32]);
        let mut leaves_data = Vec::new();
        leaves_data.extend_from_slice(&leaf0);
        leaves_data.extend_from_slice(&leaf1);

        // Branch: height=1, pre_trees=0, version=1, size=2, key_leaf=1
        let branch = make_branch_buf(1, 0, 1, 2, 1, [0x33; 32]);
        let mut nodes_data = Vec::new();
        nodes_data.extend_from_slice(&branch);

        write_metadata(d, 10);
        write_file(d, "nodes", &nodes_data);
        write_file(d, "leaves", &leaves_data);
        write_file(d, "kvs", &kvs);

        let snap = Snapshot::open(d).unwrap();
        assert_eq!(snap.node_count(), 1);
        assert_eq!(snap.leaf_count(), 2);
        assert_eq!(snap.version(), 10);

        let root = snap.root_node().unwrap();
        assert!(!root.is_leaf());
        assert_eq!(root.height(), 1);

        let left = root.left();
        assert!(left.is_leaf());
        assert_eq!(left.key(), b"aa");

        let right = root.right();
        assert!(right.is_leaf());
        assert_eq!(right.key(), b"bb");
    }

    #[test]
    fn test_snapshot_root_hash_empty() {
        let snap = Snapshot::new_empty(0);
        let hash = snap.root_hash();
        // SHA-256 of empty input
        let expected = Sha256::digest([]).to_vec();
        assert_eq!(hash, expected);
    }

    #[test]
    fn test_snapshot_key_value() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let kvs0 = make_kv_entry(b"key1", b"val1");
        let kvs1 = make_kv_entry(b"key2", b"value_two");
        let mut kvs = Vec::new();
        let off0 = 0u64;
        kvs.extend_from_slice(&kvs0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs1);

        let leaf0 = make_leaf_buf(1, 4, off0, [0; 32]);
        let leaf1 = make_leaf_buf(1, 4, off1, [0; 32]);
        let mut leaves_data = Vec::new();
        leaves_data.extend_from_slice(&leaf0);
        leaves_data.extend_from_slice(&leaf1);

        let branch = make_branch_buf(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_data = Vec::new();
        nodes_data.extend_from_slice(&branch);

        write_metadata(d, 1);
        write_file(d, "nodes", &nodes_data);
        write_file(d, "leaves", &leaves_data);
        write_file(d, "kvs", &kvs);

        let snap = Snapshot::open(d).unwrap();

        // Test key() method
        assert_eq!(snap.key(off0), b"key1");
        assert_eq!(snap.key(off1), b"key2");

        // Test key_value() method
        let (k, v) = snap.key_value(off0);
        assert_eq!(k, b"key1");
        assert_eq!(v, b"val1");

        let (k, v) = snap.key_value(off1);
        assert_eq!(k, b"key2");
        assert_eq!(v, b"value_two");

        // Test leaf_key() and leaf_key_value()
        assert_eq!(snap.leaf_key(0), b"key1");
        assert_eq!(snap.leaf_key(1), b"key2");

        let (k, v) = snap.leaf_key_value(0);
        assert_eq!(k, b"key1");
        assert_eq!(v, b"val1");

        let (k, v) = snap.leaf_key_value(1);
        assert_eq!(k, b"key2");
        assert_eq!(v, b"value_two");
    }

    #[test]
    fn test_snapshot_is_empty() {
        let snap = Snapshot::new_empty(0);
        assert!(snap.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let kvs = make_kv_entry(b"k", b"v");
        let leaf = make_leaf_buf(1, 1, 0, [0; 32]);
        let mut leaves_data = Vec::new();
        leaves_data.extend_from_slice(&leaf);

        write_metadata(d, 1);
        write_file(d, "nodes", &[]);
        write_file(d, "leaves", &leaves_data);
        write_file(d, "kvs", &kvs);

        let snap = Snapshot::open(d).unwrap();
        assert!(!snap.is_empty());
    }

    #[test]
    fn test_snapshot_scan_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let kvs0 = make_kv_entry(b"aa", b"v0");
        let kvs1 = make_kv_entry(b"bb", b"v1");
        let mut kvs = Vec::new();
        kvs.extend_from_slice(&kvs0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs1);

        let leaf0 = make_leaf_buf(1, 2, 0, [0; 32]);
        let leaf1 = make_leaf_buf(1, 2, off1, [0; 32]);
        let mut leaves_data = Vec::new();
        leaves_data.extend_from_slice(&leaf0);
        leaves_data.extend_from_slice(&leaf1);

        let branch = make_branch_buf(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_data = Vec::new();
        nodes_data.extend_from_slice(&branch);

        write_metadata(d, 1);
        write_file(d, "nodes", &nodes_data);
        write_file(d, "leaves", &leaves_data);
        write_file(d, "kvs", &kvs);

        let snap = Snapshot::open(d).unwrap();
        let mut visited = Vec::new();
        snap.scan_nodes(&mut |is_leaf, index| {
            visited.push((is_leaf, index));
            false
        });

        // Leaves first (indices 0, 1), then branches (index 0)
        assert_eq!(visited, vec![(true, 0), (true, 1), (false, 0)]);
    }

    #[test]
    fn test_snapshot_scan_nodes_early_stop() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let kvs0 = make_kv_entry(b"aa", b"v0");
        let kvs1 = make_kv_entry(b"bb", b"v1");
        let mut kvs = Vec::new();
        kvs.extend_from_slice(&kvs0);
        let off1 = kvs.len() as u64;
        kvs.extend_from_slice(&kvs1);

        let leaf0 = make_leaf_buf(1, 2, 0, [0; 32]);
        let leaf1 = make_leaf_buf(1, 2, off1, [0; 32]);
        let mut leaves_data = Vec::new();
        leaves_data.extend_from_slice(&leaf0);
        leaves_data.extend_from_slice(&leaf1);

        let branch = make_branch_buf(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_data = Vec::new();
        nodes_data.extend_from_slice(&branch);

        write_metadata(d, 1);
        write_file(d, "nodes", &nodes_data);
        write_file(d, "leaves", &leaves_data);
        write_file(d, "kvs", &kvs);

        let snap = Snapshot::open(d).unwrap();
        let mut count = 0;
        snap.scan_nodes(&mut |_, _| {
            count += 1;
            true // stop immediately
        });
        assert_eq!(count, 1);
    }

    #[test]
    fn test_snapshot_open_bad_magic() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mut buf = [0u8; METADATA_SIZE];
        buf[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        let mut f = fs::File::create(d.join("metadata")).unwrap();
        f.write_all(&buf).unwrap();

        write_file(d, "nodes", &[]);
        write_file(d, "leaves", &[]);
        write_file(d, "kvs", &[]);

        let err = Snapshot::open(d).unwrap_err();
        assert!(err.to_string().contains("invalid metadata file magic"));
    }

    #[test]
    fn test_snapshot_open_bad_format() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mut buf = [0u8; METADATA_SIZE];
        buf[0..4].copy_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        buf[4..8].copy_from_slice(&99u32.to_le_bytes());
        let mut f = fs::File::create(d.join("metadata")).unwrap();
        f.write_all(&buf).unwrap();

        write_file(d, "nodes", &[]);
        write_file(d, "leaves", &[]);
        write_file(d, "kvs", &[]);

        let err = Snapshot::open(d).unwrap_err();
        assert!(err.to_string().contains("unknown snapshot format"));
    }

    #[test]
    fn test_snapshot_open_wrong_metadata_size() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_file(d, "metadata", &[0u8; 8]); // too short
        write_file(d, "nodes", &[]);
        write_file(d, "leaves", &[]);
        write_file(d, "kvs", &[]);

        let err = Snapshot::open(d).unwrap_err();
        assert!(err.to_string().contains("wrong metadata file size"));
    }

    #[test]
    fn test_snapshot_open_mismatched_node_leaf_counts() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // 1 branch node but 0 leaves → invalid
        let branch = make_branch_buf(1, 0, 1, 2, 1, [0; 32]);
        let mut nodes_data = Vec::new();
        nodes_data.extend_from_slice(&branch);

        write_metadata(d, 1);
        write_file(d, "nodes", &nodes_data);
        write_file(d, "leaves", &[]);
        write_file(d, "kvs", &[]);

        let err = Snapshot::open(d).unwrap_err();
        assert!(err.to_string().contains("don't match"));
    }
}
