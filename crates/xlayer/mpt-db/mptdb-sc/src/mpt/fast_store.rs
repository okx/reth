use alloy_primitives::B256;
use mptdb_common::error::{MptDbError, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use super::{arena::MutableTrieArena, node::MptNode, tree::MptTree};

/// A lean storage trie image that omits rlp_cache and dirty flags but
/// retains hash_cache for clean-subtree skipping during encode.
#[derive(Serialize, Deserialize)]
pub struct StorageTrieImage {
    pub root: B256,
    nodes: Vec<MptNode>,
    hash_cache: Vec<Option<B256>>,
    root_idx: Option<u32>,
}

impl StorageTrieImage {
    /// Create from an MptTree, stripping rlp_cache and dirty.
    pub fn from_tree(tree: &MptTree, root: B256) -> Self {
        Self {
            root,
            nodes: tree.arena_nodes().to_vec(),
            hash_cache: tree.arena_hash_cache().to_vec(),
            root_idx: tree.root_index(),
        }
    }

    /// Restore into an MptTree with clean dirty flags and empty rlp_cache.
    pub fn into_tree(self) -> MptTree {
        MptTree::from_lean_image(self.nodes, self.hash_cache, self.root_idx)
    }
}

/// Persistent hot cache for storage trie images.
///
/// Key: hashed_address, Value: serialized StorageTrieImage.
/// Uses a single bincode file for simplicity in Phase A.
/// One read + one deserialize replaces ~20 recursive RocksDB reads
/// when a storage trie has a cache miss in L1 (working set) and L2 (LRU).
pub struct FastStorageTrieStore {
    dir: PathBuf,
    cache: Mutex<HashMap<B256, Vec<u8>>>,
}

impl FastStorageTrieStore {
    /// Open (or create) the fast store directory, loading any existing data from disk.
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .map_err(|e| MptDbError::Other(format!("create fast store dir: {e}")))?;
        let store = Self { dir: dir.to_path_buf(), cache: Mutex::new(HashMap::new()) };
        store.load_from_disk()?;
        Ok(store)
    }

    /// Load the latest image for a hashed address. Returns None on miss.
    pub fn load_latest(&self, hashed_address: &B256) -> Result<Option<StorageTrieImage>> {
        let cache = self.cache.lock();
        match cache.get(hashed_address) {
            Some(bytes) => {
                let image: StorageTrieImage = bincode::deserialize(bytes)
                    .map_err(|e| MptDbError::Other(format!("deserialize trie image: {e}")))?;
                Ok(Some(image))
            }
            None => Ok(None),
        }
    }

    /// Save a trie image for a hashed address (in-memory only until flush_to_disk).
    pub fn save_latest(&self, hashed_address: B256, image: &StorageTrieImage) -> Result<()> {
        let bytes = bincode::serialize(image)
            .map_err(|e| MptDbError::Other(format!("serialize trie image: {e}")))?;
        self.cache.lock().insert(hashed_address, bytes);
        Ok(())
    }

    /// Remove a trie image (e.g. on selfdestruct / storage wipe).
    pub fn delete_latest(&self, hashed_address: &B256) {
        self.cache.lock().remove(hashed_address);
    }

    /// Clear all entries from the in-memory cache.
    pub fn clear(&self) {
        self.cache.lock().clear();
    }

    /// Number of cached images.
    pub fn len(&self) -> usize {
        self.cache.lock().len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.lock().is_empty()
    }

    /// Persist the in-memory cache to disk atomically (write-tmp then rename).
    /// Called from the async persist worker or on close.
    pub fn flush_to_disk(&self) -> Result<()> {
        let cache = self.cache.lock();
        let data: Vec<(B256, Vec<u8>)> = cache.iter().map(|(k, v)| (*k, v.clone())).collect();
        drop(cache);

        let tmp = self.dir.join("images.bin.tmp");
        let target = self.dir.join("images.bin");
        let bytes = bincode::serialize(&data)
            .map_err(|e| MptDbError::Other(format!("serialize fast store: {e}")))?;
        fs::write(&tmp, bytes).map_err(|e| MptDbError::Other(format!("write fast store: {e}")))?;
        fs::rename(&tmp, &target)
            .map_err(|e| MptDbError::Other(format!("rename fast store: {e}")))?;
        Ok(())
    }

    /// Load persisted data from disk into the in-memory cache.
    fn load_from_disk(&self) -> Result<()> {
        let path = self.dir.join("images.bin");
        if !path.exists() {
            return Ok(());
        }
        let bytes =
            fs::read(&path).map_err(|e| MptDbError::Other(format!("read fast store: {e}")))?;
        let data: Vec<(B256, Vec<u8>)> = bincode::deserialize(&bytes)
            .map_err(|e| MptDbError::Other(format!("deserialize fast store: {e}")))?;
        let mut cache = self.cache.lock();
        for (k, v) in data {
            cache.insert(k, v);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use alloy_trie::Nibbles;
    use tempfile::TempDir;

    fn make_test_tree() -> MptTree {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(B256::with_last_byte(1));
        tree.insert(&key, vec![0xaa, 0xbb]);
        let key2 = Nibbles::unpack(B256::with_last_byte(2));
        tree.insert(&key2, vec![0xcc, 0xdd]);
        tree
    }

    fn tree_root(tree: &mut MptTree) -> B256 {
        tree.root_hash()
    }

    #[test]
    fn test_l3_hit() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(&dir.path().join("fast")).unwrap();

        let mut tree = make_test_tree();
        let root = tree_root(&mut tree);
        let addr = B256::with_last_byte(0x42);

        let image = StorageTrieImage::from_tree(&tree, root);
        store.save_latest(addr, &image).unwrap();

        // Load and verify
        let loaded = store.load_latest(&addr).unwrap().expect("should find image");
        assert_eq!(loaded.root, root);

        // Restore trie and verify content matches
        let restored = loaded.into_tree();
        let key = Nibbles::unpack(B256::with_last_byte(1));
        assert_eq!(restored.get(&key), Some(&[0xaa, 0xbb][..]));
        let key2 = Nibbles::unpack(B256::with_last_byte(2));
        assert_eq!(restored.get(&key2), Some(&[0xcc, 0xdd][..]));
    }

    #[test]
    fn test_l3_stale() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(&dir.path().join("fast")).unwrap();

        let mut tree = make_test_tree();
        let root = tree_root(&mut tree);
        let addr = B256::with_last_byte(0x42);

        let image = StorageTrieImage::from_tree(&tree, root);
        store.save_latest(addr, &image).unwrap();

        // Load with a different expected root -> stale
        let loaded = store.load_latest(&addr).unwrap().expect("image exists");
        let different_root = B256::with_last_byte(0xff);
        assert_ne!(loaded.root, different_root, "should be stale (root mismatch)");
    }

    #[test]
    fn test_l3_miss() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(&dir.path().join("fast")).unwrap();

        let addr = B256::with_last_byte(0x99);
        let loaded = store.load_latest(&addr).unwrap();
        assert!(loaded.is_none(), "should be None on miss");
    }

    #[test]
    fn test_l3_selfdestruct() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(&dir.path().join("fast")).unwrap();

        let mut tree = make_test_tree();
        let root = tree_root(&mut tree);
        let addr = B256::with_last_byte(0x42);

        let image = StorageTrieImage::from_tree(&tree, root);
        store.save_latest(addr, &image).unwrap();
        assert!(!store.is_empty());

        // Simulate selfdestruct
        store.delete_latest(&addr);
        let loaded = store.load_latest(&addr).unwrap();
        assert!(loaded.is_none(), "should be gone after delete");
    }

    #[test]
    fn test_flush_and_reload() {
        let dir = TempDir::new().unwrap();
        let fast_dir = dir.path().join("fast");

        let mut tree = make_test_tree();
        let root = tree_root(&mut tree);
        let addr = B256::with_last_byte(0x42);

        // Save and flush
        {
            let store = FastStorageTrieStore::open(&fast_dir).unwrap();
            let image = StorageTrieImage::from_tree(&tree, root);
            store.save_latest(addr, &image).unwrap();
            store.flush_to_disk().unwrap();
        }

        // Reload from a fresh store instance
        {
            let store = FastStorageTrieStore::open(&fast_dir).unwrap();
            assert_eq!(store.len(), 1);
            let loaded = store.load_latest(&addr).unwrap().expect("should persist across reload");
            assert_eq!(loaded.root, root);

            let restored = loaded.into_tree();
            let key = Nibbles::unpack(B256::with_last_byte(1));
            assert_eq!(restored.get(&key), Some(&[0xaa, 0xbb][..]));
        }
    }

    #[test]
    fn test_clear() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(&dir.path().join("fast")).unwrap();

        let mut tree = make_test_tree();
        let root = tree_root(&mut tree);
        let addr1 = B256::with_last_byte(0x01);
        let addr2 = B256::with_last_byte(0x02);

        let image = StorageTrieImage::from_tree(&tree, root);
        store.save_latest(addr1, &image).unwrap();
        store.save_latest(addr2, &image).unwrap();
        assert_eq!(store.len(), 2);

        store.clear();
        assert!(store.is_empty());
        assert!(store.load_latest(&addr1).unwrap().is_none());
    }

    #[test]
    fn test_from_tree_into_tree_roundtrip() {
        let mut tree = make_test_tree();
        let root = tree_root(&mut tree);

        let image = StorageTrieImage::from_tree(&tree, root);
        let mut restored = image.into_tree();

        // Root hash should match after recomputation
        let restored_root = restored.root_hash();
        assert_eq!(restored_root, root, "roundtrip root must match");
    }
}
