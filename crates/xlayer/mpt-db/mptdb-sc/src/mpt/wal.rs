use alloy_primitives::{Address, B256, U256};
use alloy_trie::Nibbles;
use mptdb_common::error::{MptDbError, Result};
use revm_state::AccountInfo;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use super::state::{DirtyAccount, StorageChange};

const WAL_DIR: &str = "changelog";
const WAL_META_FILE: &str = "meta.json";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitWalStorageChange {
    pub hashed_slot: B256,
    pub value: U256,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitWalAccountInfo {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitWalAccountChange {
    pub address: Address,
    pub hashed_address: B256,
    pub info: Option<CommitWalAccountInfo>,
    pub storage_wiped: bool,
    pub storage_known_empty: bool,
    pub storage_changes: Vec<CommitWalStorageChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitWalEntry {
    pub format_version: u32,
    pub version: i64,
    pub state_root: B256,
    pub account_root: B256,
    pub deleted_accounts: Vec<B256>,
    pub accounts: Vec<CommitWalAccountChange>,
}

impl CommitWalEntry {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn from_dirty_accounts(
        version: i64,
        state_root: B256,
        dirty_accounts: &[DirtyAccount],
    ) -> Self {
        let mut deleted_accounts = dirty_accounts
            .iter()
            .filter(|dirty| dirty.info.is_none() && dirty.storage_wiped)
            .map(|dirty| dirty.hashed_address)
            .collect::<Vec<_>>();
        deleted_accounts.sort_unstable();

        let mut accounts = dirty_accounts
            .iter()
            .map(|dirty| {
                let mut storage_changes = dirty
                    .storage_changes
                    .iter()
                    .map(|change| CommitWalStorageChange {
                        hashed_slot: change.hashed_slot,
                        value: change.value,
                    })
                    .collect::<Vec<_>>();
                storage_changes.sort_by(|a, b| a.hashed_slot.cmp(&b.hashed_slot));

                CommitWalAccountChange {
                    address: dirty.address,
                    hashed_address: dirty.hashed_address,
                    info: dirty.info.as_ref().map(|info| CommitWalAccountInfo {
                        nonce: info.nonce,
                        balance: info.balance,
                        code_hash: info.code_hash,
                    }),
                    storage_wiped: dirty.storage_wiped,
                    storage_known_empty: dirty.storage_known_empty,
                    storage_changes,
                }
            })
            .collect::<Vec<_>>();
        accounts.sort_by(|a, b| a.hashed_address.cmp(&b.hashed_address));

        Self {
            format_version: Self::FORMAT_VERSION,
            version,
            state_root,
            account_root: state_root,
            deleted_accounts,
            accounts,
        }
    }

    pub fn to_dirty_accounts(&self) -> Vec<DirtyAccount> {
        let mut accounts = self
            .accounts
            .iter()
            .map(|account| DirtyAccount {
                address: account.address,
                hashed_address: account.hashed_address,
                account_key: Nibbles::unpack(account.hashed_address),
                info: account.info.as_ref().map(|info| AccountInfo {
                    nonce: info.nonce,
                    balance: info.balance,
                    code_hash: info.code_hash,
                    account_id: None,
                    code: None,
                }),
                storage_wiped: account.storage_wiped,
                storage_known_empty: account.storage_known_empty,
                storage_changes: account
                    .storage_changes
                    .iter()
                    .map(|change| StorageChange {
                        hashed_slot: change.hashed_slot,
                        slot_key: Nibbles::unpack(change.hashed_slot),
                        value: change.value,
                        encoded_value: (change.value != U256::ZERO)
                            .then(|| alloy_rlp::encode(change.value)),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        accounts.sort_by(|a, b| a.hashed_address.cmp(&b.hashed_address));
        accounts
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct CommitWalMeta {
    earliest_version: i64,
    latest_version: i64,
    durable_version: i64,
}

impl CommitWalMeta {
    fn fresh() -> Self {
        Self { earliest_version: 0, latest_version: 0, durable_version: 0 }
    }

    fn is_empty(&self) -> bool {
        self.latest_version == 0
    }
}

pub struct CommitWalStore {
    dir: PathBuf,
    meta_path: PathBuf,
    meta: CommitWalMeta,
}

impl CommitWalStore {
    pub fn open(base_dir: &Path) -> Result<Self> {
        let dir = base_dir.join(WAL_DIR);
        fs::create_dir_all(&dir)
            .map_err(|e| MptDbError::Other(format!("create wal dir {}: {e}", dir.display())))?;
        let meta_path = dir.join(WAL_META_FILE);
        let meta = if meta_path.exists() {
            let bytes = fs::read(&meta_path)
                .map_err(|e| MptDbError::Other(format!("read wal meta: {e}")))?;
            serde_json::from_slice(&bytes)
                .map_err(|e| MptDbError::Other(format!("parse wal meta: {e}")))?
        } else {
            CommitWalMeta::fresh()
        };
        Ok(Self { dir, meta_path, meta })
    }

    pub fn latest_version(&self) -> i64 {
        self.meta.latest_version
    }

    pub fn earliest_version(&self) -> i64 {
        self.meta.earliest_version
    }

    pub fn durable_version(&self) -> i64 {
        self.meta.durable_version
    }

    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    pub fn append_entry(&mut self, entry: &CommitWalEntry) -> Result<()> {
        if entry.version <= 0 {
            return Err(MptDbError::Other("wal entry version must be positive".to_string()));
        }
        if !self.meta.is_empty() && entry.version != self.meta.latest_version + 1 {
            return Err(MptDbError::Other(format!(
                "wal append out of order: expected {}, got {}",
                self.meta.latest_version + 1,
                entry.version
            )));
        }
        let path = self.entry_path(entry.version);
        if path.exists() {
            return Err(MptDbError::Other(format!(
                "wal entry already exists for version {}",
                entry.version
            )));
        }

        self.write_entry_file(&path, entry)?;

        if self.meta.is_empty() {
            self.meta.earliest_version = entry.version;
        }
        self.meta.latest_version = entry.version;
        self.save_meta()
    }

    pub fn load_entry(&self, version: i64) -> Result<Option<CommitWalEntry>> {
        let path = self.entry_path(version);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)
            .map_err(|e| MptDbError::Other(format!("read wal entry {}: {e}", path.display())))?;
        let entry = serde_json::from_slice(&bytes)
            .map_err(|e| MptDbError::Other(format!("parse wal entry {}: {e}", path.display())))?;
        Ok(Some(entry))
    }

    pub fn truncate_after(&mut self, version: i64) -> Result<()> {
        if self.meta.is_empty() || version >= self.meta.latest_version {
            return Ok(());
        }
        if version < self.meta.earliest_version {
            return self.clear_all();
        }

        for v in (version + 1)..=self.meta.latest_version {
            let path = self.entry_path(v);
            if path.exists() {
                fs::remove_file(&path).map_err(|e| {
                    MptDbError::Other(format!("remove wal entry {}: {e}", path.display()))
                })?;
            }
        }

        self.meta.latest_version = version;
        if self.meta.durable_version > version {
            self.meta.durable_version = version;
        }
        if self.meta.earliest_version > self.meta.latest_version {
            self.meta = CommitWalMeta::fresh();
        }
        self.save_meta()
    }

    pub fn prune_before(&mut self, version: i64) -> Result<()> {
        if self.meta.is_empty() || version <= self.meta.earliest_version {
            return Ok(());
        }
        if version > self.meta.latest_version {
            return self.clear_all();
        }

        for v in self.meta.earliest_version..version {
            let path = self.entry_path(v);
            if path.exists() {
                fs::remove_file(&path).map_err(|e| {
                    MptDbError::Other(format!("remove wal entry {}: {e}", path.display()))
                })?;
            }
        }

        self.meta.earliest_version = version;
        self.save_meta()
    }

    pub fn set_durable_version(&mut self, version: i64) -> Result<()> {
        if version < 0 {
            return Err(MptDbError::Other("wal durable version must be non-negative".to_string()));
        }
        if version > self.meta.latest_version {
            return Err(MptDbError::Other(format!(
                "wal durable version {} exceeds latest committed {}",
                version, self.meta.latest_version
            )));
        }
        if version < self.meta.durable_version {
            return Ok(());
        }
        self.meta.durable_version = version;
        self.save_meta()
    }

    fn clear_all(&mut self) -> Result<()> {
        if !self.meta.is_empty() {
            for v in self.meta.earliest_version..=self.meta.latest_version {
                let path = self.entry_path(v);
                if path.exists() {
                    fs::remove_file(&path).map_err(|e| {
                        MptDbError::Other(format!("remove wal entry {}: {e}", path.display()))
                    })?;
                }
            }
        }
        self.meta = CommitWalMeta::fresh();
        self.save_meta()
    }

    fn entry_path(&self, version: i64) -> PathBuf {
        self.dir.join(format!("v-{version}.json"))
    }

    fn write_entry_file(&self, path: &Path, entry: &CommitWalEntry) -> Result<()> {
        let tmp_path = path.with_extension("tmp");
        let bytes = serde_json::to_vec(entry)
            .map_err(|e| MptDbError::Other(format!("serialize wal entry: {e}")))?;
        let mut file = File::create(&tmp_path)
            .map_err(|e| MptDbError::Other(format!("create wal tmp: {e}")))?;
        file.write_all(&bytes).map_err(|e| MptDbError::Other(format!("write wal tmp: {e}")))?;
        file.sync_all().map_err(|e| MptDbError::Other(format!("fsync wal tmp: {e}")))?;
        drop(file);
        fs::rename(&tmp_path, path)
            .map_err(|e| MptDbError::Other(format!("rename wal entry: {e}")))?;
        Ok(())
    }

    fn save_meta(&self) -> Result<()> {
        let tmp_path = self.meta_path.with_extension("tmp");
        let bytes = serde_json::to_vec(&self.meta)
            .map_err(|e| MptDbError::Other(format!("serialize wal meta: {e}")))?;
        let mut file = File::create(&tmp_path)
            .map_err(|e| MptDbError::Other(format!("create wal meta tmp: {e}")))?;
        file.write_all(&bytes)
            .map_err(|e| MptDbError::Other(format!("write wal meta tmp: {e}")))?;
        file.sync_all().map_err(|e| MptDbError::Other(format!("fsync wal meta tmp: {e}")))?;
        drop(file);
        fs::rename(&tmp_path, &self.meta_path)
            .map_err(|e| MptDbError::Other(format!("rename wal meta: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_entry(version: i64) -> CommitWalEntry {
        CommitWalEntry {
            format_version: CommitWalEntry::FORMAT_VERSION,
            version,
            state_root: B256::repeat_byte(version as u8),
            account_root: B256::repeat_byte(version as u8),
            deleted_accounts: vec![],
            accounts: vec![],
        }
    }

    #[test]
    fn wal_append_and_reload_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        wal.append_entry(&sample_entry(3)).unwrap();

        let reopened = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(reopened.earliest_version(), 3);
        assert_eq!(reopened.latest_version(), 3);
        assert_eq!(reopened.durable_version(), 0);
        assert_eq!(reopened.load_entry(3).unwrap(), Some(sample_entry(3)));
    }

    #[test]
    fn wal_truncate_after_removes_newer_entries() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        wal.append_entry(&sample_entry(2)).unwrap();
        wal.append_entry(&sample_entry(3)).unwrap();
        wal.truncate_after(2).unwrap();

        assert_eq!(wal.latest_version(), 2);
        assert!(wal.load_entry(3).unwrap().is_none());
        assert!(wal.load_entry(2).unwrap().is_some());
    }

    #[test]
    fn wal_prune_before_advances_earliest() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        wal.append_entry(&sample_entry(5)).unwrap();
        wal.append_entry(&sample_entry(6)).unwrap();
        wal.append_entry(&sample_entry(7)).unwrap();
        wal.prune_before(6).unwrap();

        assert_eq!(wal.earliest_version(), 6);
        assert_eq!(wal.latest_version(), 7);
        assert!(wal.load_entry(5).unwrap().is_none());
        assert!(wal.load_entry(6).unwrap().is_some());
    }

    #[test]
    fn wal_durable_version_persists() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        wal.append_entry(&sample_entry(1)).unwrap();
        wal.append_entry(&sample_entry(2)).unwrap();
        wal.set_durable_version(2).unwrap();

        let reopened = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(reopened.durable_version(), 2);
    }
}
