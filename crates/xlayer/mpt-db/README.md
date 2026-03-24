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

## Running benchmarks and profiles

Benchmarks and profile tests live in `mptdb/tests/`.

### Allocator mode (important)

For mpt-db profile/benchmark runs, use `--features jemalloc` so allocator behavior is stable and comparable with production-like settings.

```bash
# Recommended: mpt-db lanes with jemalloc
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_5_mpt_only -- --ignored --nocapture --exact

# reth-only lanes keep existing command (no mpt-db allocator impact)
cargo test -p mptdb --release --test profile_mptdb_vs_reth profile_b4_5_reth_only -- --ignored --nocapture --exact
```

### Profile tests (detailed per-block breakdown)

Profile tests show granular per-phase timing (trie_load, slot_updates, storage_roots, account_updates, wal_append, etc.).

```bash
# B4.4 (~35s)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_4_mpt_only -- --ignored --nocapture --exact

# B4.5 (~2 min)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_5_mpt_only -- --ignored --nocapture --exact

# B4.6 (~6 min, needs ~80GB free disk)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_6_mpt_only -- --ignored --nocapture --exact

# Compare both reth and mpt-db side by side
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_5_single_run_compare -- --ignored --nocapture --exact
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_6_single_run_compare -- --ignored --nocapture --exact

# reth-only (skip mpt-db pre-pop to save disk)
cargo test -p mptdb --release --test profile_mptdb_vs_reth profile_b4_6_reth_only -- --ignored --nocapture --exact
```

### Benchmark tests (repeated runs, averaged)

```bash
# Single iteration (fast validation)
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_2_mpt_only -- --ignored --nocapture --exact
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_5_mpt_only -- --ignored --nocapture --exact

# Multiple iterations (stable average, default=3)
cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_4_mpt_only -- --ignored --nocapture --exact

# Compare reth vs mpt-db
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --test benchmark_mptdb_vs_reth bench_b4_5_reth_only -- --ignored --nocapture --exact
```

### Quick regression check

After any change to mpt-db core (`mptdb-sc/src/mpt/`):

```bash
# 1. Compile check
cargo check -p mptdb-sc --tests

# 2. Quick profile (B4.4, ~35s)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_4_mpt_only -- --ignored --nocapture --exact

# 3. Medium profile (B4.5, ~2 min)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_5_mpt_only -- --ignored --nocapture --exact
```

### B4.7 bottleneck diagnosis fields

The profile output includes split fields for commit hot spots:

- `storage_roots.fast_path_collect.extract`: collecting pre-computed roots/trie refs.
- `storage_roots.fast_path_collect.release`: dropping old storage trie bases (allocator free path).
- `wal_append.wal_lock_wait`: wait time before acquiring WAL mutex.
- `wal_append.wal_write`: append time after lock is acquired.

In current B4.7 runs, `fast_path_collect.release` is typically dominant while `wal_lock_wait` is near zero, indicating allocator/free behavior (not WAL lock contention) is the primary source of commit spikes.

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
| B4.5 | 1M | 10 | 5K | 10 | 1,211 ms | 164 ms | **7.4x** |
| B4.6 | 1M | 30 | 10K | 10 | 8,512 ms | 948 ms | **9.0x** |

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

**mpt-db (948 ms/block):**

| Phase | Time |
|-------|------|
| `trie_load` (L2 cache + L3 segment load) | 567 ms |
| `slot_updates` (apply + hash merged) | 142 ms |
| `account_updates` (serial account trie) | 51 ms |
| `storage_roots` (collect + parallel snapshot) | 58 ms |
| `wal_append` | 23 ms |
| `account_root` (parallel hash) | 13 ms |

L2 hits: 1,991/block, L3 hits: 7,957/block (cache_capacity=50K → 200K LRU limit, covers ~20% of 1M accounts).
