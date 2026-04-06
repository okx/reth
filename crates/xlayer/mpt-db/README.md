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

### WAL-first segment materialization strategy

- Default (`MptConfig::wal_first_defer_segment_build = true`): in wal-first mode, storage segment serialization is deferred to the background persist worker.
- Sparse apply path uses sparse trie snapshots for background materialization, so frontend `segment_build` stays off the commit hot path while published segments are still backfilled.
- Diagnostic override: `MPT_WAL_DEFER_SPARSE_SEGMENT_BUILD=0` forces foreground sparse segment build for A/B checks.

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

# B4.7 (~3 min)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_7_mainnet_realistic_mpt_only -- --ignored --nocapture --exact

# B4.8 (~3 min, provider-aligned ERC20 pool workload)
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_8_integration_scale_mpt_only -- --ignored --nocapture --exact

# Compare both reth and mpt-db side by side
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_5_single_run_compare -- --ignored --nocapture --exact
cargo test -p mptdb --release --features jemalloc --test profile_mptdb_vs_reth profile_b4_6_single_run_compare -- --ignored --nocapture --exact

# reth-only (skip mpt-db pre-pop to save disk)
cargo test -p mptdb --release --test profile_mptdb_vs_reth profile_b4_6_reth_only -- --ignored --nocapture --exact
cargo test -p mptdb --release --test profile_mptdb_vs_reth profile_b4_8_integration_scale_reth_only -- --ignored --nocapture --exact
```

### Benchmark tests (repeated runs, averaged)

```bash
# Single iteration (fast validation)
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_2_mpt_only -- --ignored --nocapture --exact
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_5_mpt_only -- --ignored --nocapture --exact
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_8_integration_scale_mpt_only -- --ignored --nocapture --exact

# Multiple iterations (stable average, default=3)
cargo test -p mptdb --release --features jemalloc --test benchmark_mptdb_vs_reth bench_b4_4_mpt_only -- --ignored --nocapture --exact

# Compare reth vs mpt-db
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --test benchmark_mptdb_vs_reth bench_b4_5_reth_only -- --ignored --nocapture --exact
MPT_BENCH_ITERS=1 cargo test -p mptdb --release --test benchmark_mptdb_vs_reth bench_b4_8_integration_scale_reth_only -- --ignored --nocapture --exact
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
Latest refresh: April 6, 2026 (`USE_SPARSE=1`, default allocator, `MPT_BENCH_ITERS=1` for B4.2-B4.5 benchmarks; one-shot profile for B4.6-B4.8; provider integration stress for B5.0/B6.0).
Note: this refresh reran mpt-db for B4.2-B4.8; reth values in the table remain the last recorded baseline.

### Summary

For B4.1-B4.7, each updated account has its nonce and balance modified, plus all configured storage slots rewritten.  
B4.8 is different: it is tx-style ERC20 pool traffic (provider-aligned), so writes are sparse/incremental rather than full-slot rewrites.

| Test | Pre-pop accounts | Storage slots per account | Accounts updated per block | Blocks | reth per-block | mpt-db per-block | Speedup |
|------|-----------------|--------------------------|---------------------------|--------|---------------|-----------------|---------|
| B4.1 | 0 (fresh) | 10 | 100 | 1 | 1.26 ms | 1.31 ms | 1.0x |
| B4.2 | 1K | 10 | 200 | 1 | 5.73 ms | 2.9 ms | **2.0x** |
| B4.3 | 1K | 10 | 200 | 10 | 5.39 ms | 1.7 ms | **3.2x** |
| B4.4 | 200K | 10 | 2K | 10 | 285 ms | 21.2 ms | **13.4x** |
| B4.5 | 1M | 10 | 5K | 10 | 1,211 ms | 41.8 ms | **29.0x** |
| B4.6 | 1M | 30 | 10K | 10 | 8,512 ms | 104.9 ms | **81.1x** |
| B4.7 | 500K mixed | 200 (30% contracts) | 1K mixed | 10 | 1,984 ms | 15.2 ms | **130.5x** |
| B4.8 | 500K mixed | 128 (30% contracts, active pool 10%) | 50K tx-style updates | 10 | 12,172.2 ms | 136.9 ms | **88.9x** |

### Workload vs real-world comparison

| Test | Slot changes/block | Real-world equivalent |
|------|-------------------|----------------------|
| B4.3 | 2K | Light L1 block |
| B4.4 | 20K | Typical Ethereum mainnet block |
| B4.5 | 50K | Busy mainnet / moderate L2 |
| B4.6 | 300K | High-throughput L2 / stress test |
| B4.8 | ~100K slot updates + 50K sender nonce updates | Provider-aligned ERC20 transfer pool |

Ethereum L1 mainnet: ~150–300 txns/block, ~5K–20K storage slot changes. B4.4-B4.5 are the most representative of current mainnet workloads. B4.6 targets future high-throughput scenarios (increased gas limit, L2 sequencers). B4.8 is the provider-aligned integration workload (`erc20_transfer_10pct_contract_pool` style).

### Provider integration stress (B5.0/B5.1/B5.2/B6.0)

These are from `mptdb-provider/benches/block_execution.rs` (full provider integration lifecycle, not SC-only micro profile).

Per-block transaction type for B5.0/B5.1/B5.2/B6.0:

- ERC20 `transfer(address,uint256)` contract call (`TxKind::Call`)
- `value = 0`, `gas_limit = 100_000`, `amount = 100`
- contract selected from active pool; sender selected from prefilled holder set

Command examples:

```bash
# B5.0
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
cargo bench --bench block_execution -p mptdb-provider -- "b5_0_peak_integration_10x"

# B6.0
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
cargo bench --bench block_execution -p mptdb-provider -- "b6_0_peak_integration_20x"
```

Latest run snapshot (April 6, 2026):

| Test | Pre-pop accounts | Tx/block | Blocks | Contract ratio | KV/contract | Active contract pool | Tx type | reth per-block | mpt-db per-block | Speedup |
|------|------------------|----------|--------|----------------|------------|----------------------|---------|----------------|------------------|---------|
| B5.0 | 1,000,000 mixed | 20,000 | 10 | 30% | 64 | 10% (pool 30,000) | ERC20 transfer pool | 815–826 ms | 320–332 ms | **~2.5x** |
| B5.1 | 1,000,000 mixed | 10,000 | 10 | 30% | 64 | 10% (pool 30,000) | ERC20 transfer pool | 550.92–556.13 ms | 170.58–173.88 ms | **~3.2x** |
| B5.2 | 1,000,000 mixed | 50,000 | 10 | 30% | 64 | 10% (pool 30,000) | ERC20 transfer pool | 1.2967–1.3262 s | 671.74–684.60 ms | **~1.9x** |
| B6.0 | 2,000,000 mixed | 50,000 | 10 | 30% | 64 | 10% (pool 60,000) | ERC20 transfer pool | 10.73–12.43 s | 3.12–3.64 s | **~3.4x** |

### Analysis

- **Small datasets (B4.1–B4.3)**: reth's MDBX keeps everything in page cache, so both systems are fast. mpt-db's advantage is modest (1–2x) because the WAL append overhead is a significant fraction of the per-block time.
- **Large datasets (B4.4–B4.6)**: reth's MDBX page cache becomes cold as the dataset exceeds cache capacity. Random reads from disk dominate reth's `overlay_root_with_updates` and `write_trie_updates`. mpt-db's in-memory tries are unaffected by dataset size, giving **10–15x** speedup for B4.4/B4.5.
- **Storage-heavy workload (B4.6)**: 1M accounts × 30 storage slots each. latest mpt-db run is ~104.9ms/block (`apply_bundle_state` ~72.9ms, `commit` ~32.1ms, `account_root` ~10.7ms).
- **Mainnet-realistic (B4.7)**: 500K mixed accounts (30% contracts with 200 slots, 70% EOA). latest mpt-db run is ~15.2ms/block (`account_root` ~1.5ms).
- **Provider-aligned ERC20 pool (B4.8)**: 500K mixed accounts (30% contracts, 128 pre-pop holders per contract, active contract pool 10%), 50K tx-style updates per block. latest mpt-db run is ~136.9ms/block (`apply_bundle_state` ~85.6ms, `commit` ~51.3ms, `account_root` ~26.1ms).

### B4.6 detailed breakdown (latest mpt-db profile)

**mpt-db (104.9 ms/block):**

| Phase | Time |
|-------|------|
| `apply_bundle_state` | 72.9 ms |
| `commit` | 32.1 ms |
| `storage_roots` | 8.8 ms |
| `account_updates` | 6.5 ms |
| `account_root` | 10.7 ms |
| `cache_publish` | 0.3 ms |
