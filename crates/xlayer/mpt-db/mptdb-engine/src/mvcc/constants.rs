pub const VERSION_SIZE: usize = 8;
pub const LATEST_VERSION_KEY: &[u8] = b"s/_latest";
pub const EARLIEST_VERSION_KEY: &[u8] = b"s/_earliest";
pub const TOMBSTONE_VAL: &[u8] = b"TOMBSTONE";
pub const IMPORT_COMMIT_BATCH_SIZE: usize = 10000;
pub const PRUNE_COMMIT_BATCH_SIZE: usize = 50;
pub const DELETE_COMMIT_BATCH_SIZE: usize = 50;
pub const MIN_WAL_ENTRIES_TO_KEEP: u64 = 1000;
