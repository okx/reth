# mpt-db

In-memory Merkle Patricia Trie (MPT) state commitment engine for Ethereum, designed as a high-performance replacement for reth's default MDBX-backed trie.

## Architecture

mpt-db keeps the account trie and storage tries resident in memory with a WAL (Write-Ahead Log) + mmap segment model for durability, inspired by [sei-db](https://github.com/sei-protocol/sei-db):

- **In-memory COW tries**: Account trie and storage tries use copy-on-write arenas. Modifications create overlay entries; the frozen base is shared via `Arc`.
- **WAL-first commits**: Block commits append a WAL entry (buffered, no fsync) and send trie data to a background worker. RocksDB is not on the critical path.
- **Published segments**: The background worker serializes storage tries into mmap-backed segment files. Reads go through L2 cache (in-memory handles) or L3 (mmap segments).
- **Merged apply+hash phase**: Storage slot updates and root hash computation run in a single rayon parallel pass, keeping trie data CPU-cache-hot.
- **Overlay capacity recycling**: After each block the working trie steals the cleared-but-capacity-holding overlay HashMaps from the previous base, eliminating per-block HashMap resize allocations.
- **Deferred handle drop**: Evicted L2 handles are dropped at the start of the next block's trie-load phase rather than on the commit critical path, removing ~400K small allocator frees from the hot path.
- **L3 overlay pre-allocation**: Storage tries loaded from published segments are pre-sized based on their page node count, eliminating HashMap resizes during path materialisation.
- **Multi-gen try_extend**: The published-view refresh fast path can fast-forward across multiple background-worker generations, preventing fallback to full chain rebuild under any worker lag.
- **Empty-trie lifecycle control**: Empty-storage handles activated during apply are removed from `storage_trie_handles` after commit, preventing unbounded map growth and LRU slot pollution.

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

### Profile output fields

The profile output includes granular sub-timers for commit hot spots:

- `storage_roots.fast_path_collect.extract`: collecting pre-computed roots/trie refs (should be ~0ms).
- `storage_roots.fast_path_collect.release`: dropping old storage trie handles. With overlay capacity recycling this is ~0ms.
- `storage_roots.fast_path_drop`: parallel in-place snapshot pass (freezes overlay into frozen base).
- `wal_append.wal_lock_wait`: wait before acquiring WAL mutex (should be ~0ms).
- `wal_append.wal_write`: actual buffered WAL write time.
- `persist`: time in `save_storage_version` including `wait_for_backpressure()` and sending committed tries to background worker.
- `cache_prep`: time for `cache_storage_trie` loop (LRU updates, `set_committed_base` for each dirty handle).

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
| B4.4 | 200K | 10 | 2K | 10 | 285 ms | 24 ms | **11.9x** |
| B4.5 | 1M | 10 | 5K | 10 | 1,211 ms | 83 ms | **14.6x** |
| B4.6 | 1M | 30 | 10K | 10 | 8,512 ms | 839 ms | **10.1x** |
| B4.7 | 500K mixed | 200 (30% contracts) | 1K mixed | 10 | 1,984 ms | 245 ms | **8.1x** |

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
- **Large datasets (B4.4–B4.6)**: reth's MDBX page cache becomes cold as the dataset exceeds cache capacity. Random reads from disk dominate reth's `overlay_root_with_updates` and `write_trie_updates`. mpt-db's in-memory tries are unaffected by dataset size, giving **10–15x** speedup for B4.4/B4.5.
- **Storage-heavy workload (B4.6)**: 1M accounts × 30 storage slots each. mpt-db completes in ~839ms. The dominant cost is L3 segment load (10K L2 misses/block): deferred handle drop removes ~195ms of synchronous allocator pressure from the critical path; L3 overlay pre-allocation eliminates per-trie HashMap resize chains.
- **Mainnet-realistic (B4.7)**: 500K mixed accounts (30% contracts with 200 slots, 70% EOA). Overlay capacity recycling + parallel hash for large tries + deferred handle drop + incremental published-view refresh brings B4.7 from ~395ms to ~245ms (-38%).

### B4.6 detailed breakdown

**reth (8,512 ms/block):**

| Phase | Time |
|-------|------|
| `overlay_root_with_updates` | 3,195 ms |
| `write_trie_updates` | 4,176 ms |
| `write_hashed_state` | 844 ms |
| `commit` | 276 ms |

**mpt-db (~839 ms/block):**

| Phase | Time | Notes |
|-------|------|-------|
| `trie_load` (L2 cache + L3 segment load) | ~630 ms | L2 hits: ~2K, L3 hits: ~8K/block |
| `slot_updates` (apply + hash merged) | ~155 ms | |
| `commit` | ~170 ms | |
| `storage_roots` (collect + parallel snapshot) | ~55 ms | `fast_path_drop` ~36ms (deferred to next block start) |
| `wal_append` | ~25 ms | |
| `account_updates` | ~65 ms | |
| `account_root` (parallel hash) | ~12 ms | |

L2 hits: ~2K/block, L3 hits: ~8K/block (cache_capacity=50K × 4 = 200K LRU limit).
Primary bottleneck is L3 `pub_open` (mmap page access for 8K tries/block) and `changes_preload`
(path materialisation). See `.claude/problems/b4_6_hotspot_code_analysis.md` for root cause
analysis and planned optimisations.
