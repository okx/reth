use seidb_common::error::Result;
use seidb_proto::NamedChangeSet;
use seidb_traits::{
    iterator::DbIterator,
    sc::{Exporter, Importer},
};

/// FlatKV internal trait (not exposed in seidb-traits, only used within seidb-sc).
/// Phase 7's CompositeCommitStore uses this trait object to operate on FlatKV.
#[allow(dead_code)]
pub(crate) trait FlatKvStore: Send {
    fn load_version(&mut self, target_version: i64) -> Result<()>;
    fn apply_change_sets(&mut self, cs: &[NamedChangeSet]) -> Result<()>;
    fn commit(&mut self) -> Result<i64>;
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn has(&self, key: &[u8]) -> bool;
    fn iterator(&self, start: &[u8], end: &[u8]) -> Box<dyn DbIterator>;
    fn iterator_by_prefix(&self, prefix: &[u8]) -> Box<dyn DbIterator>;
    fn root_hash(&self) -> Vec<u8>;
    fn version(&self) -> i64;
    fn write_snapshot(&mut self) -> Result<()>;
    fn rollback(&mut self, target_version: i64) -> Result<()>;
    fn exporter(&self, version: i64) -> Result<Box<dyn Exporter>>;
    fn importer(&self, version: i64) -> Result<Box<dyn Importer>>;
    fn close(&mut self) -> Result<()>;
}

// Sub-modules (Phase 5 implementation)
pub mod catchup;
pub mod commit;
pub mod importer;
pub mod iterator;
pub mod keys;
pub mod lthash;
pub mod lthash_compute;
pub mod meta;
pub mod read;
pub mod snapshot;
pub mod snapshot_dir;
pub mod store;
pub mod trait_impl;
pub mod write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trait_object_safety() {
        fn _assert_flat_kv_store(_: Box<dyn FlatKvStore>) {}
    }
}
