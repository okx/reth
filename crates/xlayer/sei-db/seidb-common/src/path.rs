use std::path::{Path, PathBuf};

/// Returns the path to the commit store database.
pub fn get_commit_store_path(home: &Path) -> PathBuf {
    home.join("data").join("committer.db")
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
    fn test_commit_store_path() {
        let home = Path::new("/home/user");
        assert_eq!(get_commit_store_path(home), PathBuf::from("/home/user/data/committer.db"));
    }

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
}
