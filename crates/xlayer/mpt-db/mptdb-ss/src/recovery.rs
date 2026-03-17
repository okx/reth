//! WAL recovery: replay changelog entries to bring a state store up to date.
//!
//! When a node restarts after a crash, the state store may be behind the WAL.
//! This module provides functions to replay WAL entries and bring the store to
//! the latest committed version.

use mptdb_common::{
    config::WalConfig,
    error::{MptDbError, Result},
};
use mptdb_proto::{ChangeSet, ChangelogEntry};
use mptdb_traits::{ss::StateStore, wal::Wal};
use mptdb_wal::changelog::{new_changelog_wal, ChangelogWal};
use std::path::Path;
use tracing::info;

/// Recover a state store by replaying WAL entries that are ahead of the store
/// version.
pub fn recover_state_store(changelog_path: &Path, store: &dyn StateStore) -> Result<()> {
    let store_version = store.get_latest_version();

    info!(store_version, ?changelog_path, "recovering state store from WAL");

    replay_wal(changelog_path, store_version, -1, &mut |entry: &ChangelogEntry| {
        if entry.version > store_version {
            let empty = ChangeSet { pairs: vec![] };
            let cs = entry.changeset.as_ref().unwrap_or(&empty);
            store.apply_changeset_sync(entry.version, cs).map_err(|e| {
                MptDbError::Other(format!(
                    "failed to apply changeset at version {}: {e}",
                    entry.version
                ))
            })?;
            store.set_latest_version(entry.version).map_err(|e| {
                MptDbError::Other(format!(
                    "failed to set store version {}: {e}",
                    entry.version
                ))
            })?;
        }
        Ok(())
    })
}

/// Replay WAL entries in `[from_version+1, to_version]`, invoking `handler`
/// for each entry.
///
/// If `to_version` is negative, replays to the end of the WAL.
pub(crate) fn replay_wal(
    changelog_path: &Path,
    from_version: i64,
    to_version: i64,
    handler: &mut dyn FnMut(&ChangelogEntry) -> Result<()>,
) -> Result<()> {
    let wal = new_changelog_wal(WalConfig::default(), changelog_path).map_err(|e| {
        MptDbError::Other(format!("failed to open WAL at {}: {e}", changelog_path.display()))
    })?;

    let first_offset = wal
        .first_offset()
        .map_err(|e| MptDbError::Other(format!("failed to read WAL first offset: {e}")))?;
    if first_offset == 0 {
        return Ok(());
    }

    let last_offset = wal
        .last_offset()
        .map_err(|e| MptDbError::Other(format!("failed to read WAL last offset: {e}")))?;
    if last_offset == 0 {
        return Ok(());
    }

    let last_entry = wal
        .read_at(last_offset)
        .map_err(|e| MptDbError::Other(format!("failed to read last WAL entry: {e}")))?;

    let end_version = if to_version < 0 { last_entry.version } else { to_version };

    // Nothing to replay if the WAL's latest entry is at or before from_version.
    if last_entry.version <= from_version {
        return Ok(());
    }

    let start_offset = find_replay_start_offset(&wal, first_offset, last_offset, from_version)?;

    if start_offset > last_offset {
        return Ok(());
    }

    info!(from_version, end_version, start_offset, last_offset, "replaying WAL");

    wal.replay(start_offset, last_offset, &mut |_index, entry: ChangelogEntry| {
        if to_version >= 0 && entry.version > to_version {
            return Ok(());
        }
        let _ = end_version; // suppress unused warning; bounds already checked above
        handler(&entry)
    })
}

/// Binary search for the first WAL offset whose entry has version > target_version.
fn find_replay_start_offset(
    wal: &ChangelogWal,
    first: u64,
    last: u64,
    target_version: i64,
) -> Result<u64> {
    let mut lo = first;
    let mut hi = last;
    let mut result = last + 1; // sentinel: "not found"

    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let entry = wal
            .read_at(mid)
            .map_err(|e| MptDbError::Other(format!("failed to read WAL at offset {mid}: {e}")))?;
        if entry.version > target_version {
            result = mid;
            if mid == first {
                break;
            }
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mptdb_common::config::StateStoreConfig;
    use mptdb_engine::mvcc::db::MvccDatabase;
    use mptdb_proto::KvPair;
    use mptdb_traits::wal::Wal;
    use tempfile::tempdir;

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    fn open_store(dir: &std::path::Path) -> Box<dyn StateStore> {
        let cfg = test_config(dir);
        let db = MvccDatabase::open_db(&cfg).unwrap();
        Box::new(db)
    }

    fn make_changelog_entry(
        version: i64,
        pairs: Vec<(&[u8], &[u8])>,
    ) -> ChangelogEntry {
        ChangelogEntry {
            version,
            changeset: Some(ChangeSet {
                pairs: pairs
                    .into_iter()
                    .map(|(k, v)| KvPair { delete: false, key: k.to_vec(), value: v.to_vec() })
                    .collect(),
            }),
        }
    }

    fn write_wal_entries(wal_dir: &std::path::Path, entries: &[ChangelogEntry]) {
        let wal =
            new_changelog_wal(WalConfig { fsync_enabled: false, ..Default::default() }, wal_dir)
                .unwrap();
        for entry in entries {
            wal.write(entry.clone()).unwrap();
        }
    }

    #[test]
    fn test_recover_empty_wal() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let store_dir = dir.path().join("store");
        let store = open_store(&store_dir);

        let result = recover_state_store(&wal_dir, store.as_ref());
        assert!(result.is_ok());
    }

    #[test]
    fn test_recover_store_behind() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Write 5 WAL entries.
        let entries: Vec<_> = (1..=5)
            .map(|v| {
                make_changelog_entry(v, vec![(b"key", format!("v{v}").as_bytes())])
            })
            .collect();
        write_wal_entries(&wal_dir, &entries);

        // Store is at version 3.
        let store_dir = dir.path().join("store");
        let store = open_store(&store_dir);
        let cs = ChangeSet {
            pairs: vec![KvPair { delete: false, key: b"key".to_vec(), value: b"v3".to_vec() }],
        };
        store.apply_changeset_sync(3, &cs).unwrap();
        store.set_latest_version(3).unwrap();
        assert_eq!(store.get_latest_version(), 3);

        // Recover: should replay v4 and v5.
        recover_state_store(&wal_dir, store.as_ref()).unwrap();

        assert_eq!(store.get_latest_version(), 5);
        let val = store.get(5, b"key").unwrap();
        assert_eq!(val, Some(b"v5".to_vec()));
    }

    #[test]
    fn test_replay_wal_basic() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let entries: Vec<_> = (1..=5)
            .map(|v| make_changelog_entry(v, vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        let mut collected = Vec::new();
        replay_wal(&wal_dir, 0, -1, &mut |entry| {
            collected.push(entry.version);
            Ok(())
        })
        .unwrap();

        assert_eq!(collected, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_replay_wal_from_version() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let entries: Vec<_> = (1..=5)
            .map(|v| make_changelog_entry(v, vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        let mut collected = Vec::new();
        replay_wal(&wal_dir, 3, -1, &mut |entry| {
            collected.push(entry.version);
            Ok(())
        })
        .unwrap();

        assert_eq!(collected, vec![4, 5]);
    }

    #[test]
    fn test_replay_wal_with_to_version() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let entries: Vec<_> = (1..=5)
            .map(|v| make_changelog_entry(v, vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        let mut collected = Vec::new();
        replay_wal(&wal_dir, 1, 3, &mut |entry| {
            collected.push(entry.version);
            Ok(())
        })
        .unwrap();

        assert_eq!(collected, vec![2, 3]);
    }

    #[test]
    fn test_find_replay_start_offset() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let entries: Vec<_> = (1..=5)
            .map(|v| make_changelog_entry(v, vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        let wal = new_changelog_wal(WalConfig::default(), &wal_dir).unwrap();
        let first = wal.first_offset().unwrap();
        let last = wal.last_offset().unwrap();

        let offset = find_replay_start_offset(&wal, first, last, 0).unwrap();
        assert_eq!(offset, first);

        let offset = find_replay_start_offset(&wal, first, last, 3).unwrap();
        let entry = wal.read_at(offset).unwrap();
        assert_eq!(entry.version, 4);

        let offset = find_replay_start_offset(&wal, first, last, 5).unwrap();
        assert_eq!(offset, last + 1);
    }
}
