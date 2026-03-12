//! FlatKV-specific snapshot directory management.
//!
//! This module handles FlatKV-specific operations on top of the common
//! snapshot directory utilities in [`seidb_common::snapshot_dir`]. It manages
//! the working directory (mutable clone of a snapshot), the `SNAPSHOT_BASE`
//! file that records which snapshot the working dir was cloned from, and
//! migration from the legacy flat layout to the versioned snapshot layout.

use std::{
    fs,
    path::{Path, PathBuf},
};

use seidb_common::error::{Result, SeiDbError};

// Re-export common snapshot directory functions so callers can use a single import path.
pub use seidb_common::snapshot_dir::{
    atomic_remove_dir, clone_dir, copy_file, current_path, current_tmp_path, is_snapshot_name,
    parse_snapshot_version, remove_tmp_dirs, seek_snapshot, snapshot_name, traverse_snapshots,
    update_current_symlink,
};

/// Root directory name for the FlatKV store.
pub const FLATKV_ROOT_DIR: &str = "flatkv";

/// Sub-directory for changelog (WAL, shared across snapshots).
pub const CHANGELOG_DIR: &str = "changelog";

/// Sub-directory for account data (PebbleDB: addr -> AccountValue).
pub const ACCOUNT_DB_DIR: &str = "account";

/// Sub-directory for contract bytecode (PebbleDB: addr -> bytecode).
pub const CODE_DB_DIR: &str = "code";

/// Sub-directory for storage slots (PebbleDB: addr||slot -> value).
pub const STORAGE_DB_DIR: &str = "storage";

/// Sub-directory for legacy key-value data (PebbleDB: full key -> value).
pub const LEGACY_DB_DIR: &str = "legacy";

/// Sub-directory for metadata (PebbleDB: version + LtHash).
pub const METADATA_DIR: &str = "metadata";

/// Name of the mutable working directory cloned from the active snapshot.
pub const WORKING_DIR_NAME: &str = "working";

/// File within the working directory recording which snapshot it was cloned from.
pub const SNAPSHOT_BASE_FILE: &str = "SNAPSHOT_BASE";

/// Lock file name skipped during clone operations.
pub const LOCK_FILE_NAME: &str = "LOCK";

/// Metadata key for the global committed version watermark (8 bytes, big-endian).
pub const META_GLOBAL_VERSION: &str = "_meta/version";

/// All 5 sub-DB directory names within a snapshot.
pub const SNAPSHOT_DB_DIRS: &[&str] =
    &[ACCOUNT_DB_DIR, CODE_DB_DIR, STORAGE_DB_DIR, LEGACY_DB_DIR, METADATA_DIR];

/// Read the `current` symlink under `root` and return the full path to the
/// snapshot directory along with the parsed version number.
///
/// Returns an I/O error (with `NotFound` kind) if the symlink does not exist.
pub fn current_snapshot_dir(root: &Path) -> Result<(PathBuf, i64)> {
    let target = fs::read_link(current_path(root))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| target.to_str().unwrap_or(""));
    let version = parse_snapshot_version(name)?;
    Ok((root.join(name), version))
}

/// Ensure a mutable working directory exists, cloned from `snap_dir`.
///
/// If the working dir already exists and was cloned from the same snapshot
/// (recorded in `SNAPSHOT_BASE`), the expensive re-clone is skipped because
/// WAL catchup is idempotent and will bring the data up to date.
///
/// During cloning, `LOCK` files are skipped as they belong to the running
/// database engine and must not be copied.
pub fn create_working_dir(snap_dir: &Path, work_dir: &Path) -> Result<()> {
    let snap_base = snap_dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

    if reuse_working_dir(work_dir, &snap_base) {
        return Ok(());
    }

    // Remove stale working directory if it exists.
    if work_dir.exists() {
        fs::remove_dir_all(work_dir)?;
    }

    fs::create_dir_all(work_dir)?;

    for &sub in SNAPSHOT_DB_DIRS {
        let src_path = snap_dir.join(sub);
        let dst_path = work_dir.join(sub);

        if !src_path.exists() {
            // Source sub-DB doesn't exist — create an empty dir so the engine
            // can open it later.
            fs::create_dir_all(&dst_path)?;
            continue;
        }

        clone_dir_skip_lock(&src_path, &dst_path)?;
    }

    write_snapshot_base(work_dir, &snap_base)
}

/// Clone a directory tree, skipping `LOCK` files. Immutable `.sst` files are
/// hard-linked; everything else is byte-copied.
fn clone_dir_skip_lock(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ft.is_dir() {
            clone_dir_skip_lock(&src_path, &dst_path)?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();

        // Skip LOCK files — they belong to the running DB engine.
        if *name == *LOCK_FILE_NAME {
            continue;
        }

        if name.ends_with(".sst") && fs::hard_link(&src_path, &dst_path).is_ok() {
            continue;
        }

        copy_file(&src_path, &dst_path)?;
    }
    Ok(())
}

/// Returns `true` if `work_dir` exists and was cloned from a snapshot whose
/// name matches `snap_name`, meaning a full re-clone can be skipped.
pub fn reuse_working_dir(work_dir: &Path, snap_name: &str) -> bool {
    let base_path = work_dir.join(SNAPSHOT_BASE_FILE);
    match fs::read_to_string(&base_path) {
        Ok(content) => content.trim() == snap_name,
        Err(_) => false,
    }
}

/// Write the snapshot name into `work_dir/SNAPSHOT_BASE` so that
/// [`reuse_working_dir`] can detect whether the working dir is still valid.
pub fn write_snapshot_base(work_dir: &Path, snap_name: &str) -> Result<()> {
    let path = work_dir.join(SNAPSHOT_BASE_FILE);
    fs::write(&path, format!("{snap_name}\n"))?;
    Ok(())
}

/// Resolve the active snapshot directory for FlatKV.
///
/// Handles four cases:
/// 1. `current` symlink exists — return the symlink target.
/// 2. Working dir exists with `SNAPSHOT_BASE` — return working dir (reuse).
/// 3. Flat layout (sub-DB dirs exist directly under `flatkv_dir`) — migrate to snapshot layout via
///    [`migrate_flat_layout`].
/// 4. Fresh start — create an initial `snapshot-0` directory with empty sub-DB dirs, set up the
///    `current` symlink, and return it.
pub fn resolve_snapshot_dir(flatkv_dir: &Path) -> Result<PathBuf> {
    // Case 1: current symlink exists.
    match current_snapshot_dir(flatkv_dir) {
        Ok((snap_dir, _)) => return Ok(snap_dir),
        Err(SeiDbError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            // Symlink doesn't exist — fall through to other cases.
        }
        Err(e) => {
            return Err(SeiDbError::Other(format!("read current symlink: {e}")));
        }
    }

    // Case 2: check for orphaned working dir with SNAPSHOT_BASE.
    // (This is actually a sub-case handled after checking flat layout.)

    // Case 3: check for flat layout — sub-DB dirs exist directly under flatkv_dir.
    let has_flat_dirs = SNAPSHOT_DB_DIRS.iter().any(|sub| flatkv_dir.join(sub).exists());
    if has_flat_dirs {
        return migrate_flat_layout(flatkv_dir);
    }

    // Check for orphaned snapshot directory (migration crashed after moving
    // dirs but before creating symlink).
    let mut latest_snap: Option<i64> = None;
    traverse_snapshots(flatkv_dir, false, |v| {
        latest_snap = Some(v);
        Ok(false) // stop after first (highest)
    })?;
    if let Some(v) = latest_snap {
        let snap_name = snapshot_name(v);
        update_current_symlink(flatkv_dir, &snap_name)?;
        return Ok(flatkv_dir.join(snap_name));
    }

    // Case 4: fresh start — create initial snapshot-0.
    let init_snap = snapshot_name(0);
    let init_dir = flatkv_dir.join(&init_snap);
    for &sub in SNAPSHOT_DB_DIRS {
        fs::create_dir_all(init_dir.join(sub))?;
    }
    update_current_symlink(flatkv_dir, &init_snap)?;
    Ok(init_dir)
}

/// Migrate a legacy flat layout (sub-DB dirs directly under `flatkv_dir`)
/// into a versioned snapshot directory.
///
/// Opens the metadata DB to read the committed version, creates a
/// `snapshot-{version}` directory, and moves each sub-DB into it. The
/// function is idempotent: directories already moved by a prior partial
/// attempt are skipped.
pub fn migrate_flat_layout(flatkv_dir: &Path) -> Result<PathBuf> {
    // Determine the version from the metadata DB.
    let version = read_version_from_metadata(flatkv_dir);

    let snap_name = snapshot_name(version);
    let snap_dir = flatkv_dir.join(&snap_name);
    fs::create_dir_all(&snap_dir)?;

    for &sub in SNAPSHOT_DB_DIRS {
        let src = flatkv_dir.join(sub);
        let dst = snap_dir.join(sub);
        if !src.exists() {
            continue;
        }
        fs::rename(&src, &dst).map_err(|e| {
            SeiDbError::Other(format!(
                "migration: move {} -> {}: {e}",
                src.display(),
                dst.display()
            ))
        })?;
    }

    update_current_symlink(flatkv_dir, &snap_name)?;
    Ok(snap_dir)
}

/// Try to read the global version from the metadata DB at `flatkv_dir/metadata`.
///
/// Opens the metadata directory as a RocksDB, reads `META_GLOBAL_VERSION`,
/// and interprets the 8-byte big-endian value as `i64`. Returns `0` on any
/// failure (missing DB, missing key, wrong length, etc.).
fn read_version_from_metadata(flatkv_dir: &Path) -> i64 {
    let meta_path = flatkv_dir.join(METADATA_DIR);
    // Try to open the metadata DB. If it doesn't exist or fails, check
    // for an existing snapshot from a prior partial migration.
    let db: seidb_engine::engine::RocksDbEngine =
        match seidb_engine::engine::RocksDbEngine::open_plain(&meta_path) {
            Ok(db) => db,
            Err(_) => {
                // Metadata already moved — look for snapshot dir from a prior attempt.
                let mut version = 0i64;
                let _ = traverse_snapshots(flatkv_dir, false, |v| {
                    version = v;
                    Ok(false) // stop after first (highest)
                });
                return version;
            }
        };

    use seidb_traits::kv::KvEngine;
    match db.get(META_GLOBAL_VERSION.as_bytes()) {
        Ok(Some(data)) if data.len() == 8 => i64::from_be_bytes(data[..8].try_into().unwrap()),
        _ => 0,
    }
}

/// Returns the path to the working directory: `flatkv_dir/working`.
pub fn working_dir_path(flatkv_dir: &Path) -> PathBuf {
    flatkv_dir.join(WORKING_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn test_snapshot_name_roundtrip() {
        let name = snapshot_name(42);
        assert!(is_snapshot_name(&name));
        assert_eq!(parse_snapshot_version(&name).unwrap(), 42);

        let name_zero = snapshot_name(0);
        assert!(is_snapshot_name(&name_zero));
        assert_eq!(parse_snapshot_version(&name_zero).unwrap(), 0);
    }

    #[test]
    fn test_current_snapshot_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let snap = snapshot_name(100);
        fs::create_dir(root.join(&snap)).unwrap();
        update_current_symlink(root, &snap).unwrap();

        let (dir, version) = current_snapshot_dir(root).unwrap();
        assert_eq!(version, 100);
        assert_eq!(dir, root.join(&snap));
    }

    #[test]
    fn test_create_working_dir_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a snapshot directory with some sub-DB dirs and files.
        let snap_name = snapshot_name(10);
        let snap_dir = root.join(&snap_name);
        for &sub in SNAPSHOT_DB_DIRS {
            fs::create_dir_all(snap_dir.join(sub)).unwrap();
        }
        // Add a file to account DB.
        let mut f = fs::File::create(snap_dir.join(ACCOUNT_DB_DIR).join("000001.sst")).unwrap();
        f.write_all(b"sst-data").unwrap();
        // Add a LOCK file that should be skipped.
        fs::write(snap_dir.join(ACCOUNT_DB_DIR).join(LOCK_FILE_NAME), b"lock").unwrap();

        let work_dir = root.join(WORKING_DIR_NAME);
        create_working_dir(&snap_dir, &work_dir).unwrap();

        // Working dir should exist with all sub-DB dirs.
        for &sub in SNAPSHOT_DB_DIRS {
            assert!(work_dir.join(sub).is_dir());
        }
        // SST file should be cloned (hard-linked).
        assert!(work_dir.join(ACCOUNT_DB_DIR).join("000001.sst").exists());
        // LOCK file should NOT be cloned.
        assert!(!work_dir.join(ACCOUNT_DB_DIR).join(LOCK_FILE_NAME).exists());
        // SNAPSHOT_BASE should record the source snapshot.
        assert!(reuse_working_dir(&work_dir, &snap_name));
    }

    #[test]
    fn test_create_working_dir_reuse() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let snap_name = snapshot_name(20);
        let snap_dir = root.join(&snap_name);
        for &sub in SNAPSHOT_DB_DIRS {
            fs::create_dir_all(snap_dir.join(sub)).unwrap();
        }

        let work_dir = root.join(WORKING_DIR_NAME);

        // First clone.
        create_working_dir(&snap_dir, &work_dir).unwrap();
        // Add a marker file to detect if re-clone happens.
        fs::write(work_dir.join("marker.txt"), b"should-survive").unwrap();

        // Second call with same snapshot — should skip re-clone.
        create_working_dir(&snap_dir, &work_dir).unwrap();
        assert!(
            work_dir.join("marker.txt").exists(),
            "marker file should survive when SNAPSHOT_BASE matches"
        );
    }

    #[test]
    fn test_create_working_dir_reclone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create two snapshot directories.
        let snap1_name = snapshot_name(10);
        let snap1_dir = root.join(&snap1_name);
        let snap2_name = snapshot_name(20);
        let snap2_dir = root.join(&snap2_name);
        for &sub in SNAPSHOT_DB_DIRS {
            fs::create_dir_all(snap1_dir.join(sub)).unwrap();
            fs::create_dir_all(snap2_dir.join(sub)).unwrap();
        }

        let work_dir = root.join(WORKING_DIR_NAME);

        // Clone from snap1.
        create_working_dir(&snap1_dir, &work_dir).unwrap();
        fs::write(work_dir.join("marker.txt"), b"from-snap1").unwrap();
        assert!(reuse_working_dir(&work_dir, &snap1_name));

        // Clone from snap2 — should trigger re-clone (SNAPSHOT_BASE mismatch).
        create_working_dir(&snap2_dir, &work_dir).unwrap();
        assert!(!work_dir.join("marker.txt").exists(), "marker file should be gone after re-clone");
        assert!(reuse_working_dir(&work_dir, &snap2_name));
    }

    #[test]
    fn test_write_read_snapshot_base() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path();

        let snap_name = snapshot_name(55);
        write_snapshot_base(work_dir, &snap_name).unwrap();

        // Read back and verify.
        let content = fs::read_to_string(work_dir.join(SNAPSHOT_BASE_FILE)).unwrap();
        assert_eq!(content.trim(), snap_name);
        assert!(reuse_working_dir(work_dir, &snap_name));
        assert!(!reuse_working_dir(work_dir, "snapshot-other"));
    }

    #[test]
    fn test_resolve_snapshot_dir_with_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create a snapshot and symlink.
        let snap = snapshot_name(50);
        let snap_dir = root.join(&snap);
        for &sub in SNAPSHOT_DB_DIRS {
            fs::create_dir_all(snap_dir.join(sub)).unwrap();
        }
        update_current_symlink(root, &snap).unwrap();

        let resolved = resolve_snapshot_dir(root).unwrap();
        assert_eq!(resolved, snap_dir);
    }

    #[test]
    fn test_resolve_snapshot_dir_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Empty directory — fresh start.
        let resolved = resolve_snapshot_dir(root).unwrap();
        let expected = root.join(snapshot_name(0));
        assert_eq!(resolved, expected);

        // All sub-DB dirs should have been created.
        for &sub in SNAPSHOT_DB_DIRS {
            assert!(expected.join(sub).is_dir());
        }

        // current symlink should exist and point to snapshot-0.
        let (dir, version) = current_snapshot_dir(root).unwrap();
        assert_eq!(version, 0);
        assert_eq!(dir, expected);
    }

    #[test]
    fn test_working_dir_path() {
        let root = Path::new("/data/flatkv");
        assert_eq!(working_dir_path(root), PathBuf::from("/data/flatkv/working"));
    }
}
