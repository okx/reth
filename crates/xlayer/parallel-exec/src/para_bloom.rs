//! Parallel Bloom filter for fast read/write conflict detection.
//!
//! Each execution frame maintains two Bloom filters (read set + write set).
//! Used by the Framer to quickly determine which frames conflict with a new transaction.
