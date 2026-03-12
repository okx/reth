use crate::memiavl::commit_store::MemiavlCommitStore;
use seidb_common::error::{Result, SeiDbError};
use seidb_traits::sc::Exporter;

/// Creates a snapshot exporter for the given version by delegating to the
/// Cosmos (memiavl) commit store.
///
/// Validates that the version is in the range `[1, u32::MAX]` before
/// forwarding to the underlying exporter implementation.
pub fn create_exporter(cosmos: &MemiavlCommitStore, version: i64) -> Result<Box<dyn Exporter>> {
    if version <= 0 || version > u32::MAX as i64 {
        return Err(SeiDbError::Other(format!("invalid export version: {version}")));
    }
    cosmos.exporter(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seidb_common::config::MemIavlConfig;

    fn make_store() -> MemiavlCommitStore {
        MemiavlCommitStore::new("/tmp/test_exporter", MemIavlConfig::default())
    }

    #[test]
    fn test_exporter_version_validation() {
        let store = make_store();

        // Negative version
        let result = create_exporter(&store, -1);
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().unwrap());
        assert!(err_msg.contains("invalid export version"), "got: {err_msg}");

        // Version exceeding u32::MAX
        let result = create_exporter(&store, u32::MAX as i64 + 1);
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().unwrap());
        assert!(err_msg.contains("invalid export version"), "got: {err_msg}");
    }

    #[test]
    fn test_exporter_zero_version() {
        let store = make_store();
        let result = create_exporter(&store, 0);
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().unwrap());
        assert!(err_msg.contains("invalid export version"), "got: {err_msg}");
    }

    #[test]
    fn test_exporter_valid_version_delegates() {
        let store = make_store();
        // Valid version delegates to cosmos exporter. Since the store is not
        // opened (no DB loaded), it should return a "not opened" error.
        let result = create_exporter(&store, 1);
        assert!(result.is_err());
        let err_msg = format!("{}", result.err().unwrap());
        assert!(err_msg.contains("not opened"), "got: {err_msg}");
    }
}
