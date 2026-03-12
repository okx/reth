use crate::composite::store::CompositeCommitStore;
use seidb_common::error::Result;
use seidb_proto::{CommitInfo, NamedChangeSet, TreeNameUpgrade};
use seidb_traits::sc::{CommitKvStore, Committer, Exporter, Importer};

impl Committer for CompositeCommitStore {
    fn initialize(&mut self, initial_stores: &[String]) {
        self.initialize(initial_stores)
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
        let mut new_store = CompositeCommitStore::new(&self.home_dir, &self.config);
        CompositeCommitStore::load_version(&mut new_store, version, read_only)?;
        Ok(Box::new(new_store))
    }

    fn rollback(&mut self, version: i64) -> Result<()> {
        self.rollback(version)
    }

    fn set_initial_version(&mut self, version: i64) -> Result<()> {
        self.set_initial_version(version)
    }

    fn get_child_store_by_name(&self, name: &str) -> Option<Box<dyn CommitKvStore>> {
        self.get_child_store_by_name(name)
    }

    fn importer(&self, version: i64) -> Result<Box<dyn Importer>> {
        self.create_importer(version)
    }

    fn exporter(&self, version: i64) -> Result<Box<dyn Exporter>> {
        self.create_exporter(version)
    }

    fn close(&mut self) -> Result<()> {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_composite_implements_committer() {
        fn _assert_committer<T: Committer>() {}
        _assert_committer::<CompositeCommitStore>();
    }

    #[test]
    fn test_composite_committer_send_sync() {
        fn _assert_send_sync<T: Send + Sync>() {}
        _assert_send_sync::<CompositeCommitStore>();
    }
}
