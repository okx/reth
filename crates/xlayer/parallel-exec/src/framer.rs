//! Transaction framer for grouping non-conflicting transactions.
//!
//! Uses ParaBloom to detect conflicts and assigns transactions to frames.
//! Transactions within the same frame can be executed in parallel.
