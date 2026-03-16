use alloy_primitives::B256;
use revm_database::BundleState;
use seidb_common::error::Result;

/// Independent MPT commit engine trait.
///
/// Each block: `apply_bundle_state()` then `commit()`.
/// Recovery: `load_version()` reloads from disk.
pub trait MptCommitter: Send {
    /// Apply a single block's final BundleState. Must be called exactly once per block.
    fn apply_bundle_state(&mut self, bundle: &BundleState) -> Result<()>;

    /// Persist current state, returning `(new_version, state_root)`.
    fn commit(&mut self) -> Result<(i64, B256)>;

    /// Current committed version.
    fn version(&self) -> i64;

    /// Reload latest committed version from disk manifest.
    fn load_version(&mut self) -> Result<()>;

    /// Rollback to a historical version (truncates manifest, rebuilds working state).
    fn rollback(&mut self, target_version: i64) -> Result<()>;

    /// Release resources (DB handle, file lock).
    fn close(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T1.1: trait object safety check
    #[test]
    fn t1_1_trait_object_safety() {
        fn _assert_object_safe(_: &dyn MptCommitter) {}
    }
}
