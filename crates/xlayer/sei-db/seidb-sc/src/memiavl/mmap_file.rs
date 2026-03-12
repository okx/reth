use memmap2::Mmap;
use seidb_common::error::Result;
use std::{fs::File, path::Path};

/// A read-only memory-mapped file. Wraps `memmap2::Mmap` with MADV_RANDOM
/// advisory for the random-access patterns typical of IAVL tree traversal.
pub struct MmapFile {
    /// `None` when the source file was empty (memmap2 cannot mmap empty files).
    mmap: Option<Mmap>,
}

impl MmapFile {
    /// Creates an empty `MmapFile` with no backing file.
    /// `data()` returns `&[]` and `len()` returns 0.
    pub fn empty() -> Self {
        Self { mmap: None }
    }

    /// Opens the file at `path` and creates a read-only mmap.
    ///
    /// Empty files are handled gracefully: `data()` returns `&[]` and `len()` returns 0.
    /// For non-empty files, `MADV_RANDOM` is applied to disable kernel readahead,
    /// which suits the random access patterns of B+ tree traversal.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if meta.len() == 0 {
            return Ok(Self { mmap: None });
        }

        let mmap = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        mmap.advise(memmap2::Advice::Random).ok();
        Ok(Self { mmap: Some(mmap) })
    }

    /// Returns the mmap-ed data as a byte slice.
    pub fn data(&self) -> &[u8] {
        match &self.mmap {
            Some(m) => m,
            None => &[],
        }
    }

    /// Returns the length of the mapped region in bytes.
    pub fn len(&self) -> usize {
        self.data().len()
    }

    /// Returns `true` if the mapped region is empty (zero-length file).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_mmap_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.dat");
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(b"hello mmap world").unwrap();
        }
        let mf = MmapFile::open(&path).unwrap();
        assert_eq!(mf.data(), b"hello mmap world");
        assert_eq!(mf.len(), 16);
        assert!(!mf.is_empty());
    }

    #[test]
    fn test_mmap_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.dat");
        File::create(&path).unwrap(); // empty file
        let mf = MmapFile::open(&path).unwrap();
        assert!(mf.is_empty());
        assert_eq!(mf.len(), 0);
        assert_eq!(mf.data(), &[] as &[u8]);
    }

    #[test]
    fn test_mmap_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.dat");
        assert!(MmapFile::open(&path).is_err());
    }
}
