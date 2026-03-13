//! Snapshot write pipeline — post-order DFS traversal writing nodes/leaves/kvs to files.
//!
//! Uses a 3-thread pipeline (matching Go's goroutine-based design):
//!   - Main thread does post-order DFS, sending operations to channels
//!   - kvWriter thread: writes key-value data to kvs file
//!   - leafWriter thread: writes leaf records to leaves file
//!   - branchWriter thread: writes branch records to nodes file
//!
//! File format (4 files in `dir`):
//!   - `metadata`: 12 bytes — `magic(u32 LE) + format(u32 LE) + version(u32 LE)`
//!   - `nodes`:    array of 48-byte branch-node records
//!   - `leaves`:   array of 48-byte leaf-node records
//!   - `kvs`:      blob of `keyLen(u32 LE) + key + valueLen(u32 LE) + value` entries

use crate::memiavl::{
    arena::{resolve_mem_node, FrozenArena, MutableArena, NodeIdx},
    layout::{
        OFFSET_HASH, OFFSET_HEIGHT, OFFSET_KEY_LEAF, OFFSET_LEAF_HASH, OFFSET_LEAF_KEY_LEN,
        OFFSET_LEAF_KEY_OFFSET, OFFSET_LEAF_VERSION, OFFSET_PRE_TREES, OFFSET_SIZE, OFFSET_VERSION,
        SIZE_HASH,
    },
    node::Node,
    rate_limiter::{MonitoringWriter, RateLimitedWriter, RateLimiter},
    snapshot::{Snapshot, METADATA_SIZE, SNAPSHOT_FORMAT, SNAPSHOT_MAGIC},
    tree_algo::compute_hash_recursive,
};
use crossbeam_channel::{bounded, Sender};
use seidb_common::error::{Result, SeiDbError};
use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
    thread,
};

/// Size of the branch-node record before the hash (height + preTrees + pad + version + size +
/// keyLeaf).
const SIZE_NODE_WITHOUT_HASH: usize = OFFSET_HASH; // 16

/// Size of the leaf-node record before the hash (version + keyLen + keyOffset).
const SIZE_LEAF_WITHOUT_HASH: usize = OFFSET_LEAF_HASH; // 16

/// Default channel buffer size for the pipeline.
/// Large enough to keep writer threads busy without excessive memory usage.
const PIPELINE_CHANNEL_SIZE: usize = 500_000;

/// Operation sent to the KV writer thread.
struct KvOp {
    key: Vec<u8>,
    value: Vec<u8>,
}

/// Operation sent to the leaf writer thread.
struct LeafOp {
    version: u32,
    key_len: u32,
    key_offset: u64,
    hash: [u8; 32],
}

/// Operation sent to the branch writer thread.
struct BranchOp {
    height: u8,
    pre_trees: u8,
    version: u32,
    size: u32,
    key_leaf: u32,
    hash: [u8; 32],
}

/// Write an IAVL tree snapshot to `dir` (no rate limiting).
///
/// Convenience wrapper around [`write_snapshot_with_limiter`] with no rate
/// limiting.
pub fn write_snapshot(dir: &Path, version: u32, root: Option<&Node>) -> Result<()> {
    write_snapshot_with_limiter(dir, version, root, None)
}

/// Write an IAVL tree snapshot to `dir`, optionally rate-limiting writes.
///
/// The tree is traversed in post-order DFS (left, right, current). Each leaf
/// is assigned a monotonically increasing *leaf index* and each branch a
/// monotonically increasing *branch index*, both starting at 0.
///
/// Internally uses a 3-thread pipeline: the main thread traverses and sends
/// write operations to kv/leaf/branch writer threads via bounded channels.
///
/// When a [`RateLimiter`] is provided, each writer thread wraps its buffered
/// writer with [`RateLimitedWriter`] and [`MonitoringWriter`] so that the
/// aggregate disk write throughput is bounded, preventing page cache eviction.
///
/// If `root` is `None` the snapshot is written as an empty tree (only the
/// metadata file has content; the other three files are empty).
pub fn write_snapshot_with_limiter(
    dir: &Path,
    version: u32,
    root: Option<&Node>,
    limiter: Option<&RateLimiter>,
) -> Result<()> {
    fs::create_dir_all(dir)?;

    let fp_nodes = File::create(dir.join("nodes"))?;
    let fp_leaves = File::create(dir.join("leaves"))?;
    let fp_kvs = File::create(dir.join("kvs"))?;

    if let Some(node) = root {
        // Set up the 3-thread pipeline
        let (kv_tx, kv_rx) = bounded::<KvOp>(PIPELINE_CHANNEL_SIZE);
        let (leaf_tx, leaf_rx) = bounded::<LeafOp>(PIPELINE_CHANNEL_SIZE);
        let (branch_tx, branch_rx) = bounded::<BranchOp>(PIPELINE_CHANNEL_SIZE);

        // Clone the limiter for each writer thread (Arc-backed, cheap clone).
        let kv_limiter = limiter.cloned();
        let leaf_limiter = limiter.cloned();
        let branch_limiter = limiter.cloned();

        // Spawn KV writer thread
        let kv_handle = thread::spawn(move || -> Result<()> {
            let mut w = wrap_writer(BufWriter::new(fp_kvs), "kvs", kv_limiter.as_ref());
            for op in kv_rx {
                write_kv_entry(&mut w, &op.key, &op.value)?;
            }
            w.flush()?;
            Ok(())
        });

        // Spawn leaf writer thread
        let leaf_handle = thread::spawn(move || -> Result<()> {
            let mut w = wrap_writer(BufWriter::new(fp_leaves), "leaves", leaf_limiter.as_ref());
            for op in leaf_rx {
                write_leaf_record(&mut w, op.version, op.key_len, op.key_offset, &op.hash)?;
            }
            w.flush()?;
            Ok(())
        });

        // Spawn branch writer thread
        let branch_handle = thread::spawn(move || -> Result<()> {
            let mut w = wrap_writer(BufWriter::new(fp_nodes), "nodes", branch_limiter.as_ref());
            for op in branch_rx {
                write_branch_record(
                    &mut w,
                    op.height,
                    op.pre_trees,
                    op.version,
                    op.size,
                    op.key_leaf,
                    &op.hash,
                )?;
            }
            w.flush()?;
            Ok(())
        });

        // Main thread: DFS traversal sending ops to channels
        let mut branch_count: u32 = 0;
        let mut leaf_count: u32 = 0;
        let mut kvs_offset: u64 = 0;

        let traverse_result = write_recursive_pipeline(
            node,
            &kv_tx,
            &leaf_tx,
            &branch_tx,
            &mut branch_count,
            &mut leaf_count,
            &mut kvs_offset,
        );

        // Drop senders to signal channel closure so writer threads exit
        drop(kv_tx);
        drop(leaf_tx);
        drop(branch_tx);

        // Check traversal result first
        traverse_result?;

        // Join all writer threads and propagate errors
        kv_handle
            .join()
            .map_err(|_| SeiDbError::Other("kv writer thread panicked".to_string()))??;
        leaf_handle
            .join()
            .map_err(|_| SeiDbError::Other("leaf writer thread panicked".to_string()))??;
        branch_handle
            .join()
            .map_err(|_| SeiDbError::Other("branch writer thread panicked".to_string()))??;
    } else {
        // Empty tree: just create empty files (they already exist from File::create)
        drop(fp_nodes);
        drop(fp_leaves);
        drop(fp_kvs);
    }

    // Write metadata last so that a partial write is detectable.
    let mut meta_buf = [0u8; METADATA_SIZE];
    meta_buf[0..4].copy_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
    meta_buf[4..8].copy_from_slice(&SNAPSHOT_FORMAT.to_le_bytes());
    meta_buf[8..12].copy_from_slice(&version.to_le_bytes());

    let mut fp_meta = File::create(dir.join("metadata"))?;
    fp_meta.write_all(&meta_buf)?;
    fp_meta.flush()?;

    Ok(())
}

/// Wrap a writer with optional rate limiting and monitoring.
///
/// When a limiter is provided, the writer stack is:
///   `MonitoringWriter -> RateLimitedWriter -> BufWriter -> File`
/// When no limiter is provided, the writer is returned as-is (boxed).
fn wrap_writer<W: Write + Send + 'static>(
    writer: W,
    name: &str,
    limiter: Option<&RateLimiter>,
) -> Box<dyn Write + Send> {
    match limiter {
        Some(l) => {
            let rate_limited = RateLimitedWriter::new(writer, l.clone());
            Box::new(MonitoringWriter::new(rate_limited, name))
        }
        None => Box::new(writer),
    }
}

/// Recursively traverse the tree in post-order, sending leaf, branch, and
/// key-value operations to their respective channel senders.
///
/// Returns `(is_leaf, index)` so the parent can record child references.
fn write_recursive_pipeline(
    node: &Node,
    kv_tx: &Sender<KvOp>,
    leaf_tx: &Sender<LeafOp>,
    branch_tx: &Sender<BranchOp>,
    branch_count: &mut u32,
    leaf_count: &mut u32,
    kvs_offset: &mut u64,
) -> Result<(bool, u32)> {
    if node.is_leaf() {
        let key = node.key();
        let value = node.value();
        let key_len = key.len() as u32;

        // Record the current kvs offset for this leaf.
        let this_offset = *kvs_offset;
        *kvs_offset += 4 + key.len() as u64 + 4 + value.len() as u64;

        // Send KV write op
        kv_tx
            .send(KvOp { key: key.to_vec(), value: value.to_vec() })
            .map_err(|_| SeiDbError::Other("kv channel closed unexpectedly".to_string()))?;

        // Send leaf write op
        let hash = node.hash();
        if hash.len() != SIZE_HASH {
            return Err(SeiDbError::Other(format!(
                "expected {SIZE_HASH}-byte hash, got {}",
                hash.len()
            )));
        }
        let mut hash_arr = [0u8; 32];
        hash_arr.copy_from_slice(hash);

        leaf_tx
            .send(LeafOp {
                version: node.version(),
                key_len,
                key_offset: this_offset,
                hash: hash_arr,
            })
            .map_err(|_| SeiDbError::Other("leaf channel closed unexpectedly".to_string()))?;

        let idx = *leaf_count;
        *leaf_count += 1;
        return Ok((true, idx));
    }

    // Sanity check matching Go: leafCounter >= branchCounter
    if *leaf_count < *branch_count {
        return Err(SeiDbError::Other(format!(
            "leafCounter {} < branchCounter {}",
            leaf_count, branch_count
        )));
    }

    let pre_trees = (*leaf_count - *branch_count) as u8;

    // Recurse left
    let left = node
        .left()
        .ok_or_else(|| SeiDbError::Other("branch node missing left child".to_string()))?;
    write_recursive_pipeline(
        left,
        kv_tx,
        leaf_tx,
        branch_tx,
        branch_count,
        leaf_count,
        kvs_offset,
    )?;

    // The first leaf of the right subtree — this is the keyLeaf for the branch.
    let key_leaf = *leaf_count;

    // Recurse right
    let right = node
        .right()
        .ok_or_else(|| SeiDbError::Other("branch node missing right child".to_string()))?;
    write_recursive_pipeline(
        right,
        kv_tx,
        leaf_tx,
        branch_tx,
        branch_count,
        leaf_count,
        kvs_offset,
    )?;

    // Send branch record
    let size = node.size();
    if size < 0 || size > u32::MAX as i64 {
        return Err(SeiDbError::Other(format!("node size {} out of range", size)));
    }

    let hash = node.hash();
    if hash.len() != SIZE_HASH {
        return Err(SeiDbError::Other(format!(
            "expected {SIZE_HASH}-byte hash, got {}",
            hash.len()
        )));
    }
    let mut hash_arr = [0u8; 32];
    hash_arr.copy_from_slice(hash);

    branch_tx
        .send(BranchOp {
            height: node.height(),
            pre_trees,
            version: node.version(),
            size: size as u32,
            key_leaf,
            hash: hash_arr,
        })
        .map_err(|_| SeiDbError::Other("branch channel closed unexpectedly".to_string()))?;

    let idx = *branch_count;
    *branch_count += 1;
    Ok((false, idx))
}

// ---------------------------------------------------------------------------
// Arena-based snapshot writing (eliminates ensure_root_ref / idx_to_node_ref)
// ---------------------------------------------------------------------------

/// Write an IAVL tree snapshot from arena-based storage, without building Arc<Node> tree.
pub fn write_snapshot_arena(
    dir: &Path,
    version: u32,
    root: Option<NodeIdx>,
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
) -> Result<()> {
    write_snapshot_arena_with_limiter(
        dir,
        version,
        root,
        arena,
        frozen,
        snapshot,
        current_gen,
        None,
    )
}

/// Write an IAVL tree snapshot from arena-based storage with optional rate limiting.
pub fn write_snapshot_arena_with_limiter(
    dir: &Path,
    version: u32,
    root: Option<NodeIdx>,
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
    limiter: Option<&RateLimiter>,
) -> Result<()> {
    fs::create_dir_all(dir)?;

    let fp_nodes = File::create(dir.join("nodes"))?;
    let fp_leaves = File::create(dir.join("leaves"))?;
    let fp_kvs = File::create(dir.join("kvs"))?;

    if let Some(root_idx) = root {
        let (kv_tx, kv_rx) = bounded::<KvOp>(PIPELINE_CHANNEL_SIZE);
        let (leaf_tx, leaf_rx) = bounded::<LeafOp>(PIPELINE_CHANNEL_SIZE);
        let (branch_tx, branch_rx) = bounded::<BranchOp>(PIPELINE_CHANNEL_SIZE);

        let kv_limiter = limiter.cloned();
        let leaf_limiter = limiter.cloned();
        let branch_limiter = limiter.cloned();

        let kv_handle = thread::spawn(move || -> Result<()> {
            let mut w = wrap_writer(BufWriter::new(fp_kvs), "kvs", kv_limiter.as_ref());
            for op in kv_rx {
                write_kv_entry(&mut w, &op.key, &op.value)?;
            }
            w.flush()?;
            Ok(())
        });

        let leaf_handle = thread::spawn(move || -> Result<()> {
            let mut w = wrap_writer(BufWriter::new(fp_leaves), "leaves", leaf_limiter.as_ref());
            for op in leaf_rx {
                write_leaf_record(&mut w, op.version, op.key_len, op.key_offset, &op.hash)?;
            }
            w.flush()?;
            Ok(())
        });

        let branch_handle = thread::spawn(move || -> Result<()> {
            let mut w = wrap_writer(BufWriter::new(fp_nodes), "nodes", branch_limiter.as_ref());
            for op in branch_rx {
                write_branch_record(
                    &mut w,
                    op.height,
                    op.pre_trees,
                    op.version,
                    op.size,
                    op.key_leaf,
                    &op.hash,
                )?;
            }
            w.flush()?;
            Ok(())
        });

        let mut branch_count: u32 = 0;
        let mut leaf_count: u32 = 0;
        let mut kvs_offset: u64 = 0;

        let traverse_result = write_recursive_arena(
            root_idx,
            arena,
            frozen,
            snapshot,
            current_gen,
            &kv_tx,
            &leaf_tx,
            &branch_tx,
            &mut branch_count,
            &mut leaf_count,
            &mut kvs_offset,
        );

        drop(kv_tx);
        drop(leaf_tx);
        drop(branch_tx);

        traverse_result?;

        kv_handle
            .join()
            .map_err(|_| SeiDbError::Other("kv writer thread panicked".to_string()))??;
        leaf_handle
            .join()
            .map_err(|_| SeiDbError::Other("leaf writer thread panicked".to_string()))??;
        branch_handle
            .join()
            .map_err(|_| SeiDbError::Other("branch writer thread panicked".to_string()))??;
    } else {
        drop(fp_nodes);
        drop(fp_leaves);
        drop(fp_kvs);
    }

    let mut meta_buf = [0u8; METADATA_SIZE];
    meta_buf[0..4].copy_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
    meta_buf[4..8].copy_from_slice(&SNAPSHOT_FORMAT.to_le_bytes());
    meta_buf[8..12].copy_from_slice(&version.to_le_bytes());

    let mut fp_meta = File::create(dir.join("metadata"))?;
    fp_meta.write_all(&meta_buf)?;
    fp_meta.flush()?;

    Ok(())
}

/// Arena-based post-order DFS traversal for snapshot writing.
fn write_recursive_arena(
    idx: NodeIdx,
    arena: &MutableArena,
    frozen: &[std::sync::Arc<FrozenArena>],
    snapshot: &Option<std::sync::Arc<Snapshot>>,
    current_gen: u16,
    kv_tx: &Sender<KvOp>,
    leaf_tx: &Sender<LeafOp>,
    branch_tx: &Sender<BranchOp>,
    branch_count: &mut u32,
    leaf_count: &mut u32,
    kvs_offset: &mut u64,
) -> Result<(bool, u32)> {
    // Handle persisted nodes via PersistedNode API
    if idx.is_persisted() {
        let snap = snapshot
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("snapshot required for persisted node".into()))?;
        let pn = snap.node_at(idx.persisted_index(), idx.persisted_is_leaf());
        // Delegate to the existing Node-based writer via PersistedNode → Node conversion
        let node = Node::Persisted(pn);
        return write_recursive_pipeline(
            &node,
            kv_tx,
            leaf_tx,
            branch_tx,
            branch_count,
            leaf_count,
            kvs_offset,
        );
    }

    let n = resolve_mem_node(arena, frozen, current_gen, idx);

    if n.height == 0 {
        // Leaf
        let this_offset = *kvs_offset;
        *kvs_offset += 4 + n.key.len() as u64 + 4 + n.value.len() as u64;

        kv_tx
            .send(KvOp { key: n.key.clone(), value: n.value.clone() })
            .map_err(|_| SeiDbError::Other("kv channel closed unexpectedly".to_string()))?;

        let hash = compute_hash_recursive(arena, frozen, snapshot, current_gen, idx);

        leaf_tx
            .send(LeafOp {
                version: n.version,
                key_len: n.key.len() as u32,
                key_offset: this_offset,
                hash,
            })
            .map_err(|_| SeiDbError::Other("leaf channel closed unexpectedly".to_string()))?;

        let i = *leaf_count;
        *leaf_count += 1;
        return Ok((true, i));
    }

    // Branch
    if *leaf_count < *branch_count {
        return Err(SeiDbError::Other(format!(
            "leafCounter {} < branchCounter {}",
            leaf_count, branch_count
        )));
    }

    let pre_trees = (*leaf_count - *branch_count) as u8;

    // Recurse left
    let left_idx = n
        .left_idx
        .ok_or_else(|| SeiDbError::Other("branch node missing left child".to_string()))?;
    write_recursive_arena(
        left_idx,
        arena,
        frozen,
        snapshot,
        current_gen,
        kv_tx,
        leaf_tx,
        branch_tx,
        branch_count,
        leaf_count,
        kvs_offset,
    )?;

    let key_leaf = *leaf_count;

    // Recurse right
    let right_idx = n
        .right_idx
        .ok_or_else(|| SeiDbError::Other("branch node missing right child".to_string()))?;
    write_recursive_arena(
        right_idx,
        arena,
        frozen,
        snapshot,
        current_gen,
        kv_tx,
        leaf_tx,
        branch_tx,
        branch_count,
        leaf_count,
        kvs_offset,
    )?;

    // Re-read node (arena didn't change, but need fresh reference)
    let n = resolve_mem_node(arena, frozen, current_gen, idx);
    let hash = compute_hash_recursive(arena, frozen, snapshot, current_gen, idx);

    branch_tx
        .send(BranchOp {
            height: n.height,
            pre_trees,
            version: n.version,
            size: n.size as u32,
            key_leaf,
            hash,
        })
        .map_err(|_| SeiDbError::Other("branch channel closed unexpectedly".to_string()))?;

    let i = *branch_count;
    *branch_count += 1;
    Ok((false, i))
}

/// Write a key-value entry to the kvs file: keyLen(4 LE) + key + valueLen(4 LE) + value.
fn write_kv_entry<W: Write>(w: &mut W, key: &[u8], value: &[u8]) -> Result<()> {
    let key_len = key.len() as u32;
    let value_len = value.len() as u32;

    w.write_all(&key_len.to_le_bytes())?;
    w.write_all(key)?;
    w.write_all(&value_len.to_le_bytes())?;
    w.write_all(value)?;

    Ok(())
}

/// Write a single leaf record: version(4) + keyLen(4) + keyOffset(8) + hash(32).
fn write_leaf_record<W: Write>(
    w: &mut W,
    version: u32,
    key_len: u32,
    key_offset: u64,
    hash: &[u8],
) -> Result<()> {
    let mut buf = [0u8; SIZE_LEAF_WITHOUT_HASH];
    buf[OFFSET_LEAF_VERSION..OFFSET_LEAF_VERSION + 4].copy_from_slice(&version.to_le_bytes());
    buf[OFFSET_LEAF_KEY_LEN..OFFSET_LEAF_KEY_LEN + 4].copy_from_slice(&key_len.to_le_bytes());
    buf[OFFSET_LEAF_KEY_OFFSET..OFFSET_LEAF_KEY_OFFSET + 8]
        .copy_from_slice(&key_offset.to_le_bytes());

    w.write_all(&buf)?;

    if hash.len() != SIZE_HASH {
        return Err(SeiDbError::Other(format!(
            "expected {SIZE_HASH}-byte hash, got {}",
            hash.len()
        )));
    }
    w.write_all(hash)?;
    Ok(())
}

/// Write a single branch-node record to the nodes file.
fn write_branch_record<W: Write>(
    w: &mut W,
    height: u8,
    pre_trees: u8,
    version: u32,
    size: u32,
    key_leaf: u32,
    hash: &[u8],
) -> Result<()> {
    let mut buf = [0u8; SIZE_NODE_WITHOUT_HASH];
    buf[OFFSET_HEIGHT] = height;
    buf[OFFSET_PRE_TREES] = pre_trees;
    // bytes [2..4] are padding (zeroed)
    buf[OFFSET_VERSION..OFFSET_VERSION + 4].copy_from_slice(&version.to_le_bytes());
    buf[OFFSET_SIZE..OFFSET_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    buf[OFFSET_KEY_LEAF..OFFSET_KEY_LEAF + 4].copy_from_slice(&key_leaf.to_le_bytes());

    w.write_all(&buf)?;

    if hash.len() != SIZE_HASH {
        return Err(SeiDbError::Other(format!(
            "expected {SIZE_HASH}-byte hash, got {}",
            hash.len()
        )));
    }
    w.write_all(hash)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memiavl::{
        layout::{LEAF_SIZE, NODE_SIZE},
        node::{MemNode, NodeRef},
        snapshot::Snapshot,
    };
    use std::sync::Arc;

    /// Build a leaf `Node` wrapped in `Arc`.
    fn leaf(key: &[u8], value: &[u8], version: u32) -> NodeRef {
        Arc::new(Node::Mem(MemNode::new_leaf_node(key.to_vec(), value.to_vec(), version)))
    }

    /// Build a branch `Node` from two children.
    fn branch(left: NodeRef, right: NodeRef, version: u32) -> NodeRef {
        Arc::new(Node::Mem(MemNode::new_branch_node(left, right, version)))
    }

    // -- tests --

    #[test]
    fn test_write_snapshot_empty() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        write_snapshot(d, 42, None).unwrap();

        // Metadata should exist and be valid.
        let snap = Snapshot::open(d).unwrap();
        assert!(snap.is_empty());
        assert_eq!(snap.version(), 42);
        assert_eq!(snap.node_count(), 0);
        assert_eq!(snap.leaf_count(), 0);

        // Data files should be empty.
        assert_eq!(fs::metadata(d.join("nodes")).unwrap().len(), 0);
        assert_eq!(fs::metadata(d.join("leaves")).unwrap().len(), 0);
        assert_eq!(fs::metadata(d.join("kvs")).unwrap().len(), 0);
    }

    #[test]
    fn test_write_snapshot_single_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let root = Node::Mem(MemNode::new_leaf_node(b"hello".to_vec(), b"world".to_vec(), 7));

        write_snapshot(d, 7, Some(&root)).unwrap();

        // Verify file sizes.
        assert_eq!(fs::metadata(d.join("nodes")).unwrap().len(), 0);
        assert_eq!(fs::metadata(d.join("leaves")).unwrap().len(), LEAF_SIZE as u64);
        // kvs = 4 + 5 + 4 + 5 = 18
        assert_eq!(fs::metadata(d.join("kvs")).unwrap().len(), 18);

        // Re-open and verify content.
        let snap = Snapshot::open(d).unwrap();
        assert_eq!(snap.version(), 7);
        assert_eq!(snap.leaf_count(), 1);
        assert_eq!(snap.node_count(), 0);

        let root_node = snap.root_node().unwrap();
        assert!(root_node.is_leaf());
        assert_eq!(root_node.key(), b"hello");
        assert_eq!(root_node.value(), Some(b"world".as_ref()));
        // Hash must match what the in-memory node computes.
        assert_eq!(root_node.hash(), root.hash());
    }

    #[test]
    fn test_write_snapshot_small_tree() {
        // Build a 3-leaf tree:
        //        branch(v=3)
        //       /            \
        //   branch(v=2)    leaf_c(v=1)
        //   /       \
        // leaf_a    leaf_b
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let leaf_a = leaf(b"aaa", b"v_a", 1);
        let leaf_b = leaf(b"bbb", b"v_b", 1);
        let leaf_c = leaf(b"ccc", b"v_c", 1);

        let inner = branch(leaf_a, leaf_b.clone(), 2);
        let root_ref = branch(inner, leaf_c, 3);

        write_snapshot(d, 10, Some(&root_ref)).unwrap();

        // 3 leaves, 2 branches
        assert_eq!(fs::metadata(d.join("leaves")).unwrap().len(), 3 * LEAF_SIZE as u64);
        assert_eq!(fs::metadata(d.join("nodes")).unwrap().len(), 2 * NODE_SIZE as u64);

        let snap = Snapshot::open(d).unwrap();
        assert_eq!(snap.version(), 10);
        assert_eq!(snap.leaf_count(), 3);
        assert_eq!(snap.node_count(), 2);

        // Verify all leaf keys via leaf_key().
        assert_eq!(snap.leaf_key(0), b"aaa");
        assert_eq!(snap.leaf_key(1), b"bbb");
        assert_eq!(snap.leaf_key(2), b"ccc");

        // Verify leaf values.
        let (k, v) = snap.leaf_key_value(0);
        assert_eq!(k, b"aaa");
        assert_eq!(v, b"v_a");
        let (k, v) = snap.leaf_key_value(1);
        assert_eq!(k, b"bbb");
        assert_eq!(v, b"v_b");
        let (k, v) = snap.leaf_key_value(2);
        assert_eq!(k, b"ccc");
        assert_eq!(v, b"v_c");
    }

    #[test]
    fn test_write_snapshot_roundtrip() {
        // Build a balanced 4-leaf tree and verify that writing then reading
        // back produces the same root hash and all key-value pairs.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let l0 = leaf(b"key0", b"val0", 1);
        let l1 = leaf(b"key1", b"val1", 1);
        let l2 = leaf(b"key2", b"val2", 1);
        let l3 = leaf(b"key3", b"val3", 1);

        let left = branch(l0, l1, 2);
        let right = branch(l2, l3, 2);
        let root_ref = branch(left, right, 3);

        let expected_hash = root_ref.hash().to_vec();

        write_snapshot(d, 99, Some(&root_ref)).unwrap();

        let snap = Snapshot::open(d).unwrap();
        assert_eq!(snap.version(), 99);
        assert_eq!(snap.leaf_count(), 4);
        assert_eq!(snap.node_count(), 3);

        // Root hash must match.
        assert_eq!(snap.root_hash(), expected_hash);

        // Verify each leaf key-value.
        for i in 0..4 {
            let expected_key = format!("key{i}");
            let expected_val = format!("val{i}");
            let (k, v) = snap.leaf_key_value(i);
            assert_eq!(k, expected_key.as_bytes());
            assert_eq!(v, expected_val.as_bytes());
        }

        // Verify root node structure via PersistedNode.
        let root_node = snap.root_node().unwrap();
        assert!(!root_node.is_leaf());
        assert_eq!(root_node.height(), 2);
        assert_eq!(root_node.size(), 4);
        assert_eq!(root_node.hash(), expected_hash.as_slice());

        // Verify left subtree.
        let left_node = root_node.left();
        assert!(!left_node.is_leaf());
        assert_eq!(left_node.height(), 1);
        assert_eq!(left_node.key(), b"key1");

        // Verify a leaf inside the left subtree.
        let ll = left_node.left();
        assert!(ll.is_leaf());
        assert_eq!(ll.key(), b"key0");
        assert_eq!(ll.value(), Some(b"val0".as_ref()));
    }

    #[test]
    fn test_write_snapshot_large() {
        // 100 leaves, roundtrip verify.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Build a skewed tree (right-heavy chain) to exercise preTrees logic.
        let mut nodes: Vec<NodeRef> = (0..100u32)
            .map(|i| {
                let key = format!("k{i:04}");
                let val = format!("v{i:04}");
                leaf(key.as_bytes(), val.as_bytes(), 1)
            })
            .collect();

        // Build balanced tree bottom-up.
        let mut version = 2u32;
        while nodes.len() > 1 {
            let mut next = Vec::new();
            let mut i = 0;
            while i + 1 < nodes.len() {
                next.push(branch(nodes[i].clone(), nodes[i + 1].clone(), version));
                i += 2;
            }
            if i < nodes.len() {
                next.push(nodes[i].clone());
            }
            nodes = next;
            version += 1;
        }

        let root_ref = &nodes[0];
        let expected_hash = root_ref.hash().to_vec();

        write_snapshot(d, 500, Some(root_ref)).unwrap();

        let snap = Snapshot::open(d).unwrap();
        assert_eq!(snap.version(), 500);
        assert_eq!(snap.leaf_count(), 100);
        // A balanced binary tree with 100 leaves has 99 branch nodes.
        assert_eq!(snap.node_count(), 99);
        assert_eq!(snap.root_hash(), expected_hash);

        // Spot-check a few key-value pairs.
        let (k, v) = snap.leaf_key_value(0);
        assert_eq!(k, b"k0000");
        assert_eq!(v, b"v0000");

        let (k, v) = snap.leaf_key_value(50);
        assert_eq!(k, b"k0050");
        assert_eq!(v, b"v0050");

        let (k, v) = snap.leaf_key_value(99);
        assert_eq!(k, b"k0099");
        assert_eq!(v, b"v0099");
    }

    #[test]
    fn test_write_snapshot_overwrite() {
        // Writing to the same directory twice should succeed (files are truncated).
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        let root1 = Node::Mem(MemNode::new_leaf_node(b"a".to_vec(), b"1".to_vec(), 1));
        write_snapshot(d, 1, Some(&root1)).unwrap();

        let root2 = Node::Mem(MemNode::new_leaf_node(b"b".to_vec(), b"2".to_vec(), 2));
        write_snapshot(d, 2, Some(&root2)).unwrap();

        let snap = Snapshot::open(d).unwrap();
        assert_eq!(snap.version(), 2);
        assert_eq!(snap.leaf_count(), 1);
        let (k, v) = snap.leaf_key_value(0);
        assert_eq!(k, b"b");
        assert_eq!(v, b"2");
    }
}
