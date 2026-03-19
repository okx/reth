use alloy_primitives::B256;
use alloy_trie::Nibbles;
use memmap2::{Mmap, MmapOptions};
use mptdb_common::error::{MptDbError, Result};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use super::{
    flat_layout::{
        data_path as pages_data_path, index_path as pages_index_path, load_page_index,
        open_data_file as open_pages_data_file, open_index_file as open_pages_index_file,
        read_page_header, FlatPageIndexEntry, PAGE_INDEX_RECORD_LEN, SHARED_PAGES_INDEX_MAGIC,
    },
    segment::{
        MappedSegmentPage, SegmentPageLease, StoragePathTrace, StorageSegmentLocator,
        StorageTrieSegmentReader,
    },
    storage_cow::StorageTrieCow,
};

const LATEST_INDEX_MAGIC: &[u8; 8] = b"lstidx03";
const INDEX_RECORD_LEN: usize = 1 + 32 + 32 + 8 + 4 + 2;
const INDEX_OP_PUT: u8 = 1;
const INDEX_OP_DELETE: u8 = 2;

pub struct LatestPathTraceLoaded {
    pub trace: StoragePathTrace,
    pub lookup_elapsed: Duration,
    pub materialize_elapsed: Duration,
}

pub struct LatestTriePageLoaded {
    pub lease: Arc<SegmentPageLease>,
    pub lookup_elapsed: Duration,
}

pub struct LatestTrieLoaded {
    pub trie: StorageTrieCow,
    pub lookup_elapsed: Duration,
}

struct FastStoreState {
    index_path: PathBuf,
    data_path: PathBuf,
    pages_index_path: PathBuf,
    index: HashMap<B256, StorageSegmentLocator>,
    index_file: File,
    data_file: File,
    pages_index_file: File,
    pages: HashMap<u64, FlatPageIndexEntry>,
    data_mmap: Option<Arc<Mmap>>,
}

/// Latest-only best-effort locator index over the shared storage segment data file.
pub struct FastStorageTrieStore {
    state: RwLock<FastStoreState>,
}

impl FastStorageTrieStore {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)
            .map_err(|e| MptDbError::Other(format!("create fast store dir: {e}")))?;

        let index_path = Self::index_path(dir);
        let data_path = Self::data_path(dir);
        let pages_index_path = Self::pages_index_path(dir);
        let mut index_file = Self::open_with_magic(&index_path, LATEST_INDEX_MAGIC)?;
        let data_file = open_pages_data_file(&data_path)?;
        let mut pages_index_file = open_pages_index_file(&pages_index_path)?;
        let pages_entries = load_page_index(&mut pages_index_file)?;
        let pages = pages_entries.into_iter().map(|entry| (entry.page_off, entry)).collect();
        let index = Self::load_index(&mut index_file)?;

        Ok(Self {
            state: RwLock::new(FastStoreState {
                index_path,
                data_path,
                pages_index_path,
                index,
                index_file,
                data_file,
                pages_index_file,
                pages,
                data_mmap: None,
            }),
        })
    }

    pub fn trace_touched_paths(
        &self,
        hashed_address: &B256,
        expected_root: B256,
        keys: &[Nibbles],
    ) -> Result<Option<LatestPathTraceLoaded>> {
        let loaded = match self.open_trie_page(hashed_address, expected_root)? {
            Some(loaded) => loaded,
            None => return Ok(None),
        };
        let reader = StorageTrieSegmentReader::open_shared_page(
            &loaded.lease,
            expected_root,
            loaded.lease.root_record_off(),
        )?;
        let materialize_start = std::time::Instant::now();
        let trace = reader.cursor().trace_paths(keys)?;
        let materialize_elapsed = materialize_start.elapsed();
        Ok(Some(LatestPathTraceLoaded {
            trace,
            lookup_elapsed: loaded.lookup_elapsed,
            materialize_elapsed,
        }))
    }

    pub fn open_trie_page(
        &self,
        hashed_address: &B256,
        expected_root: B256,
    ) -> Result<Option<LatestTriePageLoaded>> {
        let lookup_start = std::time::Instant::now();
        let (locator, page, mmap) = {
            let state = self.state.upgradable_read();
            let locator = match state.index.get(hashed_address).copied() {
                Some(locator) => locator,
                None => return Ok(None),
            };
            if locator.root != expected_root {
                return Ok(None);
            }

            let mut page = state.pages.get(&locator.page_off).copied();
            let mmap = state.data_mmap.clone();

            if page.is_none() || mmap.is_none() {
                let mut state = RwLockUpgradableReadGuard::upgrade(state);
                if page.is_none() {
                    Self::refresh_shared_pages(&mut state)?;
                    page = state.pages.get(&locator.page_off).copied();
                }
                let Some(page) = page else {
                    return Ok(None);
                };
                let mmap = match mmap {
                    Some(mmap) => mmap,
                    None => Arc::clone(Self::ensure_data_mmap(&mut state)?),
                };
                (locator, page, mmap)
            } else {
                (locator, page.expect("checked above"), mmap.expect("checked above"))
            }
        };
        let mapped = Arc::new(MappedSegmentPage::new(
            mmap,
            locator.page_off as usize,
            page.total_len as usize,
        ));
        let lease = Arc::new(SegmentPageLease::new(mapped, expected_root, locator.record_off));
        let page_header = read_page_header(lease.as_slice())?;
        if page_header.root != expected_root || page_header.root_record_off != locator.record_off {
            return Ok(None);
        }

        Ok(Some(LatestTriePageLoaded { lease, lookup_elapsed: lookup_start.elapsed() }))
    }

    pub fn open_trie(
        &self,
        hashed_address: &B256,
        expected_root: B256,
    ) -> Result<Option<LatestTrieLoaded>> {
        let loaded = match self.open_trie_page(hashed_address, expected_root)? {
            Some(loaded) => loaded,
            None => return Ok(None),
        };
        Ok(Some(LatestTrieLoaded {
            trie: StorageTrieCow::from_segment_page(loaded.lease),
            lookup_elapsed: loaded.lookup_elapsed,
        }))
    }

    pub(crate) fn apply_latest_updates(
        &self,
        puts: &[(B256, StorageSegmentLocator)],
        deletes: &[B256],
        new_pages: &[FlatPageIndexEntry],
    ) -> Result<()> {
        let mut state = self.state.write();
        for hashed_address in deletes {
            Self::append_index_record(
                &mut state.index_file,
                INDEX_OP_DELETE,
                *hashed_address,
                StorageSegmentLocator::new(B256::ZERO, 0, 0),
            )?;
            state.index.remove(hashed_address);
        }
        for (hashed_address, locator) in puts {
            Self::append_index_record(
                &mut state.index_file,
                INDEX_OP_PUT,
                *hashed_address,
                *locator,
            )?;
            state.index.insert(*hashed_address, *locator);
        }
        if !new_pages.is_empty() {
            for entry in new_pages {
                state.pages.insert(entry.page_off, *entry);
            }
            state.data_mmap = None;
        } else if !puts.is_empty() {
            Self::refresh_shared_pages(&mut state)?;
        }
        Ok(())
    }

    pub fn delete_latest(&self, hashed_address: &B256) -> Result<()> {
        self.apply_latest_updates(&[], &[*hashed_address], &[])
    }

    pub fn clear_memory(&self) {
        self.state.write().data_mmap = None;
    }

    pub fn snapshot_index(&self) -> HashMap<B256, StorageSegmentLocator> {
        self.state.read().index.clone()
    }

    pub fn replace_latest_index(
        &self,
        new_index: &HashMap<B256, StorageSegmentLocator>,
    ) -> Result<()> {
        let mut state = self.state.write();
        let tmp = state.index_path.with_extension("index.tmp");
        Self::write_full_index(&tmp, new_index)?;
        fs::rename(&tmp, &state.index_path)
            .map_err(|e| MptDbError::Other(format!("rename latest index: {e}")))?;
        state.index_file = Self::open_with_magic(&state.index_path, LATEST_INDEX_MAGIC)?;
        state.data_file = open_pages_data_file(&state.data_path)?;
        state.pages_index_file = open_pages_index_file(&state.pages_index_path)?;
        state.pages = load_page_index(&mut state.pages_index_file)?
            .into_iter()
            .map(|entry| (entry.page_off, entry))
            .collect();
        state.data_mmap = None;
        state.index = new_index.clone();
        Ok(())
    }

    pub(crate) fn index_path(dir: &Path) -> PathBuf {
        dir.join("latest.index")
    }

    pub(crate) fn pages_index_path(dir: &Path) -> PathBuf {
        pages_index_path(dir)
    }

    pub(crate) fn data_path(dir: &Path) -> PathBuf {
        pages_data_path(dir)
    }

    fn open_with_magic(path: &Path, magic: &[u8; 8]) -> Result<File> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| {
                MptDbError::Other(format!("open fast store file {}: {e}", path.display()))
            })?;

        let len = file
            .metadata()
            .map_err(|e| {
                MptDbError::Other(format!("stat fast store file {}: {e}", path.display()))
            })?
            .len();

        if len == 0 {
            file.write_all(magic)
                .map_err(|e| MptDbError::Other(format!("write fast store header: {e}")))?;
            file.flush().map_err(|e| MptDbError::Other(format!("flush fast store header: {e}")))?;
        } else {
            let mut header = [0u8; 8];
            file.seek(SeekFrom::Start(0))
                .map_err(|e| MptDbError::Other(format!("seek fast store header: {e}")))?;
            file.read_exact(&mut header)
                .map_err(|e| MptDbError::Other(format!("read fast store header: {e}")))?;
            if &header != magic {
                return Err(MptDbError::Other(format!(
                    "unexpected fast store header in {}",
                    path.display()
                )));
            }
        }

        Ok(file)
    }

    fn load_index(index_file: &mut File) -> Result<HashMap<B256, StorageSegmentLocator>> {
        index_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| MptDbError::Other(format!("seek latest index: {e}")))?;
        let mut bytes = Vec::new();
        index_file
            .read_to_end(&mut bytes)
            .map_err(|e| MptDbError::Other(format!("read latest index: {e}")))?;
        if bytes.len() < LATEST_INDEX_MAGIC.len() ||
            &bytes[..LATEST_INDEX_MAGIC.len()] != LATEST_INDEX_MAGIC
        {
            return Err(MptDbError::Other("invalid latest index header".to_string()));
        }

        let mut index = HashMap::new();
        let mut pos = LATEST_INDEX_MAGIC.len();
        while pos + INDEX_RECORD_LEN <= bytes.len() {
            let op = bytes[pos];
            pos += 1;
            let key = B256::from_slice(&bytes[pos..pos + 32]);
            pos += 32;
            let root = B256::from_slice(&bytes[pos..pos + 32]);
            pos += 32;

            let mut page_off_bytes = [0u8; 8];
            page_off_bytes.copy_from_slice(&bytes[pos..pos + 8]);
            let page_off = u64::from_le_bytes(page_off_bytes);
            pos += 8;

            let mut record_off_bytes = [0u8; 4];
            record_off_bytes.copy_from_slice(&bytes[pos..pos + 4]);
            let record_off = u32::from_le_bytes(record_off_bytes);
            pos += 4;
            let mut version_bytes = [0u8; 2];
            version_bytes.copy_from_slice(&bytes[pos..pos + 2]);
            let format_version = u16::from_le_bytes(version_bytes);
            pos += 2;

            match op {
                INDEX_OP_PUT => {
                    index.insert(
                        key,
                        StorageSegmentLocator { root, page_off, record_off, format_version },
                    );
                }
                INDEX_OP_DELETE => {
                    index.remove(&key);
                }
                _ => {}
            }
        }

        Ok(index)
    }

    fn append_index_record(
        index_file: &mut File,
        op: u8,
        key: B256,
        locator: StorageSegmentLocator,
    ) -> Result<()> {
        index_file
            .seek(SeekFrom::End(0))
            .map_err(|e| MptDbError::Other(format!("seek latest index append: {e}")))?;
        let mut record = [0u8; INDEX_RECORD_LEN];
        let mut pos = 0usize;
        record[pos] = op;
        pos += 1;
        record[pos..pos + 32].copy_from_slice(key.as_slice());
        pos += 32;
        record[pos..pos + 32].copy_from_slice(locator.root.as_slice());
        pos += 32;
        record[pos..pos + 8].copy_from_slice(&locator.page_off.to_le_bytes());
        pos += 8;
        record[pos..pos + 4].copy_from_slice(&locator.record_off.to_le_bytes());
        pos += 4;
        record[pos..pos + 2].copy_from_slice(&locator.format_version.to_le_bytes());
        index_file
            .write_all(&record)
            .map_err(|e| MptDbError::Other(format!("append latest index record: {e}")))?;
        index_file
            .flush()
            .map_err(|e| MptDbError::Other(format!("flush latest index record: {e}")))?;
        Ok(())
    }

    fn write_full_index(path: &Path, index: &HashMap<B256, StorageSegmentLocator>) -> Result<()> {
        let mut file = File::create(path)
            .map_err(|e| MptDbError::Other(format!("create latest index tmp: {e}")))?;
        file.write_all(LATEST_INDEX_MAGIC)
            .map_err(|e| MptDbError::Other(format!("write latest index header: {e}")))?;
        for (key, locator) in index {
            let mut record = [0u8; INDEX_RECORD_LEN];
            let mut pos = 0usize;
            record[pos] = INDEX_OP_PUT;
            pos += 1;
            record[pos..pos + 32].copy_from_slice(key.as_slice());
            pos += 32;
            record[pos..pos + 32].copy_from_slice(locator.root.as_slice());
            pos += 32;
            record[pos..pos + 8].copy_from_slice(&locator.page_off.to_le_bytes());
            pos += 8;
            record[pos..pos + 4].copy_from_slice(&locator.record_off.to_le_bytes());
            pos += 4;
            record[pos..pos + 2].copy_from_slice(&locator.format_version.to_le_bytes());
            file.write_all(&record)
                .map_err(|e| MptDbError::Other(format!("write latest index record: {e}")))?;
        }
        file.flush().map_err(|e| MptDbError::Other(format!("flush latest index tmp: {e}")))?;
        Ok(())
    }

    fn ensure_data_mmap(state: &mut FastStoreState) -> Result<&Arc<Mmap>> {
        if state.data_mmap.is_none() {
            let mmap = unsafe {
                MmapOptions::new()
                    .map(&state.data_file)
                    .map_err(|e| MptDbError::Other(format!("mmap latest flat data: {e}")))?
            };
            state.data_mmap = Some(Arc::new(mmap));
        }
        Ok(state.data_mmap.as_ref().unwrap())
    }

    fn refresh_shared_pages(state: &mut FastStoreState) -> Result<()> {
        let start = state
            .pages_index_file
            .seek(SeekFrom::Current(0))
            .map_err(|e| MptDbError::Other(format!("seek latest pages index cursor: {e}")))?
            .max(SHARED_PAGES_INDEX_MAGIC.len() as u64);
        let file_len = state
            .pages_index_file
            .metadata()
            .map_err(|e| MptDbError::Other(format!("stat latest pages index: {e}")))?
            .len();
        if file_len <= start {
            return Ok(());
        }

        state
            .pages_index_file
            .seek(SeekFrom::Start(start))
            .map_err(|e| MptDbError::Other(format!("seek latest pages index tail: {e}")))?;
        let mut bytes = Vec::with_capacity((file_len - start) as usize);
        state
            .pages_index_file
            .read_to_end(&mut bytes)
            .map_err(|e| MptDbError::Other(format!("read latest pages index tail: {e}")))?;

        let mut pos = 0usize;
        while pos + PAGE_INDEX_RECORD_LEN <= bytes.len() {
            let page_off = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
            pos += 8;
            let total_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let root = B256::from_slice(&bytes[pos..pos + 32]);
            pos += 32;
            let root_record_off = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            let layout_version = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
            pos += 2;
            let feature_flags = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
            pos += 2;
            let checksum = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;

            state.pages.insert(
                page_off,
                FlatPageIndexEntry {
                    page_off,
                    total_len,
                    root,
                    root_record_off,
                    layout_version,
                    feature_flags,
                    checksum,
                },
            );
        }

        state
            .pages_index_file
            .seek(SeekFrom::Start(start + pos as u64))
            .map_err(|e| MptDbError::Other(format!("rewind latest pages index cursor: {e}")))?;

        if pos > 0 {
            state.data_mmap = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256};
    use alloy_trie::Nibbles;
    use tempfile::TempDir;

    use crate::mpt::{flat_layout, tree::MptTree, StorageTrieSegment};

    #[test]
    fn test_latest_segment_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(dir.path()).unwrap();

        let mut tree = MptTree::new();
        let key = Nibbles::unpack(B256::with_last_byte(1));
        tree.insert(&key, alloy_rlp::encode(U256::from(7u64)));
        let (root, _) = tree.root_hash_and_dirty_blobs();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();

        let data_path = FastStorageTrieStore::data_path(dir.path());
        let pages_index_path = FastStorageTrieStore::pages_index_path(dir.path());
        let mut data_file = open_pages_data_file(&data_path).unwrap();
        let mut pages_index_file = open_pages_index_file(&pages_index_path).unwrap();
        let entry = flat_layout::append_page(
            &mut data_file,
            &mut pages_index_file,
            segment.as_bytes(),
            root,
            segment.root_record_off(),
        )
        .unwrap();
        let locator = StorageSegmentLocator::new(root, entry.page_off, entry.root_record_off);
        store
            .apply_latest_updates(&[(B256::with_last_byte(0x11), locator)], &[], &[entry])
            .unwrap();
        store.replace_latest_index(&store.snapshot_index()).unwrap();
        store.clear_memory();

        let loaded = store
            .trace_touched_paths(&B256::with_last_byte(0x11), root, &[key.clone()])
            .unwrap()
            .expect("latest segment should materialize");
        assert_eq!(loaded.trace.into_tree().root_hash(), root);
    }

    #[test]
    fn test_delete_latest() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(dir.path()).unwrap();

        let mut tree = MptTree::new();
        let key = Nibbles::unpack(B256::with_last_byte(1));
        tree.insert(&key, alloy_rlp::encode(U256::from(7u64)));
        let (root, _) = tree.root_hash_and_dirty_blobs();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();

        let data_path = FastStorageTrieStore::data_path(dir.path());
        let pages_index_path = FastStorageTrieStore::pages_index_path(dir.path());
        let mut data_file = open_pages_data_file(&data_path).unwrap();
        let mut pages_index_file = open_pages_index_file(&pages_index_path).unwrap();
        let entry = flat_layout::append_page(
            &mut data_file,
            &mut pages_index_file,
            segment.as_bytes(),
            root,
            segment.root_record_off(),
        )
        .unwrap();
        let locator = StorageSegmentLocator::new(root, entry.page_off, entry.root_record_off);
        let addr = B256::with_last_byte(0x22);
        store.apply_latest_updates(&[(addr, locator)], &[], &[entry]).unwrap();
        store.replace_latest_index(&store.snapshot_index()).unwrap();
        store.delete_latest(&addr).unwrap();
        assert!(store.trace_touched_paths(&addr, root, &[key]).unwrap().is_none());
    }

    #[test]
    fn test_open_trie_page_reuses_mmap_bytes() {
        let dir = TempDir::new().unwrap();
        let store = FastStorageTrieStore::open(dir.path()).unwrap();

        let mut tree = MptTree::new();
        let key = Nibbles::unpack(B256::with_last_byte(3));
        tree.insert(&key, alloy_rlp::encode(U256::from(9u64)));
        let (root, _) = tree.root_hash_and_dirty_blobs();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();

        let data_path = FastStorageTrieStore::data_path(dir.path());
        let pages_index_path = FastStorageTrieStore::pages_index_path(dir.path());
        let mut data_file = open_pages_data_file(&data_path).unwrap();
        let mut pages_index_file = open_pages_index_file(&pages_index_path).unwrap();
        let entry = flat_layout::append_page(
            &mut data_file,
            &mut pages_index_file,
            segment.as_bytes(),
            root,
            segment.root_record_off(),
        )
        .unwrap();
        let locator = StorageSegmentLocator::new(root, entry.page_off, entry.root_record_off);
        let addr = B256::with_last_byte(0x33);
        store.apply_latest_updates(&[(addr, locator)], &[], &[entry]).unwrap();
        store.replace_latest_index(&store.snapshot_index()).unwrap();
        store.clear_memory();

        let loaded = store.open_trie_page(&addr, root).unwrap().unwrap();
        let expected_ptr = {
            let state = store.state.read();
            let mmap = state.data_mmap.as_ref().unwrap();
            unsafe { mmap.as_ptr().add(entry.page_off as usize) }
        };
        assert_eq!(loaded.lease.as_ptr(), expected_ptr);
    }
}
