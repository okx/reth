# mpt-db

In-memory Merkle Patricia Trie (MPT) state commitment engine for Ethereum, designed as a high-performance replacement for reth's default MDBX-backed trie.

## Architecture

mpt-db keeps the account trie and storage tries resident in memory with a WAL (Write-Ahead Log) + mmap segment model for durability, inspired by [sei-db](https://github.com/sei-protocol/sei-db):

- **In-memory COW tries**: Account trie and storage tries use copy-on-write arenas. Modifications create overlay entries; the frozen base is shared via `Arc`.
- **WAL-first commits**: Block commits append a WAL entry (buffered, no fsync) and send trie data to a background worker. RocksDB is not on the critical path.
- **Published segments**: The background worker serializes storage tries into mmap-backed segment files. Reads go through L2 cache (in-memory handles) or L3 (mmap segments).
- **Merged apply+hash phase**: Storage slot updates and root hash computation run in a single rayon parallel pass, keeping trie data CPU-cache-hot.

### Crate structure

| Crate | Description |
|-------|-------------|
| `mptdb-sc` | Core state commitment engine (trie, WAL, segments, commit store) |
| `mptdb-engine` | RocksDB/MDBX storage backend for persisted trie nodes |
| `mptdb-common` | Shared error types and utilities |
| `mptdb-traits` | Trait definitions for the commit store |
| `mptdb-wal` | WAL segment format and replay |
| `mptdb-proto` | Protobuf definitions |
| `mptdb-ss` | State store layer |
| `mptdb-ledger` | Ledger integration |
| `mptdb` | Top-level facade crate |

## Running benchmarks

### Criterion benchmarks (B4.1–B4.6)

Small-scale (B4.1–B4.3):

```bash
cargo bench -p xlayer-salt --bench mptdb_vs_reth
```

Large-scale (B4.4–B4.6, requires more time and memory):

```bash
BENCH_LARGE=1 cargo bench -p xlayer-salt --bench mptdb_vs_reth
```

Run a single benchmark:

```bash
cargo bench -p xlayer-salt --bench mptdb_vs_reth -- "B4.3"
BENCH_LARGE=1 cargo bench -p xlayer-salt --bench mptdb_vs_reth -- "B4.5"
```

### Profile tests (detailed per-block breakdown)

```bash
# B4.4
cargo test -p xlayer-salt --release --test profile_mptdb_vs_reth profile_b4_4_single_run_compare -- --ignored --nocapture

# B4.5
cargo test -p xlayer-salt --release --test profile_mptdb_vs_reth profile_b4_5_single_run_compare -- --ignored --nocapture

# B4.6 (needs ~100GB free disk space)
cargo test -p xlayer-salt --release --test profile_mptdb_vs_reth profile_b4_6_single_run_compare -- --ignored --nocapture
```

## Benchmark results

All benchmarks use **chunked incremental pre-population** for both reth and mpt-db, matching real blockchain state accumulation (one block at a time, no batch shortcuts).

Machine: Apple M-series, 32GB RAM, SSD.

### Summary

Each updated account has its nonce and balance modified, plus **all** of its storage slots rewritten. For example, B4.6 with 10K accounts updated × 30 slots = 300K storage slot changes per block.

| Test | Pre-pop accounts | Storage slots per account | Accounts updated per block | Blocks | reth per-block | mpt-db per-block | Speedup |
|------|-----------------|--------------------------|---------------------------|--------|---------------|-----------------|---------|
| B4.1 | 0 (fresh) | 10 | 100 | 1 | 1.26 ms | 1.31 ms | 1.0x |
| B4.2 | 1K | 10 | 200 | 1 | 5.73 ms | 2.98 ms | **1.9x** |
| B4.3 | 1K | 10 | 200 | 10 | 5.39 ms | 2.38 ms | **2.3x** |
| B4.4 | 200K | 10 | 2K | 10 | 285 ms | 39.6 ms | **7.2x** |
| B4.5 | 1M | 10 | 5K | 10 | 1,211 ms | 191 ms | **6.3x** |
| B4.6 | 1M | 30 | 10K | 10 | 8,512 ms | 946 ms | **9.0x** |

### Workload vs real-world comparison

| Test | Slot changes/block | Real-world equivalent |
|------|-------------------|----------------------|
| B4.3 | 2K | Light L1 block |
| B4.4 | 20K | Typical Ethereum mainnet block |
| B4.5 | 50K | Busy mainnet / moderate L2 |
| B4.6 | 300K | High-throughput L2 / stress test |

Ethereum L1 mainnet: ~150–300 txns/block, ~5K–20K storage slot changes. B4.4–B4.5 are the most representative of current mainnet workloads. B4.6 targets future high-throughput scenarios (increased gas limit, L2 sequencers).

### Analysis

- **Small datasets (B4.1–B4.3)**: reth's MDBX keeps everything in page cache, so both systems are fast. mpt-db's advantage is modest (1–2x) because the WAL append overhead is a significant fraction of the per-block time.
- **Large datasets (B4.4–B4.6)**: reth's MDBX page cache becomes cold as the dataset exceeds cache capacity. Random reads from disk dominate reth's `overlay_root_with_updates` and `write_trie_updates`. mpt-db's in-memory tries are unaffected by dataset size, giving **7–9x** speedup.
- **Storage-heavy workload (B4.6)**: 1M accounts × 30 storage slots each creates a ~4GB working set. reth spends 3.2s on root computation + 4.2s writing trie updates. mpt-db's merged apply+hash phase completes in 946ms total.

### B4.6 detailed breakdown

**reth (8,512 ms/block):**

| Phase | Time |
|-------|------|
| `overlay_root_with_updates` | 3,195 ms |
| `write_trie_updates` | 4,176 ms |
| `write_hashed_state` | 844 ms |
| `commit` | 276 ms |

**mpt-db (946 ms/block):**

| Phase | Time |
|-------|------|
| `trie_load` (L2 cache + L3 segment load) | 533 ms |
| `slot_updates` (apply + hash merged) | 149 ms |
| `account_updates` (serial account trie) | 52 ms |
| `wal_append` | 27 ms |
| `storage_roots` (collect pre-computed) | 18 ms |
| `account_root` (parallel hash) | 13 ms |
