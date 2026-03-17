use crate::mvcc::{db::MvccDatabase, iterator::MvccIterator};
use crossbeam_channel::Receiver;
use mptdb_common::error::Result;
use mptdb_proto::ChangeSet;
use mptdb_traits::{iterator::DbIterator, ss::StateStore, types::SnapshotNode};

impl StateStore for MvccDatabase {
    fn get(&self, version: i64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get(version, key)
    }

    fn has(&self, version: i64, key: &[u8]) -> Result<bool> {
        self.has(version, key)
    }

    fn iterator(&self, version: i64, start: &[u8], end: &[u8]) -> Result<Box<dyn DbIterator>> {
        let start_opt = if start.is_empty() { None } else { Some(start) };
        let end_opt = if end.is_empty() { None } else { Some(end) };
        let iter = MvccIterator::new(
            self.engine.as_ref(),
            start_opt,
            end_opt,
            version,
            self.get_earliest_version(),
            false,
        )?;
        Ok(Box::new(iter))
    }

    fn reverse_iterator(
        &self,
        version: i64,
        start: &[u8],
        end: &[u8],
    ) -> Result<Box<dyn DbIterator>> {
        let start_opt = if start.is_empty() { None } else { Some(start) };
        let end_opt = if end.is_empty() { None } else { Some(end) };
        let iter = MvccIterator::new(
            self.engine.as_ref(),
            start_opt,
            end_opt,
            version,
            self.get_earliest_version(),
            true,
        )?;
        Ok(Box::new(iter))
    }

    fn raw_iterate(&self, f: &mut dyn FnMut(&[u8], &[u8], i64) -> bool) -> Result<bool> {
        self.raw_iterate(f)
    }

    fn get_latest_version(&self) -> i64 {
        self.get_latest_version()
    }

    fn set_latest_version(&self, version: i64) -> Result<()> {
        self.set_latest_version(version)
    }

    fn get_earliest_version(&self) -> i64 {
        self.get_earliest_version()
    }

    fn set_earliest_version(&self, version: i64, ignore_version: bool) -> Result<()> {
        self.set_earliest_version(version, ignore_version)
    }

    fn apply_changeset_sync(&self, version: i64, changeset: &ChangeSet) -> Result<()> {
        self.apply_changeset_sync(version, changeset)
    }

    fn apply_changeset_async(&self, version: i64, changeset: &ChangeSet) -> Result<()> {
        self.apply_changeset_async(version, changeset)
    }

    fn prune(&self, version: i64) -> Result<()> {
        self.prune(version)
    }

    fn import(&self, version: i64, nodes: Receiver<SnapshotNode>) -> Result<()> {
        self.import(version, nodes)
    }

    fn close(&mut self) -> Result<()> {
        self.close()
    }
}
