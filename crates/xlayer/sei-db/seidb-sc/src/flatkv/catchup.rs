//! FlatKV rollback and WAL catchup.
//!
//! Implements WAL replay (catchup), rollback to a prior snapshot, and
//! helper methods for WAL offset resolution. Ported from the Go reference
//! in `sei-db/state_db/sc/flatkv/store_catchup.go` and `snapshot.go`.

use crate::flatkv::{
    meta::commit_global_metadata,
    snapshot_dir::{
        atomic_remove_dir, seek_snapshot, snapshot_name, traverse_snapshots,
        update_current_symlink, working_dir_path, CHANGELOG_DIR, SNAPSHOT_BASE_FILE,
    },
    store::CommitStore,
};
use seidb_common::{
    config::WalConfig,
    error::{Result, SeiDbError},
};
use seidb_traits::wal::Wal;
use seidb_wal::changelog::new_changelog_wal;
use std::fs;
use tracing::{error, info};

impl CommitStore {
    /// Restores state to `target_version` by rewinding to the highest
    /// snapshot <= target_version, replaying WAL to reach the target, and
    /// truncating all WAL entries and snapshots beyond that point.
    ///
    /// Crash safety: the WAL is truncated BEFORE catchup writes any data to
    /// RocksDB. If the process crashes after truncation but before catchup
    /// completes, the next restart will simply re-run catchup against the
    /// already-truncated WAL, converging to target_version.
    pub fn rollback(&mut self, target_version: i64) -> Result<()> {
        info!(target_version, "FlatKV Rollback");

        let dir = self.flatkv_dir();

        // Close all DBs but keep the file lock.
        self.close_dbs_only()?;

        // Find the highest snapshot <= target_version.
        let base_version = seek_snapshot(&dir, target_version)?.unwrap_or(0);

        // Update the current symlink to point to that snapshot.
        update_current_symlink(&dir, &snapshot_name(base_version))?;

        // Force a fresh working dir clone from the rollback snapshot: the
        // current working dir may contain data beyond target_version.
        let snapshot_base_path = working_dir_path(&dir).join(SNAPSHOT_BASE_FILE);
        if let Err(e) = fs::remove_file(&snapshot_base_path) &&
            e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(SeiDbError::Other(format!("remove SNAPSHOT_BASE for rollback: {e}")));
        }

        // Re-open with fresh working dir cloned from snapshot.
        self.open()?;

        // Truncate WAL beyond target_version BEFORE catchup (crash safety).
        if self.changelog.is_some() {
            match self.wal_offset_for_version(target_version) {
                Ok(off) if off > 0 => {
                    let changelog = self.changelog.as_ref().unwrap();
                    changelog.truncate_after(off).map_err(|e| {
                        SeiDbError::Other(format!(
                            "truncate WAL after version {target_version} (offset {off}): {e}"
                        ))
                    })?;
                    self.verify_wal_tail(target_version)?;
                }
                _ => {
                    // Target predates all WAL entries; clear the entire WAL to
                    // prevent re-application.
                    let should_clear = if let Some(ref changelog) = self.changelog {
                        let last_off = changelog.last_offset().unwrap_or(0);
                        last_off > 0
                    } else {
                        false
                    };
                    if should_clear {
                        self.clear_changelog()?;
                    }
                }
            }
        }

        // Replay WAL from snapshot version to target.
        self.catchup(target_version)?;

        if self.committed_version != target_version {
            return Err(SeiDbError::Other(format!(
                "rollback failed: wanted version {} but reached {} (WAL may be incomplete)",
                target_version, self.committed_version
            )));
        }

        // Remove snapshots beyond target_version (best-effort).
        let _ = traverse_snapshots(&dir, true, |v| {
            if v > target_version &&
                let Err(e) = atomic_remove_dir(&dir.join(snapshot_name(v)))
            {
                error!(version = v, %e, "failed to remove snapshot");
            }
            Ok(true) // continue iterating
        });

        info!(version = self.committed_version, "FlatKV Rollback complete");
        Ok(())
    }

    /// Replays WAL entries from the current committed_version up to (and
    /// including) target_version. If target_version == 0, replay continues to
    /// the end of the WAL.
    ///
    /// Each replayed entry runs through apply_change_sets (which updates
    /// working_lt_hash) and commit_batches (which persists to per-DB RocksDBs).
    /// After all entries are replayed, global metadata is flushed once.
    pub(crate) fn catchup(&mut self, target_version: i64) -> Result<()> {
        let changelog = match &self.changelog {
            Some(c) => c,
            None => {
                return Err(SeiDbError::Other("catchup: changelog not open".to_string()));
            }
        };

        let first_off = changelog
            .first_offset()
            .map_err(|e| SeiDbError::Other(format!("catchup: first offset: {e}")))?;
        let last_off = changelog
            .last_offset()
            .map_err(|e| SeiDbError::Other(format!("catchup: last offset: {e}")))?;

        if last_off == 0 || first_off > last_off {
            return Ok(());
        }

        // Determine start offset: skip entries already committed.
        let mut start_off = first_off;
        if self.committed_version > 0 {
            // Try to find the offset for committed_version + 1 (next entry to replay).
            if let Ok(off) = self.wal_offset_for_version(self.committed_version + 1) &&
                off > start_off
            {
                if off > last_off {
                    return Ok(()); // Already past the WAL end.
                }
                start_off = off;
            }
        }

        // Bound end offset to avoid deserializing entries past the target.
        let mut end_off = last_off;
        if target_version > 0 {
            let off = self.wal_offset_for_version(target_version).map_err(|e| {
                SeiDbError::Other(format!(
                    "catchup: resolve WAL offset for target version {target_version}: {e}"
                ))
            })?;
            if off > 0 && off < end_off {
                end_off = off;
            }
        }

        // Collect entries to replay (we need &mut self for apply/commit).
        let changelog = self.changelog.as_ref().unwrap();
        let mut entries = Vec::new();
        changelog.replay(start_off, end_off, &mut |_offset, entry| {
            entries.push(entry);
            Ok(())
        })?;

        let committed_version = self.committed_version;
        let mut replayed = 0usize;

        for entry in entries {
            if entry.version <= committed_version && entry.version <= self.committed_version {
                continue;
            }
            if target_version > 0 && entry.version > target_version {
                continue;
            }

            self.apply_change_sets(&entry.changesets)?;
            self.commit_batches(entry.version)?;

            self.committed_version = entry.version;
            self.committed_lt_hash = self.working_lt_hash.clone();
            self.clear_pending_writes();

            replayed += 1;
            if replayed.is_multiple_of(1000) {
                info!(replayed, version = entry.version, "FlatKV catchup progress");
            }
        }

        if replayed > 0 {
            if !self.config.fsync {
                // During catchup with fsync=false, per-entry batch commits can
                // leave data only in OS page cache. Flush once before advancing
                // global metadata so the watermark won't get ahead of data
                // durability.
                self.flush_all_dbs()?;
            }

            let metadata_db = self
                .metadata_db
                .as_ref()
                .ok_or_else(|| SeiDbError::Other("metadata_db not open".to_string()))?;
            commit_global_metadata(
                metadata_db,
                self.committed_version,
                &self.committed_lt_hash,
                self.config.fsync,
            )?;

            info!(replayed, version = self.committed_version, "FlatKV catchup complete");
        }

        Ok(())
    }

    /// Returns the WAL offset whose entry has the given version.
    ///
    /// Strategy: try an arithmetic shortcut (O(1) reads) first -- it works when
    /// each version maps 1:1 to a sequential offset. On mismatch, fall back to
    /// binary search (O(log N) reads).
    fn wal_offset_for_version(&self, version: i64) -> Result<u64> {
        let changelog = self
            .changelog
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("changelog not open".to_string()))?;

        let first_off = changelog.first_offset()?;
        if first_off == 0 {
            return Ok(0);
        }
        let last_off = changelog.last_offset()?;
        if last_off == 0 || first_off > last_off {
            return Ok(0);
        }

        let first_ver = self.wal_version_at_offset(first_off)?;
        if first_ver <= 0 || version < first_ver {
            return Ok(0);
        }

        // Fast path: O(1) arithmetic guess.
        let guess = first_off + (version - first_ver) as u64;
        if guess >= first_off &&
            guess <= last_off &&
            let Ok(v) = self.wal_version_at_offset(guess) &&
            v == version
        {
            return Ok(guess);
        }

        // Slow path: binary search over [first_off, last_off].
        let mut lo = first_off;
        let mut hi = last_off;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let v = self.wal_version_at_offset(mid).map_err(|e| {
                SeiDbError::Other(format!("WAL binary search at offset {mid}: {e}"))
            })?;
            if v == version {
                return Ok(mid);
            } else if v < version {
                lo = mid + 1;
            } else {
                if mid == 0 {
                    break;
                }
                hi = mid - 1;
            }
        }

        Err(SeiDbError::Other(format!(
            "WAL version {version} not found (range {first_off}-{last_off})"
        )))
    }

    /// Reads a single WAL entry and returns its version.
    fn wal_version_at_offset(&self, offset: u64) -> Result<i64> {
        let changelog = self
            .changelog
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("changelog not open".to_string()))?;
        let entry = changelog.read_at(offset)?;
        Ok(entry.version)
    }

    /// Closes the WAL, deletes its directory, and re-opens an empty WAL.
    ///
    /// Used by rollback when the target version predates all WAL entries and
    /// the entire log must be discarded to prevent re-application on restart.
    fn clear_changelog(&mut self) -> Result<()> {
        if let Some(mut wal) = self.changelog.take() {
            wal.close().map_err(|e| SeiDbError::Other(format!("close changelog: {e}")))?;
        }

        let changelog_dir = self.flatkv_dir().join(CHANGELOG_DIR);
        if changelog_dir.exists() {
            fs::remove_dir_all(&changelog_dir)
                .map_err(|e| SeiDbError::Other(format!("remove changelog dir: {e}")))?;
        }

        self.changelog = Some(
            new_changelog_wal(WalConfig::default(), &changelog_dir)
                .map_err(|e| SeiDbError::Other(format!("reopen changelog: {e}")))?,
        );
        Ok(())
    }

    /// Checks that the last WAL entry has the expected version.
    ///
    /// Returns an error if there is a mismatch (WAL integrity failure).
    fn verify_wal_tail(&self, expected_version: i64) -> Result<()> {
        let changelog = self
            .changelog
            .as_ref()
            .ok_or_else(|| SeiDbError::Other("changelog not open".to_string()))?;

        let last_off = changelog
            .last_offset()
            .map_err(|e| SeiDbError::Other(format!("verify WAL last offset: {e}")))?;

        let last_ver = self.wal_version_at_offset(last_off)?;
        if last_ver != expected_version {
            return Err(SeiDbError::Other(format!(
                "WAL integrity check failed: last entry is version {last_ver}, expected {expected_version}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatkv::{
        keys::ADDRESS_LEN,
        snapshot_dir::{reuse_working_dir, snapshot_name},
    };
    use seidb_common::{config::FlatKvConfig, evm_keys::STATE_KEY_PREFIX};
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use tempfile::TempDir;

    /// Helper: create an open CommitStore.
    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    fn open_store_with_config(config: FlatKvConfig) -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), config);
        store.load_version(0).unwrap();
        (store, dir)
    }

    fn test_addr(seed: u8) -> [u8; ADDRESS_LEN] {
        let mut addr = [0u8; ADDRESS_LEN];
        addr[0] = seed;
        addr[19] = seed;
        addr
    }

    fn test_slot(seed: u8) -> [u8; 32] {
        let mut slot = [0u8; 32];
        slot[0] = seed;
        slot
    }

    fn make_storage_key(addr: &[u8; ADDRESS_LEN], slot: &[u8; 32]) -> Vec<u8> {
        let mut key = Vec::with_capacity(1 + ADDRESS_LEN + 32);
        key.push(STATE_KEY_PREFIX);
        key.extend_from_slice(addr);
        key.extend_from_slice(slot);
        key
    }

    fn evm_cs(pairs: Vec<KvPair>) -> Vec<NamedChangeSet> {
        vec![NamedChangeSet { name: "evm".to_string(), changeset: Some(ChangeSet { pairs }) }]
    }

    /// Helper: apply a storage write and commit.
    fn commit_storage_entry(
        store: &mut CommitStore,
        addr: &[u8; ADDRESS_LEN],
        slot: &[u8; 32],
        value: &[u8],
    ) {
        let cs = evm_cs(vec![KvPair {
            key: make_storage_key(addr, slot),
            value: value.to_vec(),
            delete: false,
        }]);
        store.apply_change_sets(&cs).unwrap();
        store.commit().unwrap();
    }

    #[test]
    fn test_catchup_from_empty() {
        // Empty WAL should be a no-op.
        let (mut store, _dir) = open_store();
        store.catchup(0).unwrap();
        assert_eq!(store.version(), 0);
        store.close().unwrap();
    }

    #[test]
    fn test_catchup_replays_entries() {
        let dir = TempDir::new().unwrap();
        let db_dir = dir.path().to_str().unwrap();

        // Open store, commit 5 versions with storage data.
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            store.load_version(0).unwrap();

            for i in 1u8..=5 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            }

            // Create a snapshot at version 5.
            store.write_snapshot().unwrap();

            // Commit 3 more versions (6, 7, 8) — only in WAL + working dir.
            for i in 6u8..=8 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            }
            assert_eq!(store.version(), 8);
            store.close().unwrap();
        }

        // Re-open at version 5 (snapshot), then catchup should replay 6,7,8 from WAL.
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            // open_to(0) replays to WAL end
            store.load_version(0).unwrap();
            assert_eq!(store.version(), 8, "catchup should have replayed to version 8");

            // Verify data from version 8 is present.
            let addr = test_addr(8);
            let slot = test_slot(8);
            let mut internal_key = addr.to_vec();
            internal_key.extend_from_slice(&slot);
            let val = store.storage_db.as_ref().unwrap();
            use seidb_traits::kv::KvEngine;
            let v = val.get(&internal_key).unwrap().expect("version 8 data should exist");
            assert_eq!(v, vec![8u8; 4]);

            store.close().unwrap();
        }
    }

    #[test]
    fn test_catchup_updates_lt_hash() {
        let dir = TempDir::new().unwrap();
        let db_dir = dir.path().to_str().unwrap();

        // Commit a few versions, record the LtHash at version 3.
        let lt_hash_at_v3;
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            store.load_version(0).unwrap();

            for i in 1u8..=3 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            }
            lt_hash_at_v3 = store.committed_lt_hash.clone();

            // Create snapshot and commit more.
            store.write_snapshot().unwrap();
            for i in 4u8..=5 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            }
            store.close().unwrap();
        }

        // Re-open and catchup to version 3 exactly should match lt_hash.
        // Actually, we re-open at 0 (replay all) to version 5.
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            store.load_version(0).unwrap();
            assert_eq!(store.version(), 5);
            // The LtHash should NOT equal v3 since we caught up to v5.
            assert_ne!(store.committed_lt_hash, lt_hash_at_v3);
            // But it should be non-zero (we have data).
            assert_ne!(store.committed_lt_hash, crate::flatkv::lthash::LtHash::new());
            store.close().unwrap();
        }
    }

    #[test]
    fn test_wal_offset_for_version() {
        let (mut store, _dir) = open_store();

        // Commit 5 entries to WAL, creating sequential offsets.
        for i in 1u8..=5 {
            let addr = test_addr(i);
            let slot = test_slot(i);
            commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
        }

        // The fast path should hit for versions 1-5 (1:1 mapping).
        for v in 1i64..=5 {
            let off = store.wal_offset_for_version(v).unwrap();
            assert!(off > 0, "offset for version {v} should be > 0");
            let actual_ver = store.wal_version_at_offset(off).unwrap();
            assert_eq!(actual_ver, v, "version at offset should match");
        }

        // Version 0 should return offset 0 (predates all entries).
        let off = store.wal_offset_for_version(0).unwrap();
        assert_eq!(off, 0);

        // Version 6 should fail (not in WAL).
        let result = store.wal_offset_for_version(6);
        assert!(result.is_err());

        store.close().unwrap();
    }

    #[test]
    fn test_rollback_rewinds_state() {
        let mut config = FlatKvConfig::default();
        config.snapshot_interval = 0; // manual snapshots only
        let (mut store, _dir) = open_store_with_config(config.clone());

        // Commit versions 1-3.
        for i in 1u8..=3 {
            let addr = test_addr(i);
            let slot = test_slot(i);
            commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
        }
        // Snapshot at version 3.
        store.write_snapshot().unwrap();

        // Commit versions 4-6.
        for i in 4u8..=6 {
            let addr = test_addr(i);
            let slot = test_slot(i);
            commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
        }
        assert_eq!(store.version(), 6);

        // Rollback to version 4 (will use snapshot at v3 + catchup v4 from WAL).
        store.rollback(4).unwrap();
        assert_eq!(store.version(), 4);

        // Data for version 4 should exist.
        {
            use seidb_traits::kv::KvEngine;
            let addr = test_addr(4);
            let slot = test_slot(4);
            let mut internal_key = addr.to_vec();
            internal_key.extend_from_slice(&slot);
            let v = store
                .storage_db
                .as_ref()
                .unwrap()
                .get(&internal_key)
                .unwrap()
                .expect("version 4 data should exist after rollback");
            assert_eq!(v, vec![4u8; 4]);
        }

        store.close().unwrap();
    }

    #[test]
    fn test_rollback_removes_future_snapshots() {
        let mut config = FlatKvConfig::default();
        config.snapshot_interval = 0;
        config.snapshot_keep_recent = 10; // keep all
        let (mut store, _dir) = open_store_with_config(config);

        // Commit and snapshot at versions 3, 6, 9.
        for i in 1u8..=9 {
            let addr = test_addr(i);
            let slot = test_slot(i);
            commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            if i % 3 == 0 {
                store.write_snapshot().unwrap();
            }
        }

        let flatkv_dir = store.flatkv_dir();

        // Rollback to version 5 (uses snapshot at v3).
        store.rollback(5).unwrap();
        assert_eq!(store.version(), 5);

        // Snapshots at version 6 and 9 should be removed.
        let mut remaining: Vec<i64> = Vec::new();
        traverse_snapshots(&flatkv_dir, false, |v| {
            remaining.push(v);
            Ok(true)
        })
        .unwrap();

        assert!(
            !remaining.contains(&6),
            "snapshot at v6 should be removed, remaining: {remaining:?}"
        );
        assert!(
            !remaining.contains(&9),
            "snapshot at v9 should be removed, remaining: {remaining:?}"
        );
        // Snapshots at v0 and v3 should remain.
        assert!(remaining.contains(&3), "snapshot at v3 should remain, remaining: {remaining:?}");

        store.close().unwrap();
    }

    #[test]
    fn test_persistence_after_reopen() {
        let dir = TempDir::new().unwrap();
        let db_dir = dir.path().to_str().unwrap();

        // First session: commit data and snapshot.
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            store.load_version(0).unwrap();

            for i in 1u8..=3 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            }
            store.write_snapshot().unwrap();

            // Commit 2 more versions (only in WAL).
            for i in 4u8..=5 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                commit_storage_entry(&mut store, &addr, &slot, &[i; 4]);
            }
            assert_eq!(store.version(), 5);
            store.close().unwrap();
        }

        // Second session: re-open and verify catchup replayed WAL.
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            store.load_version(0).unwrap();
            assert_eq!(store.version(), 5, "should have caught up to version 5 via WAL");

            // Verify all data is present.
            use seidb_traits::kv::KvEngine;
            for i in 1u8..=5 {
                let addr = test_addr(i);
                let slot = test_slot(i);
                let mut internal_key = addr.to_vec();
                internal_key.extend_from_slice(&slot);
                let v = store
                    .storage_db
                    .as_ref()
                    .unwrap()
                    .get(&internal_key)
                    .unwrap()
                    .unwrap_or_else(|| panic!("version {i} data should exist"));
                assert_eq!(v, vec![i; 4]);
            }
            store.close().unwrap();
        }
    }

    #[test]
    fn test_reopen_reuses_working_dir() {
        let dir = TempDir::new().unwrap();
        let db_dir = dir.path().to_str().unwrap();

        // First session: commit and snapshot.
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());
            store.load_version(0).unwrap();

            let addr = test_addr(1);
            let slot = test_slot(1);
            commit_storage_entry(&mut store, &addr, &slot, &[1; 4]);
            store.write_snapshot().unwrap();

            // Verify SNAPSHOT_BASE was written.
            let flatkv_dir = store.flatkv_dir();
            let work_dir = working_dir_path(&flatkv_dir);
            assert!(
                reuse_working_dir(&work_dir, &snapshot_name(1)),
                "SNAPSHOT_BASE should match snapshot-1"
            );

            store.close().unwrap();
        }

        // Second session: re-open should reuse the working dir (no re-clone).
        {
            let mut store = CommitStore::new(db_dir, FlatKvConfig::default());

            // Write a marker file into the working dir.
            let flatkv_dir = store.flatkv_dir();
            let marker_path = working_dir_path(&flatkv_dir).join("test_marker");
            fs::write(&marker_path, b"marker").unwrap();

            store.load_version(0).unwrap();

            // If working dir was reused (not re-cloned), the marker should survive.
            assert!(
                marker_path.exists(),
                "marker file should survive when SNAPSHOT_BASE matches (working dir reused)"
            );

            store.close().unwrap();
        }
    }
}
