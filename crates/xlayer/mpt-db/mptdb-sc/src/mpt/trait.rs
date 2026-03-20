use alloy_primitives::{Address, B256};
use mptdb_common::error::Result;
use reth_trie_common::AccountProof;
use revm_database::BundleState;

/// Metadata about the current commit state, distinguishing between logical
/// (in-memory), durable (on-disk), and working (uncommitted) frontiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFrontier {
    /// Latest version that commit() returned successfully (in-memory).
    pub logical_version: i64,
    /// Latest version whose nodes and manifest are confirmed on stable storage.
    pub durable_version: i64,
    /// Root hash of the latest logical version.
    pub committed_root: B256,
    /// Root hash of the latest durable version.
    pub durable_root: B256,
}

/// GC statistics returned by `MptCommitter::gc()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MptGcStats {
    pub scanned_nodes: u64,
    pub retained_nodes: u64,
    pub deleted_nodes: u64,
}

/// Metadata for an MPT snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MptSnapshotMeta {
    pub version: i64,
    pub state_root: B256,
}

/// A single trie node in an MPT snapshot stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MptSnapshotNode {
    pub hash: B256,
    pub rlp: Vec<u8>,
}

/// Streaming exporter for MPT snapshots.
pub trait MptSnapshotExporter: Send {
    /// Snapshot metadata (version + state root).
    fn meta(&self) -> &MptSnapshotMeta;
    /// Return the next node, or None when exhausted.
    fn next_node(&mut self) -> Result<Option<MptSnapshotNode>>;
    /// Close the exporter (idempotent).
    fn close(&mut self) -> Result<()>;
}

/// Streaming importer for MPT snapshots.
pub trait MptSnapshotImporter: Send {
    /// Add a single node. Hash must equal keccak256(rlp).
    fn add_node(&mut self, node: &MptSnapshotNode) -> Result<()>;
    /// Flush remaining buffer, verify integrity, write manifest. Idempotent.
    fn close(&mut self) -> Result<()>;
}

/// Independent MPT commit engine trait.
///
/// Each block: `apply_bundle_state()` then `commit()`.
/// Recovery: `load_version()` reloads from disk.
pub trait MptCommitter: Send {
    /// Apply a single block's final BundleState. Must be called exactly once per block.
    fn apply_bundle_state(&mut self, bundle: &BundleState) -> Result<()>;

    /// Persist current state, returning `(new_version, state_root)`.
    fn commit(&mut self) -> Result<(i64, B256)>;

    /// Current committed version.
    fn version(&self) -> i64;

    /// Reload latest committed version from disk manifest.
    fn load_version(&mut self) -> Result<()>;

    /// Rollback to a historical version (truncates manifest, rebuilds working state).
    fn rollback(&mut self, target_version: i64) -> Result<()>;

    /// Release resources (DB handle, file lock).
    fn close(&mut self) -> Result<()>;

    /// Remove manifest entries for versions in `[earliest, version)`, keeping `version` itself.
    fn prune_before(&mut self, version: i64) -> Result<()>;

    /// Mark-sweep GC: delete trie nodes not reachable from any manifest root.
    fn gc(&mut self) -> Result<MptGcStats>;

    /// Build an Ethereum account proof for the given committed version.
    fn account_proof(&self, version: i64, address: Address, slots: &[B256])
        -> Result<AccountProof>;

    /// Create a streaming snapshot exporter for the given committed version.
    fn exporter(&self, version: i64) -> Result<Box<dyn MptSnapshotExporter>>;

    /// Return the current commit frontier metadata.
    fn frontier(&self) -> CommitFrontier;

    /// Create a streaming snapshot importer. Only allowed on fresh DB.
    fn importer(
        &mut self,
        version: i64,
        expected_root: B256,
    ) -> Result<Box<dyn MptSnapshotImporter + '_>>;

    /// Set the initial version for a fresh DB.
    ///
    /// When `initial_version > 1` and the current version is 0, the first
    /// `commit()` jumps directly to `initial_version` instead of version 1.
    /// This mirrors sei-db's `SetInitialVersion` / `nextVersionU32` semantics
    /// for Cosmos chains whose genesis block starts at a non-zero height.
    ///
    /// Returns an error if the DB is not fresh (version != 0 or non-empty state).
    fn set_initial_version(&mut self, initial_version: i64) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    /// T1.1: MptCommitter object-safe
    #[test]
    fn t1_1_trait_object_safety() {
        fn _assert_object_safe(_: &dyn MptCommitter) {}
    }

    /// T1.2: MptSnapshotExporter object-safe
    #[test]
    fn t1_2_exporter_object_safety() {
        fn _assert_object_safe(_: &dyn MptSnapshotExporter) {}
    }

    /// T1.3: MptSnapshotImporter object-safe
    #[test]
    fn t1_3_importer_object_safety() {
        fn _assert_object_safe(_: &dyn MptSnapshotImporter) {}
    }

    /// T1.4: MptGcStats / MptSnapshotMeta / MptSnapshotNode derive clone/debug/eq
    #[test]
    fn t1_4_derive_traits() {
        let stats = MptGcStats { scanned_nodes: 1, retained_nodes: 1, deleted_nodes: 0 };
        assert_eq!(stats.clone(), stats);
        let _ = format!("{stats:?}");

        let meta = MptSnapshotMeta { version: 1, state_root: B256::ZERO };
        assert_eq!(meta.clone(), meta);
        let _ = format!("{meta:?}");

        let node = MptSnapshotNode { hash: B256::ZERO, rlp: vec![0x80] };
        assert_eq!(node.clone(), node);
        let _ = format!("{node:?}");
    }

    /// T1.5: MptSnapshotNode hash == keccak256(rlp) semantic example
    #[test]
    fn t1_5_snapshot_node_hash_semantic() {
        let rlp = vec![0xc1, 0x80];
        let node = MptSnapshotNode { hash: keccak256(&rlp), rlp: rlp.clone() };
        assert_eq!(node.hash, keccak256(&rlp));
    }

    /// T1.6: account_proof signature supports explicit historical version
    #[test]
    fn t1_6_account_proof_version_param() {
        // Compile-time check: the trait method takes `version: i64`
        fn _check(c: &dyn MptCommitter) {
            let _ = c.account_proof(42, Address::ZERO, &[]);
        }
    }

    /// T1.7: importer returns Box<dyn MptSnapshotImporter + '_> bound to &mut self
    #[test]
    fn t1_7_importer_lifetime_bound() {
        // Compile-time check: the return type borrows self
        fn _check(c: &mut dyn MptCommitter) {
            let _ = c.importer(1, B256::ZERO);
        }
    }
}
