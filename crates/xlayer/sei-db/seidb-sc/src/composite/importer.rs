use seidb_common::error::{Result, SeiDbError};
use seidb_traits::sc::{Importer, ScSnapshotNode};

const EVM_STORE_NAME: &str = "evm";

/// Composite snapshot importer that routes nodes to both a Cosmos (memiavl)
/// importer and an optional EVM (flatkv) importer.
///
/// The Cosmos importer receives all nodes (it needs the full tree for root hash
/// computation). The EVM importer only receives leaf nodes (height == 0) for the
/// "evm" module, since FlatKV only stores leaf key-value pairs.
///
/// Mirrors the Go `SnapshotImporter` in `sei-db/state_db/sc/composite/importer.go`.
pub struct SnapshotImporter {
    cosmos_importer: Box<dyn Importer>,
    evm_importer: Option<Box<dyn Importer>>,
    current_module: String,
}

impl SnapshotImporter {
    /// Creates a new composite snapshot importer.
    ///
    /// `cosmos` receives all nodes. `evm`, if provided, receives only EVM leaf nodes.
    pub fn new(cosmos: Box<dyn Importer>, evm: Option<Box<dyn Importer>>) -> Self {
        Self { cosmos_importer: cosmos, evm_importer: evm, current_module: String::new() }
    }
}

impl Importer for SnapshotImporter {
    fn add_module(&mut self, name: &str) -> Result<()> {
        self.current_module = name.to_string();
        // FlatKV's AddModule is a no-op, so we only call cosmos
        self.cosmos_importer.add_module(name)
    }

    fn add_node(&mut self, node: &ScSnapshotNode) {
        // Cosmos always gets every node (needs full tree for root hash)
        self.cosmos_importer.add_node(node);

        // EVM importer only gets leaf nodes from the "evm" module
        if self.current_module == EVM_STORE_NAME && node.height == 0 && self.evm_importer.is_some()
        {
            self.evm_importer.as_mut().unwrap().add_node(node);
        }
    }

    fn close(&mut self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();

        if let Err(e) = self.cosmos_importer.close() {
            errors.push(format!("cosmos importer close failed: {e}"));
        }

        if let Some(ref mut evm) = self.evm_importer &&
            let Err(e) = evm.close()
        {
            errors.push(format!("evm importer close failed: {e}"));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(SeiDbError::Other(errors.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Mock importer that records all calls for verification.
    struct MockImporter {
        modules: Arc<Mutex<Vec<String>>>,
        nodes: Arc<Mutex<Vec<(Vec<u8>, Vec<u8>, i64, i8)>>>,
        close_called: Arc<Mutex<bool>>,
    }

    impl MockImporter {
        fn new() -> (
            Self,
            Arc<Mutex<Vec<String>>>,
            Arc<Mutex<Vec<(Vec<u8>, Vec<u8>, i64, i8)>>>,
            Arc<Mutex<bool>>,
        ) {
            let modules = Arc::new(Mutex::new(Vec::new()));
            let nodes = Arc::new(Mutex::new(Vec::new()));
            let close_called = Arc::new(Mutex::new(false));
            (
                Self {
                    modules: Arc::clone(&modules),
                    nodes: Arc::clone(&nodes),
                    close_called: Arc::clone(&close_called),
                },
                modules,
                nodes,
                close_called,
            )
        }
    }

    impl Importer for MockImporter {
        fn add_module(&mut self, name: &str) -> Result<()> {
            self.modules.lock().unwrap().push(name.to_string());
            Ok(())
        }

        fn add_node(&mut self, node: &ScSnapshotNode) {
            self.nodes.lock().unwrap().push((
                node.key.clone(),
                node.value.clone(),
                node.version,
                node.height,
            ));
        }

        fn close(&mut self) -> Result<()> {
            *self.close_called.lock().unwrap() = true;
            Ok(())
        }
    }

    fn leaf_node(key: &[u8], value: &[u8]) -> ScSnapshotNode {
        ScSnapshotNode { key: key.to_vec(), value: value.to_vec(), version: 1, height: 0 }
    }

    fn branch_node(key: &[u8], value: &[u8], height: i8) -> ScSnapshotNode {
        ScSnapshotNode { key: key.to_vec(), value: value.to_vec(), version: 1, height }
    }

    #[test]
    fn test_importer_cosmos_only() {
        let (cosmos, cosmos_modules, cosmos_nodes, cosmos_closed) = MockImporter::new();
        let mut importer = SnapshotImporter::new(Box::new(cosmos), None);

        importer.add_module("bank").unwrap();
        importer.add_node(&leaf_node(b"key1", b"val1"));
        importer.add_node(&branch_node(b"key2", b"val2", 3));

        importer.add_module("evm").unwrap();
        importer.add_node(&leaf_node(b"key3", b"val3"));

        importer.close().unwrap();

        let modules = cosmos_modules.lock().unwrap();
        assert_eq!(*modules, vec!["bank", "evm"]);

        let nodes = cosmos_nodes.lock().unwrap();
        assert_eq!(nodes.len(), 3);

        assert!(*cosmos_closed.lock().unwrap());
    }

    #[test]
    fn test_importer_with_evm_leaves() {
        let (cosmos, _cosmos_modules, cosmos_nodes, _cosmos_closed) = MockImporter::new();
        let (evm, evm_modules, evm_nodes, evm_closed) = MockImporter::new();
        let mut importer = SnapshotImporter::new(Box::new(cosmos), Some(Box::new(evm)));

        // Non-evm module: leaves should NOT go to evm importer
        importer.add_module("bank").unwrap();
        importer.add_node(&leaf_node(b"balance", b"100"));

        // EVM module: leaves should go to BOTH importers
        importer.add_module("evm").unwrap();
        importer.add_node(&leaf_node(b"evm_key", b"evm_val"));
        importer.add_node(&leaf_node(b"evm_key2", b"evm_val2"));

        importer.close().unwrap();

        // Cosmos gets all 3 nodes
        let cosmos_nodes = cosmos_nodes.lock().unwrap();
        assert_eq!(cosmos_nodes.len(), 3);

        // EVM gets only the 2 evm leaf nodes
        let evm_nodes = evm_nodes.lock().unwrap();
        assert_eq!(evm_nodes.len(), 2);
        assert_eq!(evm_nodes[0].0, b"evm_key");
        assert_eq!(evm_nodes[1].0, b"evm_key2");

        // add_module should NOT be called on evm importer
        assert!(evm_modules.lock().unwrap().is_empty());

        assert!(*evm_closed.lock().unwrap());
    }

    #[test]
    fn test_importer_evm_skip_branches() {
        let (cosmos, _cm, cosmos_nodes, _cc) = MockImporter::new();
        let (evm, _em, evm_nodes, _ec) = MockImporter::new();
        let mut importer = SnapshotImporter::new(Box::new(cosmos), Some(Box::new(evm)));

        importer.add_module("evm").unwrap();

        // Branch nodes (height > 0) should NOT go to evm
        importer.add_node(&branch_node(b"branch1", b"hash1", 1));
        importer.add_node(&branch_node(b"branch2", b"hash2", 5));

        // Leaf node (height == 0) should go to evm
        importer.add_node(&leaf_node(b"leaf1", b"val1"));

        importer.close().unwrap();

        // Cosmos gets all 3
        assert_eq!(cosmos_nodes.lock().unwrap().len(), 3);

        // EVM gets only the leaf
        let evm_nodes = evm_nodes.lock().unwrap();
        assert_eq!(evm_nodes.len(), 1);
        assert_eq!(evm_nodes[0].0, b"leaf1");
    }
}
