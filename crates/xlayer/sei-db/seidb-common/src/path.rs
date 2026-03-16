use crate::config::StateCommitConfig;
use std::path::{Path, PathBuf};

/// Returns the path to the MPT commit store database.
pub fn get_mpt_commit_store_path(home: &Path) -> PathBuf {
    home.join("data").join("mpt_committer.db")
}

/// Resolves the SC path for `SeiDb`. If `config.directory` is non-empty, uses that directly;
/// otherwise falls back to `get_mpt_commit_store_path(home)`.
pub fn resolve_sc_path(home: &Path, config: &StateCommitConfig) -> PathBuf {
    if !config.directory.is_empty() {
        PathBuf::from(&config.directory)
    } else {
        get_mpt_commit_store_path(home)
    }
}

/// Returns the path to the state store for the given backend.
pub fn get_state_store_path(home: &Path, backend: &str) -> PathBuf {
    home.join("data").join(backend)
}

/// Returns the path to the changelog directory under the given db path.
pub fn get_changelog_path(db_path: &Path) -> PathBuf {
    db_path.join("changelog")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_store_path() {
        let home = Path::new("/home/user");
        assert_eq!(
            get_state_store_path(home, "pebbledb"),
            PathBuf::from("/home/user/data/pebbledb")
        );
    }

    #[test]
    fn test_changelog_path() {
        let db_path = Path::new("/home/user/data/pebbledb");
        assert_eq!(
            get_changelog_path(db_path),
            PathBuf::from("/home/user/data/pebbledb/changelog")
        );
    }

    /// T1.1: get_mpt_commit_store_path produces home/data/mpt_committer.db
    #[test]
    fn t1_1_mpt_commit_store_path() {
        assert_eq!(
            get_mpt_commit_store_path(Path::new("/x")),
            PathBuf::from("/x/data/mpt_committer.db")
        );
    }

    /// T1.2: resolve_sc_path with empty directory falls back to mpt default
    #[test]
    fn t1_2_resolve_sc_path_default() {
        let config = StateCommitConfig::default(); // directory is empty
        assert_eq!(
            resolve_sc_path(Path::new("/home"), &config),
            PathBuf::from("/home/data/mpt_committer.db")
        );
    }

    /// T1.3: resolve_sc_path with explicit directory uses that path
    #[test]
    fn t1_3_resolve_sc_path_custom() {
        let config =
            StateCommitConfig { directory: "/custom/sc".to_string(), ..Default::default() };
        assert_eq!(resolve_sc_path(Path::new("/home"), &config), PathBuf::from("/custom/sc"));
    }
}
