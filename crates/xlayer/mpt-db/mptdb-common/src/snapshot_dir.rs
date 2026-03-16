//! Shared utilities for snapshot directory management.
//!
//! Both FlatKV and MemIAVL use a common on-disk layout where versioned
//! snapshots live under a root directory and a `current` symlink points
//! to the active snapshot. This module provides the low-level helpers
//! for creating, discovering, and removing those snapshot directories.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::error::{MptDbError, Result};

/// Directory name prefix for versioned snapshots.
pub const SNAPSHOT_PREFIX: &str = "snapshot-";

/// Full directory name length: `"snapshot-"` (9 chars) + 20-digit zero-padded version.
pub const SNAPSHOT_DIR_LEN: usize = 29;

/// Symlink name pointing to the active snapshot directory.
pub const CURRENT_LINK: &str = "current";

/// Temporary symlink used during atomic swap of [`CURRENT_LINK`].
pub const CURRENT_TMP_LINK: &str = "current-tmp";

/// Suffix appended during [`atomic_remove_dir`] before `remove_dir_all`.
const REMOVING_SUFFIX: &str = "-removing";

/// Suffix for temporary directories created during snapshot writes.
const TMP_SUFFIX: &str = "-tmp";

/// Format a snapshot directory name: `"snapshot-{version:020}"`.
pub fn snapshot_name(version: i64) -> String {
    format!("{}{:020}", SNAPSHOT_PREFIX, version)
}

/// Returns `true` if `name` matches the snapshot directory naming convention.
pub fn is_snapshot_name(name: &str) -> bool {
    name.len() == SNAPSHOT_DIR_LEN &&
        name.starts_with(SNAPSHOT_PREFIX) &&
        name[SNAPSHOT_PREFIX.len()..].bytes().all(|b| b.is_ascii_digit())
}

/// Parse the version number from a snapshot directory name.
pub fn parse_snapshot_version(name: &str) -> Result<i64> {
    if !is_snapshot_name(name) {
        return Err(MptDbError::Other(format!("invalid snapshot name: {name}")));
    }
    name[SNAPSHOT_PREFIX.len()..]
        .parse::<i64>()
        .map_err(|e| MptDbError::Other(format!("parse snapshot version {name:?}: {e}")))
}

/// Returns `root/current`.
pub fn current_path(root: &Path) -> PathBuf {
    root.join(CURRENT_LINK)
}

/// Returns `root/current-tmp`.
pub fn current_tmp_path(root: &Path) -> PathBuf {
    root.join(CURRENT_TMP_LINK)
}

/// Reads the `current` symlink under `root` and parses the snapshot version.
pub fn current_version(root: &Path) -> Result<i64> {
    let target = fs::read_link(current_path(root))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| target.to_str().unwrap_or(""));
    parse_snapshot_version(name)
}

/// Atomically updates the `current` symlink to point at `snapshot_dir`.
///
/// `snapshot_dir` should be the bare directory name (e.g.
/// `"snapshot-00000000000000000100"`), not a full path.
///
/// Uses create-tmp-symlink + rename for atomicity on POSIX systems.
pub fn update_current_symlink(root: &Path, snapshot_dir: &str) -> Result<()> {
    let tmp = current_tmp_path(root);

    // Remove stale tmp symlink if present.
    if tmp.symlink_metadata().is_ok() {
        fs::remove_file(&tmp)?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(snapshot_dir, &tmp)?;
    #[cfg(not(unix))]
    {
        // Fallback for non-unix (best-effort).
        std::fs::soft_link(snapshot_dir, &tmp)?;
    }

    fs::rename(&tmp, current_path(root)).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        MptDbError::Io(e)
    })
}

/// Find the largest snapshot version that is `<= target_version`.
///
/// Returns `Ok(None)` if no qualifying snapshot exists.
pub fn seek_snapshot(root: &Path, target_version: i64) -> Result<Option<i64>> {
    let mut found: Option<i64> = None;
    traverse_snapshots(root, false, |version| {
        if version <= target_version {
            found = Some(version);
            return Ok(false); // stop
        }
        Ok(true) // continue
    })?;
    Ok(found)
}

/// Iterate snapshot directories under `dir` in sorted order.
///
/// - `ascending = true`  -> lowest version first
/// - `ascending = false` -> highest version first
///
/// The callback returns `Ok(true)` to continue or `Ok(false)` to stop early.
pub fn traverse_snapshots(
    dir: &Path,
    ascending: bool,
    mut f: impl FnMut(i64) -> Result<bool>,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    let mut versions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_snapshot_name(&name) {
            continue;
        }
        // Only consider directories (or symlinks to directories).
        let ft = entry.file_type()?;
        if !ft.is_dir() && !ft.is_symlink() {
            continue;
        }
        if let Ok(v) = parse_snapshot_version(&name) {
            versions.push(v);
        }
    }

    if ascending {
        versions.sort_unstable();
    } else {
        versions.sort_unstable_by(|a, b| b.cmp(a));
    }

    for v in versions {
        if !f(v)? {
            break;
        }
    }
    Ok(())
}

/// Rename a directory to `path + "-removing"` then `remove_dir_all`, preventing
/// half-deleted snapshots on crash.
pub fn atomic_remove_dir(path: &Path) -> Result<()> {
    let trash = path.with_file_name(format!(
        "{}{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        REMOVING_SUFFIX
    ));
    // Remove any pre-existing trash directory.
    let _ = fs::remove_dir_all(&trash);
    fs::rename(path, &trash)?;
    fs::remove_dir_all(&trash)?;
    Ok(())
}

/// Remove directories ending with `"-tmp"` or `"-removing"` left over from
/// interrupted snapshot writes or deletes.
pub fn remove_tmp_dirs(dir: &Path) -> Result<()> {
    let entries = fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ft = entry.file_type()?;
        if ft.is_dir() && (name.ends_with(TMP_SUFFIX) || name.ends_with(REMOVING_SUFFIX)) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

/// Copy a directory tree. Immutable `.sst` files are hard-linked; everything
/// else is byte-copied. Subdirectories are handled recursively.
pub fn clone_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ft.is_dir() {
            clone_dir(&src_path, &dst_path)?;
            continue;
        }

        if !ft.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();

        if name.ends_with(".sst") {
            // Try hard link first; fall back to copy on failure (e.g. cross-device).
            if fs::hard_link(&src_path, &dst_path).is_ok() {
                continue;
            }
        }

        copy_file(&src_path, &dst_path)?;
    }
    Ok(())
}

/// Copy a single file and fsync the destination.
pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst)?;
    // fsync the destination to ensure durability.
    let f = fs::File::open(dst)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_snapshot_name_format() {
        assert_eq!(snapshot_name(0), "snapshot-00000000000000000000");
        assert_eq!(snapshot_name(1), "snapshot-00000000000000000001");
        assert_eq!(snapshot_name(100), "snapshot-00000000000000000100");
        assert_eq!(snapshot_name(i64::MAX), "snapshot-09223372036854775807");
        // Verify 20-digit zero-padding.
        let name = snapshot_name(42);
        assert_eq!(name.len(), SNAPSHOT_DIR_LEN);
    }

    #[test]
    fn test_is_snapshot_name() {
        assert!(is_snapshot_name("snapshot-00000000000000000000"));
        assert!(is_snapshot_name("snapshot-00000000000000000100"));
        assert!(is_snapshot_name("snapshot-99999999999999999999"));

        // Too short.
        assert!(!is_snapshot_name("snapshot-123"));
        // Wrong prefix.
        assert!(!is_snapshot_name("snapshotx00000000000000000000"));
        // Non-digit after prefix.
        assert!(!is_snapshot_name("snapshot-0000000000000000000a"));
        // Too long.
        assert!(!is_snapshot_name("snapshot-000000000000000000001"));
        // Empty.
        assert!(!is_snapshot_name(""));
    }

    #[test]
    fn test_parse_snapshot_version() {
        assert_eq!(parse_snapshot_version("snapshot-00000000000000000000").unwrap(), 0);
        assert_eq!(parse_snapshot_version("snapshot-00000000000000000100").unwrap(), 100);
        assert_eq!(parse_snapshot_version("snapshot-00000000000000012345").unwrap(), 12345);

        // Error cases.
        assert!(parse_snapshot_version("bad-name").is_err());
        assert!(parse_snapshot_version("snapshot-123").is_err());
    }

    #[test]
    fn test_update_current_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let snap1 = snapshot_name(10);
        fs::create_dir(root.join(&snap1)).unwrap();
        update_current_symlink(root, &snap1).unwrap();
        assert_eq!(current_version(root).unwrap(), 10);

        // Update to a new snapshot.
        let snap2 = snapshot_name(20);
        fs::create_dir(root.join(&snap2)).unwrap();
        update_current_symlink(root, &snap2).unwrap();
        assert_eq!(current_version(root).unwrap(), 20);
    }

    #[test]
    fn test_traverse_snapshots_ascending() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        for v in [30, 10, 20] {
            fs::create_dir(root.join(snapshot_name(v))).unwrap();
        }
        // Also create a non-snapshot dir to verify it's skipped.
        fs::create_dir(root.join("other-dir")).unwrap();

        let mut collected = Vec::new();
        traverse_snapshots(root, true, |v| {
            collected.push(v);
            Ok(true)
        })
        .unwrap();
        assert_eq!(collected, vec![10, 20, 30]);
    }

    #[test]
    fn test_traverse_snapshots_descending() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        for v in [30, 10, 20] {
            fs::create_dir(root.join(snapshot_name(v))).unwrap();
        }

        let mut collected = Vec::new();
        traverse_snapshots(root, false, |v| {
            collected.push(v);
            Ok(true)
        })
        .unwrap();
        assert_eq!(collected, vec![30, 20, 10]);
    }

    #[test]
    fn test_seek_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        for v in [10, 20, 30] {
            fs::create_dir(root.join(snapshot_name(v))).unwrap();
        }

        // Exact match.
        assert_eq!(seek_snapshot(root, 20).unwrap(), Some(20));
        // Between snapshots — picks the lower one.
        assert_eq!(seek_snapshot(root, 25).unwrap(), Some(20));
        // Above all snapshots.
        assert_eq!(seek_snapshot(root, 100).unwrap(), Some(30));
        // Below all snapshots — not found.
        assert_eq!(seek_snapshot(root, 5).unwrap(), None);
    }

    #[test]
    fn test_atomic_remove_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("to-remove");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file.txt"), b"data").unwrap();

        atomic_remove_dir(&dir).unwrap();
        assert!(!dir.exists());
        // The "-removing" trash should also be gone.
        assert!(!tmp.path().join("to-remove-removing").exists());
    }

    #[test]
    fn test_remove_tmp_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create dirs that should be removed.
        fs::create_dir(root.join("snapshot-123-tmp")).unwrap();
        fs::create_dir(root.join("snapshot-456-removing")).unwrap();
        // Create dirs that should be kept.
        fs::create_dir(root.join("snapshot-00000000000000000010")).unwrap();
        fs::create_dir(root.join("working")).unwrap();

        remove_tmp_dirs(root).unwrap();

        assert!(!root.join("snapshot-123-tmp").exists());
        assert!(!root.join("snapshot-456-removing").exists());
        assert!(root.join("snapshot-00000000000000000010").exists());
        assert!(root.join("working").exists());
    }

    #[test]
    fn test_clone_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");

        // Create source structure.
        fs::create_dir_all(src.join("subdir")).unwrap();

        let mut sst = fs::File::create(src.join("data.sst")).unwrap();
        sst.write_all(b"sst-contents").unwrap();

        let mut txt = fs::File::create(src.join("manifest.txt")).unwrap();
        txt.write_all(b"manifest-data").unwrap();

        let mut sub_file = fs::File::create(src.join("subdir").join("nested.txt")).unwrap();
        sub_file.write_all(b"nested").unwrap();

        clone_dir(&src, &dst).unwrap();

        // Verify .sst was hard-linked (same inode on unix).
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let src_ino = fs::metadata(src.join("data.sst")).unwrap().ino();
            let dst_ino = fs::metadata(dst.join("data.sst")).unwrap().ino();
            assert_eq!(src_ino, dst_ino, ".sst file should be hard-linked");
        }

        // Verify non-sst file was copied (content matches).
        assert_eq!(fs::read_to_string(dst.join("manifest.txt")).unwrap(), "manifest-data");
        // Verify recursive subdir copy.
        assert_eq!(fs::read_to_string(dst.join("subdir").join("nested.txt")).unwrap(), "nested");
    }

    #[test]
    fn test_update_current_symlink_concurrent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // Create snapshot directories upfront.
        for i in 0..10 {
            fs::create_dir(root.join(snapshot_name(i))).unwrap();
        }

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let snap = snapshot_name(i);
                    // Concurrent symlink updates — some may fail due to tmp
                    // file conflicts, which is the expected behavior.
                    let _ = update_current_symlink(&root, &snap);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // After all threads finish, symlink should point to a valid snapshot.
        let version = current_version(&root).unwrap();
        assert!((0..10).contains(&version));
    }
}
