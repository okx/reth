//! WAL recovery: replay changelog entries to bring state stores up to date.
//!
//! When a node restarts after a crash, the Cosmos and/or EVM state stores may
//! be behind the WAL. This module provides functions to replay WAL entries and
//! bring both stores to the latest committed version.

use crate::{evm::store::EVMStateStore, evm_types::EVM_STORE_KEY};
use seidb_common::{
    config::{WalConfig, WriteMode},
    error::{Result, SeiDbError},
    evm_keys::{parse_evm_key, EvmKeyKind},
};
use seidb_proto::{ChangeSet, ChangelogEntry, KvPair, NamedChangeSet};
use seidb_traits::{ss::StateStore, wal::Wal};
use seidb_wal::changelog::{new_changelog_wal, ChangelogWal};
use std::path::Path;
use tracing::info;

/// Recover a composite state store by replaying WAL entries that are ahead of
/// the store versions.
///
/// Reads the cosmos and (optionally) EVM store versions, opens the changelog
/// WAL, and replays any entries beyond the minimum store version. In
/// `SplitWrite` mode, EVM data is stripped from changesets before applying to
/// cosmos.
pub fn recover_composite_state_store(
    changelog_path: &Path,
    cosmos_store: &dyn StateStore,
    evm_store: Option<&EVMStateStore>,
    write_mode: WriteMode,
) -> Result<()> {
    let cosmos_version = cosmos_store.get_latest_version();
    let evm_version = evm_store.map(|s| s.get_latest_version()).unwrap_or(cosmos_version);

    let start_version = std::cmp::min(cosmos_version, evm_version);

    info!(
        cosmos_version,
        evm_version,
        start_version,
        ?changelog_path,
        "recovering composite state store"
    );

    let split_write = write_mode == WriteMode::SplitWrite;
    let mut cosmos_v = cosmos_version;
    let mut evm_v = evm_version;

    replay_wal(changelog_path, start_version, -1, &mut |entry: &ChangelogEntry| {
        // Apply to cosmos if it is behind this entry.
        if entry.version > cosmos_v {
            let changesets = if split_write {
                strip_evm_from_changesets(&entry.changesets)
            } else {
                entry.changesets.clone()
            };
            cosmos_store.apply_changeset_sync(entry.version, &changesets).map_err(|e| {
                SeiDbError::Other(format!(
                    "failed to apply cosmos changeset at version {}: {e}",
                    entry.version
                ))
            })?;
            cosmos_store.set_latest_version(entry.version).map_err(|e| {
                SeiDbError::Other(format!("failed to set cosmos version {}: {e}", entry.version))
            })?;
            cosmos_v = entry.version;
        }

        // Apply to EVM if it is behind this entry.
        if let Some(evm) = evm_store &&
            entry.version > evm_v
        {
            let evm_changesets = filter_evm_changesets(&entry.changesets);
            if !evm_changesets.is_empty() {
                evm.apply_changeset_sync(entry.version, &evm_changesets).map_err(|e| {
                    SeiDbError::Other(format!(
                        "failed to apply EVM changeset at version {}: {e}",
                        entry.version
                    ))
                })?;
            }
            evm.set_latest_version(entry.version).map_err(|e| {
                SeiDbError::Other(format!("failed to set EVM version {}: {e}", entry.version))
            })?;
            evm_v = entry.version;
        }

        Ok(())
    })
}

/// Replay WAL entries in `[from_version+1, to_version]`, invoking `handler`
/// for each entry.
///
/// If `to_version` is negative, replays to the end of the WAL.
pub fn replay_wal(
    changelog_path: &Path,
    from_version: i64,
    to_version: i64,
    handler: &mut dyn FnMut(&ChangelogEntry) -> Result<()>,
) -> Result<()> {
    let wal = new_changelog_wal(WalConfig::default(), changelog_path).map_err(|e| {
        SeiDbError::Other(format!("failed to open WAL at {}: {e}", changelog_path.display()))
    })?;

    let first_offset = wal
        .first_offset()
        .map_err(|e| SeiDbError::Other(format!("failed to read WAL first offset: {e}")))?;
    if first_offset == 0 {
        return Ok(());
    }

    let last_offset = wal
        .last_offset()
        .map_err(|e| SeiDbError::Other(format!("failed to read WAL last offset: {e}")))?;
    if last_offset == 0 {
        return Ok(());
    }

    let last_entry = wal
        .read_at(last_offset)
        .map_err(|e| SeiDbError::Other(format!("failed to read last WAL entry: {e}")))?;

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
            .map_err(|e| SeiDbError::Other(format!("failed to read WAL at offset {mid}: {e}")))?;
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

/// Return only changesets whose name matches [`EVM_STORE_KEY`].
fn filter_evm_changesets(changesets: &[NamedChangeSet]) -> Vec<NamedChangeSet> {
    changesets.iter().filter(|cs| cs.name == EVM_STORE_KEY).cloned().collect()
}

/// Strip EVM-typed key pairs from changesets named [`EVM_STORE_KEY`].
///
/// For changesets with a different name the changeset is kept as-is. For EVM
/// changesets, only pairs whose key parses to `Empty` or `Legacy` are retained
/// (these are non-EVM data that should stay in the Cosmos store). If all pairs
/// are stripped the changeset is omitted entirely.
fn strip_evm_from_changesets(changesets: &[NamedChangeSet]) -> Vec<NamedChangeSet> {
    let mut stripped = Vec::with_capacity(changesets.len());
    for cs in changesets {
        if cs.name != EVM_STORE_KEY {
            stripped.push(cs.clone());
            continue;
        }
        if let Some(ref changeset) = cs.changeset {
            let kept_pairs: Vec<KvPair> = changeset
                .pairs
                .iter()
                .filter(|pair| {
                    let (kind, _) = parse_evm_key(&pair.key);
                    kind == EvmKeyKind::Empty || kind == EvmKeyKind::Legacy
                })
                .cloned()
                .collect();
            if !kept_pairs.is_empty() {
                stripped.push(NamedChangeSet {
                    name: cs.name.clone(),
                    changeset: Some(ChangeSet { pairs: kept_pairs }),
                });
            }
        }
    }
    stripped
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::{config::StateStoreConfig, evm_keys::NONCE_KEY_PREFIX};
    use seidb_engine::mvcc::db::MvccDatabase;
    use seidb_proto::{ChangeSet, KvPair, NamedChangeSet};
    use seidb_traits::wal::Wal;
    use tempfile::tempdir;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn test_config(dir: &std::path::Path) -> StateStoreConfig {
        StateStoreConfig {
            db_directory: dir.to_string_lossy().to_string(),
            keep_last_version: true,
            ..Default::default()
        }
    }

    fn open_cosmos_store(dir: &std::path::Path) -> Box<dyn StateStore> {
        let cfg = test_config(dir);
        let db = MvccDatabase::open_db(&cfg).unwrap();
        Box::new(db)
    }

    fn open_evm_store(dir: &std::path::Path) -> EVMStateStore {
        let cfg = test_config(dir);
        EVMStateStore::new(&dir.to_string_lossy(), &cfg).unwrap()
    }

    fn make_changelog_entry(
        version: i64,
        store: &str,
        pairs: Vec<(&[u8], &[u8])>,
    ) -> ChangelogEntry {
        ChangelogEntry {
            version,
            changesets: vec![NamedChangeSet {
                name: store.to_string(),
                changeset: Some(ChangeSet {
                    pairs: pairs
                        .into_iter()
                        .map(|(k, v)| KvPair { delete: false, key: k.to_vec(), value: v.to_vec() })
                        .collect(),
                }),
            }],
            upgrades: vec![],
        }
    }

    fn make_nonce_key(addr: &[u8; 20]) -> Vec<u8> {
        let mut key = vec![NONCE_KEY_PREFIX];
        key.extend_from_slice(addr);
        key
    }

    fn test_addr() -> [u8; 20] {
        let mut addr = [0u8; 20];
        for (i, b) in addr.iter_mut().enumerate() {
            *b = (i + 1) as u8;
        }
        addr
    }

    fn write_wal_entries(wal_dir: &std::path::Path, entries: &[ChangelogEntry]) {
        let wal =
            new_changelog_wal(WalConfig { fsync_enabled: false, ..Default::default() }, wal_dir)
                .unwrap();
        for entry in entries {
            wal.write(entry.clone()).unwrap();
        }
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_recover_empty_wal() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let cosmos_dir = dir.path().join("cosmos");
        let cosmos = open_cosmos_store(&cosmos_dir);

        // Empty WAL directory — recovery should be a no-op.
        let result =
            recover_composite_state_store(&wal_dir, cosmos.as_ref(), None, WriteMode::CosmosOnly);
        assert!(result.is_ok());
    }

    #[test]
    fn test_recover_cosmos_behind() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        // Write 5 WAL entries.
        let entries: Vec<_> = (1..=5)
            .map(|v| make_changelog_entry(v, "bank", vec![(b"key", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        // Cosmos store is at version 3.
        let cosmos_dir = dir.path().join("cosmos");
        let cosmos = open_cosmos_store(&cosmos_dir);
        let cs = vec![NamedChangeSet {
            name: "bank".to_string(),
            changeset: Some(ChangeSet {
                pairs: vec![KvPair { delete: false, key: b"key".to_vec(), value: b"v3".to_vec() }],
            }),
        }];
        cosmos.apply_changeset_sync(3, &cs).unwrap();
        cosmos.set_latest_version(3).unwrap();
        assert_eq!(cosmos.get_latest_version(), 3);

        // Recover: should replay v4 and v5.
        recover_composite_state_store(&wal_dir, cosmos.as_ref(), None, WriteMode::CosmosOnly)
            .unwrap();

        assert_eq!(cosmos.get_latest_version(), 5);
        let val = cosmos.get("bank", 5, b"key").unwrap();
        assert_eq!(val, Some(b"v5".to_vec()));
    }

    #[test]
    fn test_recover_evm_behind() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write 5 WAL entries with EVM changesets.
        let entries: Vec<_> = (1..=5)
            .map(|v| ChangelogEntry {
                version: v,
                changesets: vec![
                    NamedChangeSet {
                        name: "bank".to_string(),
                        changeset: Some(ChangeSet {
                            pairs: vec![KvPair {
                                delete: false,
                                key: b"key".to_vec(),
                                value: format!("v{v}").into_bytes(),
                            }],
                        }),
                    },
                    NamedChangeSet {
                        name: EVM_STORE_KEY.to_string(),
                        changeset: Some(ChangeSet {
                            pairs: vec![KvPair {
                                delete: false,
                                key: nonce_key.clone(),
                                value: format!("nonce{v}").into_bytes(),
                            }],
                        }),
                    },
                ],
                upgrades: vec![],
            })
            .collect();
        write_wal_entries(&wal_dir, &entries);

        // Cosmos is at version 5 (up to date).
        let cosmos_dir = dir.path().join("cosmos");
        let cosmos = open_cosmos_store(&cosmos_dir);
        cosmos.set_latest_version(5).unwrap();

        // EVM is at version 3 (behind).
        let evm_dir = dir.path().join("evm");
        let evm = open_evm_store(&evm_dir);
        evm.set_latest_version(3).unwrap();

        // Recover: EVM should catch up to v5.
        recover_composite_state_store(&wal_dir, cosmos.as_ref(), Some(&evm), WriteMode::DualWrite)
            .unwrap();

        assert_eq!(evm.get_latest_version(), 5);
        let val = evm.get(EVM_STORE_KEY, 5, &nonce_key).unwrap();
        assert_eq!(val, Some(b"nonce5".to_vec()));
    }

    #[test]
    fn test_replay_wal_basic() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let entries: Vec<_> = (1..=5)
            .map(|v| make_changelog_entry(v, "bank", vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        // Replay all entries (from_version=0 means replay everything).
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
            .map(|v| make_changelog_entry(v, "bank", vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        // Replay from version 3 — should get entries 4 and 5.
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
            .map(|v| make_changelog_entry(v, "bank", vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        // Replay from v1 to v3.
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
            .map(|v| make_changelog_entry(v, "bank", vec![(b"k", format!("v{v}").as_bytes())]))
            .collect();
        write_wal_entries(&wal_dir, &entries);

        let wal = new_changelog_wal(WalConfig::default(), &wal_dir).unwrap();
        let first = wal.first_offset().unwrap();
        let last = wal.last_offset().unwrap();

        // target_version=0: first entry with version > 0 is offset 1 (version 1).
        let offset = find_replay_start_offset(&wal, first, last, 0).unwrap();
        assert_eq!(offset, first);

        // target_version=3: first entry with version > 3 is offset for version 4.
        let offset = find_replay_start_offset(&wal, first, last, 3).unwrap();
        let entry = wal.read_at(offset).unwrap();
        assert_eq!(entry.version, 4);

        // target_version=5: no entry with version > 5, result should be last+1.
        let offset = find_replay_start_offset(&wal, first, last, 5).unwrap();
        assert_eq!(offset, last + 1);
    }

    #[test]
    fn test_filter_in_recovery() {
        let dir = tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        std::fs::create_dir_all(&wal_dir).unwrap();

        let addr = test_addr();
        let nonce_key = make_nonce_key(&addr);

        // Write a WAL entry with both bank and EVM changesets.
        let entry = ChangelogEntry {
            version: 1,
            changesets: vec![
                NamedChangeSet {
                    name: "bank".to_string(),
                    changeset: Some(ChangeSet {
                        pairs: vec![KvPair {
                            delete: false,
                            key: b"alice".to_vec(),
                            value: b"100".to_vec(),
                        }],
                    }),
                },
                NamedChangeSet {
                    name: EVM_STORE_KEY.to_string(),
                    changeset: Some(ChangeSet {
                        pairs: vec![KvPair {
                            delete: false,
                            key: nonce_key.clone(),
                            value: b"42".to_vec(),
                        }],
                    }),
                },
            ],
            upgrades: vec![],
        };
        write_wal_entries(&wal_dir, &[entry]);

        // Cosmos store at version 0 — SplitWrite mode should strip EVM nonce
        // from cosmos changesets.
        let cosmos_dir = dir.path().join("cosmos");
        let cosmos = open_cosmos_store(&cosmos_dir);

        recover_composite_state_store(&wal_dir, cosmos.as_ref(), None, WriteMode::SplitWrite)
            .unwrap();

        // Bank data should be applied.
        let val = cosmos.get("bank", 1, b"alice").unwrap();
        assert_eq!(val, Some(b"100".to_vec()));

        // Nonce key should NOT be in cosmos (stripped in SplitWrite mode).
        let val = cosmos.get(EVM_STORE_KEY, 1, &nonce_key).unwrap();
        assert_eq!(val, None);
    }
}
