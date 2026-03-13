use crate::memiavl::{commit_store::MemiavlCommitStore, iterator::TreeIterator, tree::Tree};
use seidb_common::error::{Result, SeiDbError};
use seidb_proto::{CommitInfo, NamedChangeSet, TreeNameUpgrade};
use seidb_traits::{
    iterator::DbIterator,
    sc::{CommitKvStore, Committer, Exporter, Importer},
};

// ---------------------------------------------------------------------------
// CommitKvStore for Tree
// ---------------------------------------------------------------------------

impl CommitKvStore for Tree {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get(key)
    }

    fn has(&self, key: &[u8]) -> bool {
        self.has(key)
    }

    fn set(&mut self, key: &[u8], value: &[u8]) {
        self.set(key, value);
    }

    fn remove(&mut self, key: &[u8]) {
        self.remove(key);
    }

    fn version(&self) -> i64 {
        self.version()
    }

    fn root_hash(&self) -> Vec<u8> {
        self.root_hash()
    }

    fn iterator(&self, start: &[u8], end: &[u8], ascending: bool) -> Box<dyn DbIterator> {
        let start_opt = if start.is_empty() { None } else { Some(start) };
        let end_opt = if end.is_empty() { None } else { Some(end) };
        // Build a NodeRef from arena for backward compatibility with TreeIterator
        let root_ref = self.ensure_root_ref();
        let iter = TreeIterator::new(start_opt, end_opt, ascending, root_ref.as_ref());
        Box::new(TreeIteratorAdapter { inner: iter })
    }

    fn get_proof(&self, key: &[u8]) -> Result<Vec<u8>> {
        let proof = self.get_membership_proof(key)?;
        use prost::Message;
        let mut buf = Vec::new();
        proof.encode(&mut buf).map_err(|e| SeiDbError::Other(format!("encode proof: {e}")))?;
        Ok(buf)
    }

    fn close(&mut self) -> Result<()> {
        self.close()
    }
}

// ---------------------------------------------------------------------------
// TreeIteratorAdapter: adapts TreeIterator to DbIterator trait
// ---------------------------------------------------------------------------

struct TreeIteratorAdapter {
    inner: TreeIterator,
}

/// SAFETY: `TreeIterator` contains only `Vec<u8>`, `bool`, and `Vec<NodeRef>`
/// where `NodeRef = Arc<Node>`. All these types are `Send`.
unsafe impl Send for TreeIteratorAdapter {}

impl DbIterator for TreeIteratorAdapter {
    fn domain(&self) -> (Option<&[u8]>, Option<&[u8]>) {
        self.inner.domain()
    }

    fn valid(&self) -> bool {
        self.inner.valid()
    }

    fn next(&mut self) {
        self.inner.next();
    }

    fn key(&self) -> &[u8] {
        self.inner.key()
    }

    fn value(&self) -> &[u8] {
        self.inner.value()
    }

    fn error(&self) -> Option<&SeiDbError> {
        None
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Committer for MemiavlCommitStore
// ---------------------------------------------------------------------------

impl Committer for MemiavlCommitStore {
    fn initialize(&mut self, initial_stores: &[String]) {
        self.initialize(initial_stores);
    }

    fn commit(&mut self) -> Result<i64> {
        self.commit()
    }

    fn version(&self) -> i64 {
        self.version()
    }

    fn get_latest_version(&self) -> Result<i64> {
        self.get_latest_version()
    }

    fn get_earliest_version(&self) -> Result<i64> {
        self.get_earliest_version()
    }

    fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()> {
        self.apply_change_sets(cs)
    }

    fn apply_upgrades(&mut self, upgrades: &[TreeNameUpgrade]) -> Result<()> {
        self.apply_upgrades(upgrades)
    }

    fn working_commit_info(&self) -> CommitInfo {
        self.working_commit_info()
    }

    fn last_commit_info(&self) -> CommitInfo {
        self.last_commit_info()
    }

    fn load_version(&self, version: i64, read_only: bool) -> Result<Box<dyn Committer>> {
        let mut store = MemiavlCommitStore::new(&self.home_dir, self.config.clone());
        MemiavlCommitStore::load_version(&mut store, version, read_only)?;
        Ok(Box::new(store))
    }

    fn rollback(&mut self, version: i64) -> Result<()> {
        self.rollback(version)
    }

    fn set_initial_version(&mut self, version: i64) -> Result<()> {
        self.set_initial_version(version)
    }

    fn get_child_store_by_name(&self, name: &str) -> Option<Box<dyn CommitKvStore>> {
        let tree_ref = self.get_child_store_by_name(name)?;
        let cloned = tree_ref.snapshot_copy();
        Some(Box::new(cloned))
    }

    fn importer(&self, version: i64) -> Result<Box<dyn Importer>> {
        self.importer(version)
    }

    fn exporter(&self, version: i64) -> Result<Box<dyn Exporter>> {
        self.exporter(version)
    }

    fn close(&mut self) -> Result<()> {
        self.close()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_commit_kv_store_get_set() {
        let mut tree = Tree::new_empty(0, 0);

        // Use CommitKvStore trait methods.
        let store: &mut dyn CommitKvStore = &mut tree;
        store.set(b"hello", b"world");
        store.set(b"foo", b"bar");

        assert_eq!(store.get(b"hello"), Some(b"world".to_vec()));
        assert_eq!(store.get(b"foo"), Some(b"bar".to_vec()));
        assert!(store.has(b"hello"));
        assert!(!store.has(b"missing"));

        // Update via trait.
        store.set(b"hello", b"updated");
        assert_eq!(store.get(b"hello"), Some(b"updated".to_vec()));

        // Remove via trait.
        store.remove(b"foo");
        assert!(!store.has(b"foo"));
        assert_eq!(store.get(b"foo"), None);
    }

    #[test]
    fn test_tree_commit_kv_store_version_and_hash() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"k", b"v");
        tree.save_version(true).unwrap();

        let store: &dyn CommitKvStore = &tree;
        assert_eq!(store.version(), 1);
        assert_eq!(store.root_hash().len(), 32);
    }

    #[test]
    fn test_tree_commit_kv_store_iterator() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"aaa", b"1");
        tree.set(b"bbb", b"2");
        tree.set(b"ccc", b"3");
        tree.set(b"ddd", b"4");
        tree.set(b"eee", b"5");

        let store: &dyn CommitKvStore = &tree;

        // Full ascending iteration.
        let mut iter = store.iterator(b"", b"", true);
        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(iter.key().to_vec());
            iter.next();
        }
        assert_eq!(
            keys,
            vec![
                b"aaa".to_vec(),
                b"bbb".to_vec(),
                b"ccc".to_vec(),
                b"ddd".to_vec(),
                b"eee".to_vec()
            ]
        );
        iter.close().unwrap();

        // Range iteration: [bbb, ddd) ascending.
        let mut iter = store.iterator(b"bbb", b"ddd", true);
        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(iter.key().to_vec());
            iter.next();
        }
        assert_eq!(keys, vec![b"bbb".to_vec(), b"ccc".to_vec()]);
        iter.close().unwrap();

        // Descending iteration.
        let mut iter = store.iterator(b"", b"", false);
        let mut keys = Vec::new();
        while iter.valid() {
            keys.push(iter.key().to_vec());
            iter.next();
        }
        assert_eq!(
            keys,
            vec![
                b"eee".to_vec(),
                b"ddd".to_vec(),
                b"ccc".to_vec(),
                b"bbb".to_vec(),
                b"aaa".to_vec()
            ]
        );
        iter.close().unwrap();
    }

    #[test]
    fn test_tree_commit_kv_store_proof() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"alice", b"100");
        tree.set(b"bob", b"200");
        tree.save_version(true).unwrap();

        let store: &dyn CommitKvStore = &tree;

        // get_proof should succeed for existing key.
        let proof_bytes = store.get_proof(b"alice").unwrap();
        assert!(!proof_bytes.is_empty());

        // Decode the proof back to verify it's valid protobuf.
        use prost::Message;
        let decoded = ics23::CommitmentProof::decode(proof_bytes.as_slice()).unwrap();
        assert!(matches!(&decoded.proof, Some(ics23::commitment_proof::Proof::Exist(_))));

        // get_proof should fail for missing key.
        let result = store.get_proof(b"missing");
        assert!(result.is_err());
    }

    #[test]
    fn test_tree_commit_kv_store_close() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"k", b"v");

        let store: &mut dyn CommitKvStore = &mut tree;
        store.close().unwrap();
        assert_eq!(store.get(b"k"), None);
    }

    #[test]
    fn test_tree_commit_kv_store_empty_iterator() {
        let tree = Tree::new_empty(0, 0);
        let store: &dyn CommitKvStore = &tree;
        let iter = store.iterator(b"", b"", true);
        assert!(!iter.valid());
    }

    #[test]
    fn test_tree_commit_kv_store_object_safety() {
        // Verify Tree can be used as a trait object.
        fn _assert_commit_kv_store(_: Box<dyn CommitKvStore>) {}
        let tree = Tree::new_empty(0, 0);
        _assert_commit_kv_store(Box::new(tree));
    }

    #[test]
    fn test_tree_snapshot_copy() {
        let mut tree = Tree::new_empty(0, 0);
        tree.set(b"key1", b"val1");
        tree.set(b"key2", b"val2");
        tree.save_version(true).unwrap();

        // snapshot_copy from &self context.
        let copy = tree.snapshot_copy();
        assert_eq!(copy.get(b"key1"), Some(b"val1".to_vec()));
        assert_eq!(copy.get(b"key2"), Some(b"val2".to_vec()));
        assert_eq!(copy.version(), 1);

        // Modify original — copy should be unaffected.
        tree.set(b"key1", b"modified");
        assert_eq!(copy.get(b"key1"), Some(b"val1".to_vec()));
        assert_eq!(tree.get(b"key1"), Some(b"modified".to_vec()));
    }
}
