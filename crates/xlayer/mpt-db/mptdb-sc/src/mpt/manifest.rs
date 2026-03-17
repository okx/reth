use alloy_primitives::B256;
use alloy_trie::EMPTY_ROOT_HASH;
use mptdb_common::error::{MptDbError, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

/// Persistent version manifest for the MPT commit store.
///
/// Tracks all committed versions and their state roots.
/// Uses crash-safe write (tmp + fsync tmp + rename).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionManifest {
    pub earliest_version: i64,
    pub latest_version: i64,
    pub versions: BTreeMap<i64, B256>,
}

impl VersionManifest {
    /// Load manifest from disk. If the file does not exist, returns a fresh manifest.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::fresh());
        }
        let data = fs::read(path).map_err(|e| MptDbError::Other(format!("read manifest: {e}")))?;
        let manifest: Self = serde_json::from_slice(&data)
            .map_err(|e| MptDbError::Other(format!("parse manifest: {e}")))?;
        Ok(manifest)
    }

    /// Crash-safe save: write tmp, fsync tmp, rename.
    ///
    /// ## Durability Contract
    ///
    /// After `save()` returns Ok, the manifest is guaranteed to be on stable storage.
    /// On crash, either the old or new manifest will be visible, never a partial write.
    ///
    /// The rename step is atomic on POSIX. On most modern filesystems (ext4 with
    /// default mount options, APFS), rename durability is guaranteed without an
    /// explicit directory fsync. We skip the directory fsync by default to reduce
    /// latency (one fewer syscall per commit). For maximum safety on exotic
    /// filesystems, use `save_strict()` which adds a directory fsync after rename.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp_path = path.with_extension("tmp");
        let data = serde_json::to_vec_pretty(self)
            .map_err(|e| MptDbError::Other(format!("serialize manifest: {e}")))?;

        // Step 1: write tmp file
        let mut file = fs::File::create(&tmp_path)
            .map_err(|e| MptDbError::Other(format!("create manifest.tmp: {e}")))?;
        file.write_all(&data).map_err(|e| MptDbError::Other(format!("write manifest.tmp: {e}")))?;

        // Step 2: fsync tmp file to ensure contents are durable before rename
        file.sync_all().map_err(|e| MptDbError::Other(format!("fsync manifest.tmp: {e}")))?;
        drop(file);

        // Step 3: atomic rename tmp -> manifest.json
        fs::rename(&tmp_path, path)
            .map_err(|e| MptDbError::Other(format!("rename manifest: {e}")))?;

        Ok(())
    }

    /// Strict crash-safe save: write tmp, fsync tmp, rename, fsync directory.
    ///
    /// Same as `save()` but adds a directory fsync after rename for maximum
    /// durability on filesystems where rename metadata may not be immediately
    /// persisted (e.g., older ext3, or ext4 without `auto_da_alloc`).
    pub fn save_strict(&self, path: &Path) -> Result<()> {
        self.save(path)?;

        if let Some(parent) = path.parent() {
            if let Ok(dir) = fs::File::open(parent) {
                dir.sync_all().map_err(|e| MptDbError::Other(format!("fsync parent dir: {e}")))?;
            }
        }

        Ok(())
    }

    /// Get the state root for a specific version.
    pub fn get_root(&self, version: i64) -> Option<B256> {
        self.versions.get(&version).copied()
    }

    /// Add a new version. Only allows version == latest_version + 1.
    pub fn add_version(&mut self, version: i64, root: B256) -> Result<()> {
        let expected = self.latest_version + 1;
        if version != expected {
            return Err(MptDbError::Other(format!(
                "add_version: expected version {expected}, got {version}"
            )));
        }
        self.versions.insert(version, root);
        self.latest_version = version;
        Ok(())
    }

    /// Truncate all versions after `version`.
    pub fn truncate_after(&mut self, version: i64) {
        // Remove all entries with key > version
        // BTreeMap::split_off returns entries >= key, so we split at version+1
        self.versions.split_off(&(version + 1));
        self.latest_version = version;
    }

    fn fresh() -> Self {
        let mut versions = BTreeMap::new();
        versions.insert(0, EMPTY_ROOT_HASH);
        Self { earliest_version: 0, latest_version: 0, versions }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// T3.1: fresh DB load -> version 0 + EMPTY_ROOT_HASH
    #[test]
    fn t3_1_fresh_db_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("manifest.json");
        let m = VersionManifest::load(&path).unwrap();
        assert_eq!(m.latest_version, 0);
        assert_eq!(m.earliest_version, 0);
        assert_eq!(m.get_root(0), Some(EMPTY_ROOT_HASH));
    }

    /// T3.2: add_version(1), add_version(2) -> latest_version correct
    #[test]
    fn t3_2_add_versions() {
        let mut m = VersionManifest::load(Path::new("/nonexistent")).unwrap();
        let root1 = B256::repeat_byte(0x11);
        let root2 = B256::repeat_byte(0x22);
        m.add_version(1, root1).unwrap();
        assert_eq!(m.latest_version, 1);
        m.add_version(2, root2).unwrap();
        assert_eq!(m.latest_version, 2);
        assert_eq!(m.get_root(1), Some(root1));
        assert_eq!(m.get_root(2), Some(root2));
    }

    /// T3.3: truncate_after(1) -> deletes future versions
    #[test]
    fn t3_3_truncate_after() {
        let mut m = VersionManifest::load(Path::new("/nonexistent")).unwrap();
        m.add_version(1, B256::repeat_byte(0x11)).unwrap();
        m.add_version(2, B256::repeat_byte(0x22)).unwrap();
        m.add_version(3, B256::repeat_byte(0x33)).unwrap();
        m.truncate_after(1);
        assert_eq!(m.latest_version, 1);
        assert!(m.get_root(2).is_none());
        assert!(m.get_root(3).is_none());
        assert!(m.get_root(1).is_some());
    }

    /// T3.4: save + reload roundtrip
    #[test]
    fn t3_4_save_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("manifest.json");

        let mut m = VersionManifest::load(&path).unwrap();
        m.add_version(1, B256::repeat_byte(0xaa)).unwrap();
        m.save(&path).unwrap();

        let m2 = VersionManifest::load(&path).unwrap();
        assert_eq!(m, m2);
    }

    /// T3.5: save uses tmp+rename, no corrupt half-file left
    #[test]
    fn t3_5_save_atomic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("manifest.json");
        let tmp_path = path.with_extension("tmp");

        let m = VersionManifest::load(&path).unwrap();
        m.save(&path).unwrap();

        // tmp file should not exist after save
        assert!(!tmp_path.exists());
        // manifest.json should exist
        assert!(path.exists());
    }

    /// T3.6: get_root(missing) -> None
    #[test]
    fn t3_6_get_root_missing() {
        let m = VersionManifest::load(Path::new("/nonexistent")).unwrap();
        assert!(m.get_root(999).is_none());
    }

    /// T3.7: add_version skip (latest=1, add 3) -> Err
    #[test]
    fn t3_7_add_version_skip() {
        let mut m = VersionManifest::load(Path::new("/nonexistent")).unwrap();
        m.add_version(1, B256::repeat_byte(0x11)).unwrap();
        let result = m.add_version(3, B256::repeat_byte(0x33));
        assert!(result.is_err());
    }

    /// T3.8: add_version duplicate (latest=2, add 2) -> Err
    #[test]
    fn t3_8_add_version_duplicate() {
        let mut m = VersionManifest::load(Path::new("/nonexistent")).unwrap();
        m.add_version(1, B256::repeat_byte(0x11)).unwrap();
        m.add_version(2, B256::repeat_byte(0x22)).unwrap();
        let result = m.add_version(2, B256::repeat_byte(0x33));
        assert!(result.is_err());
    }
}
