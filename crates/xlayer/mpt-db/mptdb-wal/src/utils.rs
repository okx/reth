use crate::log::{WalLog, WalLogOptions};
use mptdb_common::error::Result;
use std::path::{Path, PathBuf};

/// Return the changelog WAL directory path (dir/changelog).
pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("changelog")
}

/// Get the last written index of the WAL in the given directory.
/// Opens the WAL read-only (no background thread), reads last_index, closes.
pub fn get_last_index(dir: &Path) -> Result<u64> {
    let log = WalLog::open(dir, WalLogOptions { no_sync: true, ..Default::default() })?;
    Ok(log.last_index())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_path() {
        let p = log_path(Path::new("/some/dir"));
        assert_eq!(p, PathBuf::from("/some/dir/changelog"));
    }

    #[test]
    fn test_get_last_index() {
        let dir = tempfile::tempdir().unwrap();

        // Write 3 entries directly via WalLog.
        {
            let mut log =
                WalLog::open(dir.path(), WalLogOptions { no_sync: true, ..Default::default() })
                    .unwrap();
            log.write(1, b"entry-1").unwrap();
            log.write(2, b"entry-2").unwrap();
            log.write(3, b"entry-3").unwrap();
            log.close().unwrap();
        }

        let last = get_last_index(dir.path()).unwrap();
        assert_eq!(last, 3);
    }
}
