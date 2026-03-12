//! FlatKV snapshot writing (RocksDB Checkpoint) and pruning.
//!
//! Implements `write_snapshot`, `prune_snapshots`, and `try_truncate_wal` on
//! [`CommitStore`], ported from the Go reference in
//! `sei-db/state_db/sc/flatkv/snapshot.go`.

use crate::flatkv::{
    snapshot_dir::{
        atomic_remove_dir, snapshot_name, traverse_snapshots, update_current_symlink,
        working_dir_path, write_snapshot_base, ACCOUNT_DB_DIR, CODE_DB_DIR, LEGACY_DB_DIR,
        METADATA_DIR, STORAGE_DB_DIR,
    },
    store::CommitStore,
};
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::{kv::Checkpointable, wal::Wal};
use std::{fs, time::Instant};
use tracing::{error, info};

impl CommitStore {
    /// Creates a RocksDB checkpoint of the committed state.
    ///
    /// The snapshot is written into a versioned subdirectory under the flatkv
    /// root (e.g. `flatkv/snapshot-00000000000000000100`) and the `current`
    /// symlink is updated. On success, old snapshots are pruned and the WAL
    /// truncation point is updated.
    pub fn write_snapshot(&mut self) -> Result<()> {
        let version = self.committed_version;
        if version <= 0 {
            return Err(SeiDbError::Other(format!(
                "cannot snapshot uncommitted store (version {version})"
            )));
        }

        let dir = self.flatkv_dir();
        let snap_name = snapshot_name(version);
        let final_path = dir.join(&snap_name);
        let tmp_path = dir.join(format!("{snap_name}-tmp"));

        // Clean up any stale tmp dir from a prior crash.
        let _ = fs::remove_dir_all(&tmp_path);

        fs::create_dir_all(&tmp_path)
            .map_err(|e| SeiDbError::Other(format!("create snapshot tmp dir: {e}")))?;

        // Checkpoint each of the 5 DBs into the tmp directory.
        let result = self.checkpoint_all_dbs(&tmp_path);
        if result.is_err() {
            let _ = fs::remove_dir_all(&tmp_path);
            return result;
        }

        // Remove stale final dir if it exists (idempotent).
        let _ = atomic_remove_dir(&final_path);

        // Atomic rename tmp -> final.
        fs::rename(&tmp_path, &final_path)
            .map_err(|e| SeiDbError::Other(format!("rename snapshot dir: {e}")))?;

        // Update the `current` symlink to point to the new snapshot.
        update_current_symlink(&dir, &snap_name)?;

        // Keep SNAPSHOT_BASE in sync so the next restart reuses the working
        // dir instead of re-cloning from the snapshot.
        let work_dir = working_dir_path(&dir);
        if let Err(e) = write_snapshot_base(&work_dir, &snap_name) {
            error!(%e, "failed to update SNAPSHOT_BASE");
        }

        // Prune old snapshots (best-effort).
        self.prune_snapshots();

        self.last_snapshot_time = Some(Instant::now());
        info!(version, path = %final_path.display(), "FlatKV snapshot created");
        Ok(())
    }

    /// Checkpoint all 5 RocksDB instances into `tmp_dir`.
    fn checkpoint_all_dbs(&self, tmp_dir: &std::path::Path) -> Result<()> {
        let dbs: [(&str, Option<&dyn Checkpointable>); 5] = [
            (ACCOUNT_DB_DIR, self.account_db.as_ref().map(|db| db as &dyn Checkpointable)),
            (CODE_DB_DIR, self.code_db.as_ref().map(|db| db as &dyn Checkpointable)),
            (STORAGE_DB_DIR, self.storage_db.as_ref().map(|db| db as &dyn Checkpointable)),
            (LEGACY_DB_DIR, self.legacy_db.as_ref().map(|db| db as &dyn Checkpointable)),
            (METADATA_DIR, self.metadata_db.as_ref().map(|db| db as &dyn Checkpointable)),
        ];

        for (name, db_opt) in &dbs {
            let db = db_opt.ok_or_else(|| {
                SeiDbError::Other(format!("db {name} does not support Checkpoint (not open)"))
            })?;
            let dest = tmp_dir.join(name);
            db.checkpoint(&dest)
                .map_err(|e| SeiDbError::Other(format!("checkpoint {name}: {e}")))?;
        }
        Ok(())
    }

    /// Removes old snapshots beyond `snapshot_keep_recent`, keeping the latest
    /// snapshot plus the N most recent older ones.
    ///
    /// Best-effort: errors are logged but do not fail the operation.
    fn prune_snapshots(&self) {
        let keep = self.config.snapshot_keep_recent as usize;
        let dir = self.flatkv_dir();
        let current_version = self.committed_version;

        // Collect older snapshots (descending order, excluding current).
        let mut older: Vec<i64> = Vec::new();
        let _ = traverse_snapshots(&dir, false, |v| {
            if v != current_version {
                older.push(v);
            }
            Ok(true) // continue iterating
        });

        if older.len() <= keep {
            return;
        }

        // older is in descending order; skip `keep` most recent, remove the rest.
        for &v in &older[keep..] {
            let snap_path = dir.join(snapshot_name(v));
            match atomic_remove_dir(&snap_path) {
                Ok(()) => info!(version = v, "pruned old snapshot"),
                Err(e) => error!(version = v, %e, "prune snapshot failed"),
            }
        }
    }

    /// Best-effort truncation of WAL entries older than the earliest snapshot.
    ///
    /// Prevents unbounded WAL growth while keeping enough entries for rollback
    /// to any retained snapshot. Uses a simple arithmetic mapping where
    /// WAL offset ~ version (1:1 mapping).
    pub(crate) fn try_truncate_wal(&self) {
        let changelog = match &self.changelog {
            Some(c) => c,
            None => return,
        };

        let dir = self.flatkv_dir();

        // Find the earliest (lowest-version) snapshot.
        let mut earliest_snap_version: i64 = 0;
        let _ = traverse_snapshots(&dir, true, |v| {
            earliest_snap_version = v;
            Ok(false) // stop after first (ascending = lowest)
        });
        if earliest_snap_version <= 0 {
            return;
        }

        // Compute WAL offset for that version using the arithmetic shortcut:
        // In the common case each version maps 1:1 to a sequential WAL offset.
        let first_off = match changelog.first_offset() {
            Ok(f) => f,
            Err(_) => return,
        };
        let last_off = match changelog.last_offset() {
            Ok(l) => l,
            Err(_) => return,
        };
        if first_off == 0 && last_off == 0 {
            return;
        }

        // Read the first entry to determine version-to-offset mapping.
        let first_entry = match changelog.read_at(first_off) {
            Ok(e) => e,
            Err(_) => return,
        };
        let first_version = first_entry.version;
        if earliest_snap_version <= first_version {
            // Nothing to truncate.
            return;
        }

        // Arithmetic offset: version = first_version + (offset - first_off)
        // => offset = first_off + (version - first_version)
        let delta = (earliest_snap_version - first_version) as u64;
        let target_off = first_off + delta;
        if target_off <= first_off || target_off > last_off {
            return;
        }

        if let Err(e) = changelog.truncate_before(target_off) {
            error!(%e, truncate_offset = target_off, "failed to truncate WAL");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flatkv::snapshot_dir::{
        current_snapshot_dir, reuse_working_dir, SNAPSHOT_BASE_FILE, SNAPSHOT_DB_DIRS,
    };
    use seidb_common::config::FlatKvConfig;
    use seidb_engine::engine::RocksDbEngine;
    use seidb_traits::{kv::KvEngine, types::WriteOptions};
    use tempfile::TempDir;

    /// Helper: create an open CommitStore.
    fn open_store() -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), FlatKvConfig::default());
        store.load_version(0).unwrap();
        (store, dir)
    }

    /// Helper: create an open CommitStore with custom config.
    fn open_store_with_config(config: FlatKvConfig) -> (CommitStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let mut store = CommitStore::new(dir.path().to_str().unwrap(), config);
        store.load_version(0).unwrap();
        (store, dir)
    }

    #[test]
    fn test_write_snapshot_requires_committed() {
        let (mut store, _dir) = open_store();
        // Version is 0 (no commits), snapshot should fail.
        let result = store.write_snapshot();
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("uncommitted"), "expected uncommitted error, got: {msg}");
    }

    #[test]
    fn test_write_snapshot_creates_dir() {
        let (mut store, _dir) = open_store();
        // Commit once to get version 1.
        store.commit().unwrap();
        assert_eq!(store.version(), 1);

        store.write_snapshot().unwrap();

        // Snapshot directory should exist.
        let flatkv_dir = store.flatkv_dir();
        let snap_dir = flatkv_dir.join(snapshot_name(1));
        assert!(snap_dir.exists(), "snapshot dir should exist");

        // All 5 sub-DB dirs should exist within the snapshot.
        for &sub in SNAPSHOT_DB_DIRS {
            assert!(snap_dir.join(sub).exists(), "snapshot sub-dir {sub} should exist");
        }
    }

    #[test]
    fn test_write_snapshot_updates_symlink() {
        let (mut store, _dir) = open_store();
        store.commit().unwrap();
        store.write_snapshot().unwrap();

        let flatkv_dir = store.flatkv_dir();
        let (snap_dir, version) = current_snapshot_dir(&flatkv_dir).unwrap();
        assert_eq!(version, 1);
        assert_eq!(snap_dir, flatkv_dir.join(snapshot_name(1)));
    }

    #[test]
    fn test_write_snapshot_updates_snapshot_base() {
        let (mut store, _dir) = open_store();
        store.commit().unwrap();
        store.write_snapshot().unwrap();

        let flatkv_dir = store.flatkv_dir();
        let work_dir = working_dir_path(&flatkv_dir);
        let snap_name = snapshot_name(1);
        assert!(
            reuse_working_dir(&work_dir, &snap_name),
            "SNAPSHOT_BASE should point to the new snapshot"
        );

        // Also verify the file content directly.
        let base_content = fs::read_to_string(work_dir.join(SNAPSHOT_BASE_FILE)).unwrap();
        assert_eq!(base_content.trim(), snap_name);
    }

    #[test]
    fn test_prune_snapshots_keeps_recent() {
        let mut config = FlatKvConfig::default();
        config.snapshot_keep_recent = 1; // keep current + 1 old = 2 total
        let (mut store, _dir) = open_store_with_config(config);

        // Create 4 snapshots at versions 1, 2, 3, 4.
        for _ in 0..4 {
            store.commit().unwrap();
            store.write_snapshot().unwrap();
        }
        assert_eq!(store.version(), 4);

        let flatkv_dir = store.flatkv_dir();

        // Collect remaining snapshots.
        let mut remaining: Vec<i64> = Vec::new();
        traverse_snapshots(&flatkv_dir, false, |v| {
            remaining.push(v);
            Ok(true) // continue
        })
        .unwrap();

        // Should keep current (4) + 1 recent old (3) = 2 total.
        // Snapshot 0 (initial) + snapshots 1, 2 should be pruned.
        assert_eq!(remaining.len(), 2, "expected 2 snapshots remaining, got {remaining:?}");
        assert!(remaining.contains(&4), "current snapshot should remain");
        assert!(remaining.contains(&3), "most recent old snapshot should remain");
    }

    #[test]
    fn test_snapshot_preserves_data() {
        let (mut store, _dir) = open_store();

        // Write data to each DB, then commit.
        let wo = WriteOptions::default();
        store.account_db.as_ref().unwrap().set(b"acct_key", b"acct_val", &wo).unwrap();
        store.code_db.as_ref().unwrap().set(b"code_key", b"code_val", &wo).unwrap();
        store.storage_db.as_ref().unwrap().set(b"stor_key", b"stor_val", &wo).unwrap();
        store.legacy_db.as_ref().unwrap().set(b"leg_key", b"leg_val", &wo).unwrap();
        store.metadata_db.as_ref().unwrap().set(b"meta_key", b"meta_val", &wo).unwrap();

        store.commit().unwrap();
        store.write_snapshot().unwrap();

        // Open the checkpoint DBs and verify data.
        let flatkv_dir = store.flatkv_dir();
        let snap_dir = flatkv_dir.join(snapshot_name(1));

        let cases: &[(&str, &[u8], &[u8])] = &[
            (ACCOUNT_DB_DIR, b"acct_key", b"acct_val"),
            (CODE_DB_DIR, b"code_key", b"code_val"),
            (STORAGE_DB_DIR, b"stor_key", b"stor_val"),
            (LEGACY_DB_DIR, b"leg_key", b"leg_val"),
            (METADATA_DIR, b"meta_key", b"meta_val"),
        ];

        for &(db_dir, key, expected_val) in cases {
            let db_path = snap_dir.join(db_dir);
            let db = RocksDbEngine::open_plain(&db_path).unwrap();
            let val = db
                .get(key)
                .unwrap()
                .unwrap_or_else(|| panic!("{db_dir}: key not found in checkpoint"));
            assert_eq!(val, expected_val, "{db_dir}: value mismatch in checkpoint");
        }
    }

    #[test]
    fn test_write_snapshot_sets_last_snapshot_time() {
        let (mut store, _dir) = open_store();
        assert!(store.last_snapshot_time.is_none());

        store.commit().unwrap();
        store.write_snapshot().unwrap();

        assert!(store.last_snapshot_time.is_some());
    }

    #[test]
    fn test_multiple_snapshots() {
        let (mut store, _dir) = open_store();

        store.commit().unwrap();
        store.write_snapshot().unwrap();

        store.commit().unwrap();
        store.write_snapshot().unwrap();

        let flatkv_dir = store.flatkv_dir();
        let (_, version) = current_snapshot_dir(&flatkv_dir).unwrap();
        assert_eq!(version, 2, "current symlink should point to latest snapshot");
    }
}
