use crate::flatkv::{
    keys::{unmarshal_local_meta, LocalMeta, DB_LOCAL_META_KEY},
    lthash::LtHash,
};
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::{kv::KvEngine, types::WriteOptions};

pub const META_GLOBAL_VERSION: &[u8] = b"_meta/version";
pub const META_GLOBAL_LT_HASH: &[u8] = b"_meta/hash";

/// Loads per-DB local metadata, returning default (version 0) if absent.
pub fn load_local_meta(db: &dyn KvEngine) -> Result<LocalMeta> {
    match db.get(DB_LOCAL_META_KEY)? {
        None => Ok(LocalMeta { committed_version: 0 }),
        Some(data) => unmarshal_local_meta(&data),
    }
}

/// Reads the global committed version from the metadata DB.
/// Returns 0 if not found (fresh start).
pub fn load_global_version(metadata_db: &dyn KvEngine) -> Result<i64> {
    match metadata_db.get(META_GLOBAL_VERSION)? {
        None => Ok(0),
        Some(data) => {
            if data.len() != 8 {
                return Err(SeiDbError::Other(format!(
                    "invalid global version length: got {}, want 8",
                    data.len()
                )));
            }
            let v = u64::from_be_bytes(data[..8].try_into().unwrap());
            if v > i64::MAX as u64 {
                return Err(SeiDbError::Other(format!(
                    "global version overflow: {} exceeds max i64",
                    v
                )));
            }
            Ok(v as i64)
        }
    }
}

/// Reads the global committed LtHash from the metadata DB.
/// Returns None if not found (fresh start).
pub fn load_global_lt_hash(metadata_db: &dyn KvEngine) -> Result<Option<LtHash>> {
    match metadata_db.get(META_GLOBAL_LT_HASH)? {
        None => Ok(None),
        Some(data) => LtHash::unmarshal(&data).map(Some),
    }
}

/// Atomically commits global version and LtHash to the metadata DB.
/// This is the global watermark written AFTER all per-DB commits succeed.
pub fn commit_global_metadata(
    metadata_db: &dyn KvEngine,
    version: i64,
    hash: &LtHash,
    fsync: bool,
) -> Result<()> {
    let mut batch = metadata_db.new_batch();
    batch.set(META_GLOBAL_VERSION, &(version as u64).to_be_bytes())?;
    batch.set(META_GLOBAL_LT_HASH, &hash.marshal())?;
    batch.commit(&WriteOptions { sync: fsync })
}

/// Loads both global version and LtHash from the metadata DB.
/// Returns (0, LtHash::default()) for a fresh database.
pub fn load_global_metadata(metadata_db: &dyn KvEngine) -> Result<(i64, LtHash)> {
    let version = load_global_version(metadata_db)?;
    let lt_hash = load_global_lt_hash(metadata_db)?.unwrap_or_default();
    Ok((version, lt_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_engine::engine::RocksDbEngine;
    use tempfile::TempDir;

    fn tmp_engine() -> (RocksDbEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = RocksDbEngine::open_plain(dir.path()).unwrap();
        (engine, dir)
    }

    #[test]
    fn test_load_local_meta_default() {
        let (engine, _dir) = tmp_engine();
        let meta = load_local_meta(&engine).unwrap();
        assert_eq!(meta.committed_version, 0);
    }

    #[test]
    fn test_load_global_version_default() {
        let (engine, _dir) = tmp_engine();
        let version = load_global_version(&engine).unwrap();
        assert_eq!(version, 0);
    }

    #[test]
    fn test_commit_and_load_global_metadata() {
        let (engine, _dir) = tmp_engine();

        // Write metadata
        let version = 42i64;
        let mut hash = LtHash::new();
        hash.limbs[0] = 1234;
        hash.limbs[100] = 5678;
        commit_global_metadata(&engine, version, &hash, false).unwrap();

        // Read back
        let (loaded_version, loaded_hash) = load_global_metadata(&engine).unwrap();
        assert_eq!(loaded_version, version);
        assert_eq!(loaded_hash, hash);
    }

    #[test]
    fn test_global_metadata_atomicity() {
        let (engine, _dir) = tmp_engine();

        // Write first version
        let hash1 = LtHash::new();
        commit_global_metadata(&engine, 1, &hash1, false).unwrap();

        // Write second version — both version and hash should update together
        let mut hash2 = LtHash::new();
        hash2.limbs[0] = 42;
        commit_global_metadata(&engine, 2, &hash2, false).unwrap();

        let (v, h) = load_global_metadata(&engine).unwrap();
        assert_eq!(v, 2);
        assert_eq!(h, hash2);
    }
}
