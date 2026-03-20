use alloy_primitives::{Address, B256, U256};
use alloy_trie::Nibbles;
use mptdb_common::error::{MptDbError, Result};
use revm_state::AccountInfo;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use super::state::{DirtyAccount, StorageChange};

const WAL_DIR: &str = "changelog";
const WAL_META_FILE: &str = "meta.json";
const WAL_SEGMENT_MAGIC: &[u8; 8] = b"mptwal02";
const WAL_SEGMENT_FORMAT_VERSION: u16 = 2;
const WAL_SEGMENT_HEADER_LEN: u64 = 16;
const WAL_SEGMENT_ENTRY_LIMIT: usize = 64;

/// Record layout (on disk):
///   version:     i64  (8 bytes LE)
///   payload_len: u32  (4 bytes LE)
///   crc32:       u32  (4 bytes LE)  — CRC32C of payload bytes
///   payload:     [u8; payload_len]  — bincode-encoded CommitWalEntry
const WAL_RECORD_HEADER_LEN: usize = 8 + 4 + 4;

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

/// Schema or configuration upgrade recorded in the WAL.
///
/// During replay, upgrade entries are applied before the changeset so that
/// the replay materializer uses the correct encoding/layout for the version.
/// Sei-db records `TreeNameUpgrade` (add/delete/rename trees); mpt-db uses a
/// generic key-value pair since there is only a single trie.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitWalUpgrade {
    /// Machine-readable upgrade key (e.g. "format_version", "key_encoding").
    pub key: String,
    /// Opaque value associated with the upgrade.
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitWalEntry {
    pub format_version: u32,
    pub version: i64,
    pub state_root: B256,
    pub account_root: B256,
    pub deleted_accounts: Vec<B256>,
    pub accounts: Vec<CommitWalAccountChange>,
    /// Optional schema/configuration upgrades that took effect at this version.
    /// Empty for normal commits.  Replay must apply these before the changeset.
    pub upgrades: Vec<CommitWalUpgrade>,
}

impl CommitWalEntry {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn from_dirty_accounts(
        version: i64,
        state_root: B256,
        account_root: B256,
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
            account_root,
            deleted_accounts,
            accounts,
            upgrades: Vec::new(),
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

#[derive(Clone, Copy, Debug)]
struct WalLocation {
    segment_id: u32,
    offset: u64,
    len: u32,
}

#[derive(Clone, Copy, Debug)]
struct WalSegmentRange {
    first_version: i64,
    last_version: i64,
    entries: usize,
    /// Byte offset of the first valid byte past the last complete record.
    valid_end: u64,
}

pub struct CommitWalStore {
    dir: PathBuf,
    meta_path: PathBuf,
    meta: CommitWalMeta,
    index: BTreeMap<i64, WalLocation>,
    segments: BTreeMap<u32, WalSegmentRange>,
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

        let (index, segments) = Self::scan_segments(&dir)?;

        // Reconstruct earliest/latest from segments (authoritative source).
        let mut meta = meta;
        if let Some((&earliest, _)) = index.first_key_value() {
            meta.earliest_version = earliest;
        }
        if let Some((&latest, _)) = index.last_key_value() {
            meta.latest_version = latest;
        }
        if index.is_empty() {
            meta = CommitWalMeta::fresh();
        } else if meta.durable_version > meta.latest_version {
            meta.durable_version = meta.latest_version;
        }

        Ok(Self { dir, meta_path, meta, index, segments })
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

    pub fn size_bytes(&self) -> u64 {
        let segments_size = self.segments.values().map(|range| range.valid_end).sum::<u64>();
        let meta_size = fs::metadata(&self.meta_path).map(|meta| meta.len()).unwrap_or(0);
        segments_size + meta_size
    }

    pub fn is_empty(&self) -> bool {
        self.meta.is_empty()
    }

    // ── Write path ──

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
        if self.index.contains_key(&entry.version) {
            return Err(MptDbError::Other(format!(
                "wal entry already exists for version {}",
                entry.version
            )));
        }

        let segment_id = self.next_append_segment_id();
        let payload = bincode::serialize(entry)
            .map_err(|e| MptDbError::Other(format!("serialize wal entry: {e}")))?;
        let crc = crc32fast::hash(&payload);
        let payload_len = payload.len() as u32;

        let path = self.segment_path(segment_id);
        let mut file =
            OpenOptions::new().read(true).write(true).create(true).open(&path).map_err(|e| {
                MptDbError::Other(format!("open wal segment {}: {e}", path.display()))
            })?;

        let file_len = file
            .metadata()
            .map_err(|e| MptDbError::Other(format!("stat wal segment {}: {e}", path.display())))?
            .len();

        if file_len == 0 {
            // New segment: write header.
            Self::write_segment_header(&mut file)?;
        } else if let Some(range) = self.segments.get(&segment_id) {
            // Existing segment: truncate any corrupted tail past the last valid record.
            if file_len > range.valid_end {
                file.set_len(range.valid_end)
                    .map_err(|e| MptDbError::Other(format!("truncate wal segment tail: {e}")))?;
            }
        }

        let offset = file
            .seek(SeekFrom::End(0))
            .map_err(|e| MptDbError::Other(format!("seek wal segment: {e}")))?;

        // Write record: [version(8) | payload_len(4) | crc32(4) | payload]
        file.write_all(&entry.version.to_le_bytes())
            .map_err(|e| MptDbError::Other(format!("write wal record version: {e}")))?;
        file.write_all(&payload_len.to_le_bytes())
            .map_err(|e| MptDbError::Other(format!("write wal record len: {e}")))?;
        file.write_all(&crc.to_le_bytes())
            .map_err(|e| MptDbError::Other(format!("write wal record crc: {e}")))?;
        file.write_all(&payload)
            .map_err(|e| MptDbError::Other(format!("write wal record payload: {e}")))?;
        // No fsync here — matching sei-db's async WAL model.
        // Data is in the OS page cache after write_all.  On crash,
        // unfsynced entries are lost; recovery uses durable_version
        // (synced via save_meta in the persist worker).  scan_segments
        // handles incomplete tails on restart.

        let record_end = offset + WAL_RECORD_HEADER_LEN as u64 + payload_len as u64;
        self.index.insert(entry.version, WalLocation { segment_id, offset, len: payload_len });
        self.segments
            .entry(segment_id)
            .and_modify(|range| {
                range.last_version = entry.version;
                range.entries += 1;
                range.valid_end = record_end;
            })
            .or_insert(WalSegmentRange {
                first_version: entry.version,
                last_version: entry.version,
                entries: 1,
                valid_end: record_end,
            });

        if self.meta.is_empty() {
            self.meta.earliest_version = entry.version;
        }
        self.meta.latest_version = entry.version;
        // Skip save_meta here — earliest/latest are recoverable from scan_segments.
        // Only durable_version (set via set_durable_version) needs explicit meta persistence.
        Ok(())
    }

    // ── Read path ──

    pub fn load_entry(&self, version: i64) -> Result<Option<CommitWalEntry>> {
        let Some(location) = self.index.get(&version).copied() else {
            return Ok(None);
        };
        let mut file = File::open(self.segment_path(location.segment_id))
            .map_err(|e| MptDbError::Other(format!("open wal segment for read: {e}")))?;
        file.seek(SeekFrom::Start(location.offset))
            .map_err(|e| MptDbError::Other(format!("seek wal record: {e}")))?;

        // Read record header.
        let mut hdr = [0u8; WAL_RECORD_HEADER_LEN];
        file.read_exact(&mut hdr)
            .map_err(|e| MptDbError::Other(format!("read wal record header: {e}")))?;
        let _record_version = i64::from_le_bytes(hdr[0..8].try_into().unwrap());
        let payload_len = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let stored_crc = u32::from_le_bytes(hdr[12..16].try_into().unwrap());

        if payload_len != location.len {
            return Err(MptDbError::Other(format!(
                "wal record len mismatch for version {}: index {}, file {}",
                version, location.len, payload_len
            )));
        }

        let mut payload = vec![0u8; payload_len as usize];
        file.read_exact(&mut payload)
            .map_err(|e| MptDbError::Other(format!("read wal record payload: {e}")))?;

        let actual_crc = crc32fast::hash(&payload);
        if actual_crc != stored_crc {
            return Err(MptDbError::Other(format!(
                "wal record crc mismatch for version {}: stored {stored_crc:#010x}, computed {actual_crc:#010x}",
                version
            )));
        }

        let entry = bincode::deserialize(&payload)
            .map_err(|e| MptDbError::Other(format!("parse wal record payload: {e}")))?;
        Ok(Some(entry))
    }

    // ── Truncation / pruning ──

    /// Remove all entries with version > `version`. Keeps [earliest, version].
    pub fn truncate_after(&mut self, version: i64) -> Result<()> {
        if self.meta.is_empty() || version >= self.meta.latest_version {
            return Ok(());
        }
        if version < self.meta.earliest_version {
            return self.clear_all();
        }

        // Delete whole segments that are entirely beyond the cutoff.
        let segments_to_remove: Vec<u32> = self
            .segments
            .iter()
            .filter(|(_, range)| range.first_version > version)
            .map(|(&id, _)| id)
            .collect();
        for seg_id in &segments_to_remove {
            self.remove_segment_file(*seg_id)?;
            self.segments.remove(seg_id);
        }

        // Truncate the boundary segment that spans the cutoff.
        if let Some((&seg_id, _)) = self
            .segments
            .iter()
            .rev()
            .find(|(_, range)| range.last_version > version && range.first_version <= version)
        {
            // Find the offset just past the last record we want to keep.
            if let Some(loc) = self.index.get(&version) {
                let keep_end = loc.offset + WAL_RECORD_HEADER_LEN as u64 + loc.len as u64;
                let path = self.segment_path(seg_id);
                let file = OpenOptions::new().write(true).open(&path).map_err(|e| {
                    MptDbError::Other(format!("open wal segment for truncate: {e}"))
                })?;
                file.set_len(keep_end)
                    .map_err(|e| MptDbError::Other(format!("truncate wal segment file: {e}")))?;
                file.sync_all()
                    .map_err(|e| MptDbError::Other(format!("fsync truncated wal segment: {e}")))?;
                // Update segment range.
                if let Some(range) = self.segments.get_mut(&seg_id) {
                    range.last_version = version;
                    range.valid_end = keep_end;
                    range.entries = self.index.range(range.first_version..=version).count();
                }
            }
        }

        // Remove index entries beyond version.
        let to_remove: Vec<i64> = self.index.range((version + 1)..).map(|(&v, _)| v).collect();
        for v in to_remove {
            self.index.remove(&v);
        }

        self.meta.latest_version = version;
        if self.meta.durable_version > version {
            self.meta.durable_version = version;
        }
        self.save_meta()
    }

    /// Remove all entries with version < `version`. Keeps [version, latest].
    pub fn prune_before(&mut self, version: i64) -> Result<()> {
        if self.meta.is_empty() || version <= self.meta.earliest_version {
            return Ok(());
        }
        if version > self.meta.latest_version {
            return self.clear_all();
        }

        // Delete whole segments that are entirely before the cutoff.
        let segments_to_remove: Vec<u32> = self
            .segments
            .iter()
            .filter(|(_, range)| range.last_version < version)
            .map(|(&id, _)| id)
            .collect();
        for seg_id in &segments_to_remove {
            self.remove_segment_file(*seg_id)?;
            self.segments.remove(seg_id);
        }

        // The boundary segment that spans the cutoff needs rewriting: we must
        // remove entries from the beginning, and the on-disk format is append-only.
        if let Some((&seg_id, _)) = self
            .segments
            .iter()
            .find(|(_, range)| range.first_version < version && range.last_version >= version)
        {
            self.rewrite_segment_keeping_range(seg_id, version..=self.meta.latest_version)?;
        }

        // Remove index entries before version.
        let to_remove: Vec<i64> = self.index.range(..version).map(|(&v, _)| v).collect();
        for v in to_remove {
            self.index.remove(&v);
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

    // ── Internal helpers ──

    fn clear_all(&mut self) -> Result<()> {
        self.remove_all_segment_files()?;
        self.index.clear();
        self.segments.clear();
        self.meta = CommitWalMeta::fresh();
        self.save_meta()
    }

    /// Rewrite a single segment, keeping only entries whose version is in `keep_range`.
    fn rewrite_segment_keeping_range(
        &mut self,
        seg_id: u32,
        keep_range: std::ops::RangeInclusive<i64>,
    ) -> Result<()> {
        // Collect the entries to keep (only from this segment).
        let versions_in_segment: Vec<i64> = self
            .index
            .range(keep_range)
            .filter(|(_, loc)| loc.segment_id == seg_id)
            .map(|(&v, _)| v)
            .collect();

        let mut kept_entries = Vec::with_capacity(versions_in_segment.len());
        for v in &versions_in_segment {
            if let Some(entry) = self.load_entry(*v)? {
                kept_entries.push(entry);
            }
        }

        // Remove old segment file and all its index entries.
        self.remove_segment_file(seg_id)?;
        for v in
            self.segments.get(&seg_id).map(|r| r.first_version..=r.last_version).unwrap_or(0..=0)
        {
            self.index.remove(&v);
        }
        self.segments.remove(&seg_id);

        // Write kept entries into a new segment with the same id.
        if !kept_entries.is_empty() {
            let path = self.segment_path(seg_id);
            let mut file = File::create(&path)
                .map_err(|e| MptDbError::Other(format!("create rewritten wal segment: {e}")))?;
            Self::write_segment_header(&mut file)?;

            let mut first_version = 0i64;
            let mut last_version = 0i64;
            let mut count = 0usize;

            for entry in &kept_entries {
                let payload = bincode::serialize(entry)
                    .map_err(|e| MptDbError::Other(format!("serialize wal entry: {e}")))?;
                let crc = crc32fast::hash(&payload);
                let payload_len = payload.len() as u32;
                let offset = file
                    .stream_position()
                    .map_err(|e| MptDbError::Other(format!("tell wal segment: {e}")))?;

                file.write_all(&entry.version.to_le_bytes())
                    .map_err(|e| MptDbError::Other(format!("write wal record version: {e}")))?;
                file.write_all(&payload_len.to_le_bytes())
                    .map_err(|e| MptDbError::Other(format!("write wal record len: {e}")))?;
                file.write_all(&crc.to_le_bytes())
                    .map_err(|e| MptDbError::Other(format!("write wal record crc: {e}")))?;
                file.write_all(&payload)
                    .map_err(|e| MptDbError::Other(format!("write wal record payload: {e}")))?;

                self.index.insert(
                    entry.version,
                    WalLocation { segment_id: seg_id, offset, len: payload_len },
                );

                if count == 0 {
                    first_version = entry.version;
                }
                last_version = entry.version;
                count += 1;
            }

            file.sync_all()
                .map_err(|e| MptDbError::Other(format!("fsync rewritten wal segment: {e}")))?;

            let valid_end = file
                .stream_position()
                .map_err(|e| MptDbError::Other(format!("tell wal segment: {e}")))?;
            self.segments.insert(
                seg_id,
                WalSegmentRange { first_version, last_version, entries: count, valid_end },
            );
        }

        Ok(())
    }

    fn remove_segment_file(&self, seg_id: u32) -> Result<()> {
        let path = self.segment_path(seg_id);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| {
                MptDbError::Other(format!("remove wal segment {}: {e}", path.display()))
            })?;
        }
        Ok(())
    }

    fn remove_all_segment_files(&self) -> Result<()> {
        for entry in fs::read_dir(&self.dir)
            .map_err(|e| MptDbError::Other(format!("read wal dir {}: {e}", self.dir.display())))?
        {
            let entry = entry.map_err(|e| MptDbError::Other(format!("read wal dir entry: {e}")))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == WAL_META_FILE {
                continue;
            }
            if name.starts_with("seg-") && name.ends_with(".wal") ||
                name.starts_with("v-") && name.ends_with(".json")
            {
                fs::remove_file(&path).map_err(|e| {
                    MptDbError::Other(format!("remove wal file {}: {e}", path.display()))
                })?;
            }
        }
        Ok(())
    }

    fn next_append_segment_id(&self) -> u32 {
        match self.segments.last_key_value() {
            Some((&segment_id, range)) if range.entries < WAL_SEGMENT_ENTRY_LIMIT => segment_id,
            Some((&segment_id, _)) => segment_id + 1,
            None => 0,
        }
    }

    fn segment_path(&self, segment_id: u32) -> PathBuf {
        self.dir.join(format!("seg-{segment_id:08}.wal"))
    }

    fn write_segment_header(file: &mut File) -> Result<()> {
        file.write_all(WAL_SEGMENT_MAGIC)
            .map_err(|e| MptDbError::Other(format!("write wal segment magic: {e}")))?;
        file.write_all(&WAL_SEGMENT_FORMAT_VERSION.to_le_bytes())
            .map_err(|e| MptDbError::Other(format!("write wal segment version: {e}")))?;
        file.write_all(&[0u8; (WAL_SEGMENT_HEADER_LEN as usize) - 8 - 2])
            .map_err(|e| MptDbError::Other(format!("write wal segment padding: {e}")))?;
        Ok(())
    }

    /// Scan all segment files on disk, building the in-memory index.
    ///
    /// Only reads record headers (version + len + crc) — payload is not
    /// deserialized, so startup cost is proportional to the number of entries
    /// rather than their total payload size.
    fn scan_segments(
        dir: &Path,
    ) -> Result<(BTreeMap<i64, WalLocation>, BTreeMap<u32, WalSegmentRange>)> {
        let mut segment_ids = Vec::new();
        for entry in
            fs::read_dir(dir).map_err(|e| MptDbError::Other(format!("read wal dir: {e}")))?
        {
            let entry = entry.map_err(|e| MptDbError::Other(format!("read wal dir entry: {e}")))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(raw) = name.strip_prefix("seg-").and_then(|s| s.strip_suffix(".wal")) {
                if let Ok(segment_id) = raw.parse::<u32>() {
                    segment_ids.push(segment_id);
                }
            }
        }
        segment_ids.sort_unstable();

        let mut index = BTreeMap::new();
        let mut segments = BTreeMap::new();
        let mut expected_next_version: Option<i64> = None;

        for segment_id in segment_ids {
            let path = dir.join(format!("seg-{segment_id:08}.wal"));
            let mut file = File::open(&path).map_err(|e| {
                MptDbError::Other(format!("open wal segment {}: {e}", path.display()))
            })?;

            // Validate segment header.
            let mut header = [0u8; WAL_SEGMENT_HEADER_LEN as usize];
            file.read_exact(&mut header)
                .map_err(|e| MptDbError::Other(format!("read wal segment header: {e}")))?;
            if &header[..8] != WAL_SEGMENT_MAGIC {
                return Err(MptDbError::Other(format!(
                    "invalid wal segment magic in {}",
                    path.display()
                )));
            }
            let fmt_version = u16::from_le_bytes([header[8], header[9]]);
            if fmt_version != WAL_SEGMENT_FORMAT_VERSION {
                return Err(MptDbError::Other(format!(
                    "unsupported wal segment version {} in {}",
                    fmt_version,
                    path.display()
                )));
            }

            let mut first_version = 0i64;
            let mut last_version = 0i64;
            let mut entries = 0usize;
            let mut valid_end = WAL_SEGMENT_HEADER_LEN;

            loop {
                let offset = file
                    .stream_position()
                    .map_err(|e| MptDbError::Other(format!("tell wal segment: {e}")))?;

                // Read record header only: [version(8) | payload_len(4) | crc(4)]
                let mut hdr = [0u8; WAL_RECORD_HEADER_LEN];
                match file.read_exact(&mut hdr) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(err) => {
                        return Err(MptDbError::Other(format!(
                            "read wal record header in {}: {err}",
                            path.display()
                        )));
                    }
                }

                let record_version = i64::from_le_bytes(hdr[0..8].try_into().unwrap());
                let payload_len = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
                // CRC is verified on load_entry, not during scan.

                // Skip past the payload without reading it into memory.
                let payload_end = file
                    .seek(SeekFrom::Current(payload_len as i64))
                    .map_err(|e| MptDbError::Other(format!("skip wal record payload: {e}")))?;

                // Check that we actually had enough bytes (the seek above doesn't
                // fail if the file is shorter — it just moves the cursor past EOF).
                let file_len = file
                    .metadata()
                    .map_err(|e| MptDbError::Other(format!("stat wal segment: {e}")))?
                    .len();
                if payload_end > file_len {
                    // Incomplete record at end of file (crash during write).
                    // Stop here; the valid portion of this segment ends before
                    // this record.
                    break;
                }

                if let Some(expected) = expected_next_version {
                    if record_version != expected {
                        return Err(MptDbError::Other(format!(
                            "wal scan found non-contiguous version: expected {}, got {}",
                            expected, record_version
                        )));
                    }
                }
                expected_next_version = Some(record_version + 1);
                if entries == 0 {
                    first_version = record_version;
                }
                last_version = record_version;
                entries += 1;
                valid_end = payload_end;
                index.insert(record_version, WalLocation { segment_id, offset, len: payload_len });
            }

            if entries > 0 {
                segments.insert(
                    segment_id,
                    WalSegmentRange { first_version, last_version, entries, valid_end },
                );
            }
        }

        Ok((index, segments))
    }

    fn save_meta(&self) -> Result<()> {
        let tmp_path = self.meta_path.with_extension("tmp");
        let bytes = serde_json::to_vec(&self.meta)
            .map_err(|e| MptDbError::Other(format!("serialize wal meta: {e}")))?;
        let mut file = File::create(&tmp_path)
            .map_err(|e| MptDbError::Other(format!("create wal meta tmp: {e}")))?;
        file.write_all(&bytes)
            .map_err(|e| MptDbError::Other(format!("write wal meta tmp: {e}")))?;
        file.sync_data().map_err(|e| MptDbError::Other(format!("fdatasync wal meta tmp: {e}")))?;
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
            upgrades: vec![],
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
        for v in 1..=5 {
            wal.append_entry(&sample_entry(v)).unwrap();
        }
        wal.truncate_after(3).unwrap();

        assert_eq!(wal.earliest_version(), 1);
        assert_eq!(wal.latest_version(), 3);
        assert!(wal.load_entry(1).unwrap().is_some());
        assert!(wal.load_entry(2).unwrap().is_some());
        assert!(wal.load_entry(3).unwrap().is_some());
        assert!(wal.load_entry(4).unwrap().is_none());
        assert!(wal.load_entry(5).unwrap().is_none());
    }

    #[test]
    fn wal_truncate_after_preserves_across_segments() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        // Write 130 entries = 2 full segments (64 each) + 2 in a third.
        for v in 1..=130 {
            wal.append_entry(&sample_entry(v)).unwrap();
        }
        assert_eq!(wal.segments.len(), 3);

        // Truncate to version 70 (keeps seg-0 fully, seg-1 partially, deletes seg-2).
        wal.truncate_after(70).unwrap();
        assert_eq!(wal.earliest_version(), 1);
        assert_eq!(wal.latest_version(), 70);
        assert!(wal.load_entry(1).unwrap().is_some());
        assert!(wal.load_entry(64).unwrap().is_some());
        assert!(wal.load_entry(70).unwrap().is_some());
        assert!(wal.load_entry(71).unwrap().is_none());
        assert!(wal.load_entry(130).unwrap().is_none());

        // Verify we can still append after truncation.
        wal.append_entry(&sample_entry(71)).unwrap();
        assert_eq!(wal.latest_version(), 71);
        assert!(wal.load_entry(71).unwrap().is_some());
    }

    #[test]
    fn wal_prune_before_advances_earliest() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        for version in 1..=70 {
            wal.append_entry(&sample_entry(version)).unwrap();
        }
        wal.prune_before(69).unwrap();

        assert_eq!(wal.earliest_version(), 69);
        assert_eq!(wal.latest_version(), 70);
        assert!(wal.load_entry(68).unwrap().is_none());
        assert!(wal.load_entry(69).unwrap().is_some());
    }

    #[test]
    fn wal_prune_before_deletes_whole_segments() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        // 130 entries = seg-0 (1..=64), seg-1 (65..=128), seg-2 (129..=130)
        for v in 1..=130 {
            wal.append_entry(&sample_entry(v)).unwrap();
        }
        // Prune to 100: should delete seg-0, rewrite seg-1, keep seg-2.
        wal.prune_before(100).unwrap();

        assert_eq!(wal.earliest_version(), 100);
        assert_eq!(wal.latest_version(), 130);
        assert!(wal.load_entry(99).unwrap().is_none());
        assert!(wal.load_entry(100).unwrap().is_some());
        assert!(wal.load_entry(130).unwrap().is_some());

        // seg-0 file should be gone.
        assert!(!wal.segment_path(0).exists());
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

    #[test]
    fn wal_rotates_segments() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        for version in 1..=65 {
            wal.append_entry(&sample_entry(version)).unwrap();
        }

        let files = fs::read_dir(dir.path().join(WAL_DIR))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(files.iter().any(|name| name == "seg-00000000.wal"));
        assert!(files.iter().any(|name| name == "seg-00000001.wal"));
        assert_eq!(wal.load_entry(65).unwrap(), Some(sample_entry(65)));
    }

    #[test]
    fn wal_crc_detects_corruption() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        wal.append_entry(&sample_entry(1)).unwrap();

        // Corrupt one byte of the payload in the segment file.
        let seg_path = wal.segment_path(0);
        let mut data = fs::read(&seg_path).unwrap();
        let last = data.len() - 1;
        data[last] ^= 0xff;
        fs::write(&seg_path, &data).unwrap();

        let result = wal.load_entry(1);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("crc mismatch"), "unexpected error: {err_msg}");
    }

    #[test]
    fn wal_survives_incomplete_tail_on_reopen() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        wal.append_entry(&sample_entry(1)).unwrap();
        wal.append_entry(&sample_entry(2)).unwrap();

        // Simulate crash: append garbage (incomplete record header) at end.
        let seg_path = wal.segment_path(0);
        let mut file = OpenOptions::new().append(true).open(&seg_path).unwrap();
        file.write_all(&[0xde, 0xad]).unwrap();
        drop(file);

        // Reopen should ignore the incomplete tail.
        let reopened = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(reopened.latest_version(), 2);
        assert!(reopened.load_entry(1).unwrap().is_some());
        assert!(reopened.load_entry(2).unwrap().is_some());

        // Should be able to append after recovery from corrupted tail.
        let mut reopened = reopened;
        reopened.append_entry(&sample_entry(3)).unwrap();
        assert_eq!(reopened.latest_version(), 3);
        assert!(reopened.load_entry(3).unwrap().is_some());
    }

    #[test]
    fn wal_scan_skips_payload_deserialization() {
        let dir = TempDir::new().unwrap();
        let mut wal = CommitWalStore::open(dir.path()).unwrap();
        // Write entries with large payloads.
        for v in 1..=5 {
            let mut entry = sample_entry(v);
            entry.accounts = (0..100)
                .map(|i| CommitWalAccountChange {
                    address: Address::repeat_byte(i as u8),
                    hashed_address: B256::repeat_byte(i as u8),
                    info: Some(CommitWalAccountInfo {
                        nonce: i as u64,
                        balance: U256::from(i),
                        code_hash: B256::repeat_byte(i as u8),
                    }),
                    storage_wiped: false,
                    storage_known_empty: false,
                    storage_changes: vec![],
                })
                .collect();
            wal.append_entry(&entry).unwrap();
        }
        drop(wal);

        // Reopen — scan_segments should succeed (reads headers, skips payloads).
        let reopened = CommitWalStore::open(dir.path()).unwrap();
        assert_eq!(reopened.earliest_version(), 1);
        assert_eq!(reopened.latest_version(), 5);
        assert_eq!(reopened.index.len(), 5);
    }
}
