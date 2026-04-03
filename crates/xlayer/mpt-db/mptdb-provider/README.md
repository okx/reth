# mptdb-provider Design Notes (Plan C)

This directory contains the reth integration adapter for mpt-db SC (state-commit engine).

Goal: keep reth execution/read path stable on MDBX PlainState, while replacing reth MPT root/storage path with mptdb-sc.

## 1. Architecture in one page

Plan C split:

- EVM reads (`basic_account`, `storage`, `bytecode`, `block_hash`):
  - from reth fallback provider (MDBX PlainState path)
- State root + account/storage proof:
  - from `MptCommitStore` (SC)
- State writes:
  - `MptDbStateWriter` writes SC
  - reth native writer still writes MDBX PlainState tables in its own pipeline

Code entry points:

- `src/provider.rs`: `MptDbStateProvider`
- `src/factory.rs`: `MptDbStateProviderFactory`
- `src/writer.rs`: `MptDbStateWriter`
- `benches/block_execution.rs`: integration-style block lifecycle benchmark

## 2. Read/Write ownership (important)

Read ownership:

- `basic_account` / `storage` delegate to `fallback` provider
- `state_root` delegates to SC
- `proof` / `storage_proof` / `storage_root` delegate to SC

Write ownership:

- `MptDbStateWriter::write_state` -> `sc.apply_bundle_state()` + `sc.commit()`
- MDBX writes (PlainState + history tables) are done by reth provider path
- Target architecture: MDBX does **not** maintain trie tables in mptdb lane
  (`overlay_root_with_updates + write_trie_updates` is not in the hot path)
- `commit_with_external_root` is an experiment/debug interface, not the target
  integration write path

## 3. Version model

SC version maps to block number as:

- `sc.version = block_number + 1`
- fresh DB has `version = 0`

`MptDbStateProvider::state_root()` currently requires `self.version == sc.version()` (latest only). Historical per-version state root in SC is not implemented as full snapshot semantics yet.

## 4. Prewarm model

`ScPrewarmDispatcher` (in `provider.rs`) is best-effort async prewarm:

- Triggered after block commit boundary
- Inputs are deduplicated hashed addresses
- Worker does `maybe_refresh_published_view_for_prewarm()` then per-account `prewarm_storage_trie_by_hashed_address()`
- Uses `try_lock` and can skip when SC is busy
- Not part of EVM read hot path

So prewarm helps next-block SC operations; it is not a read-path acceleration inside a running block.

## 5. Benchmark semantics (`benches/block_execution.rs`)

This is the main integration comparison for "one block lifecycle" behavior.

Two lanes:

- `mptdb` lane:
  - EVM reads default: direct MDBX provider
  - optional provider-read mode: wrap reads through `MptDbStateProvider` (`MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1`)
  - per block: EVM -> parallel { SC `write_state` (apply+commit/root), MDBX `write_state+commit` } -> wait MDBX done
- `reth_mdbx` lane:
  - EVM reads from MDBX
  - serial `write_state + overlay_root_with_updates + write_trie_updates + commit`

Key env knobs:

- `MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle|end_to_end`
- `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1` (measure wrapper/lock overhead)
- `MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1`
- `MPTDB_BENCH_LEGACY_SC=1` (force legacy non-wal-first path)
- `MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=0|1` (diagnostic A/B: serial vs parallel MDBX writer)
- `MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full|plain|noop` (diagnostic isolation of MDBX write path)
- `MPTDB_PROVIDER_BENCH_SYNC_PREWARM_AFTER_BLOCK=1` (diagnostic only: force per-block sync prewarm+flush)
- `MPTDB_PROVIDER_BENCH_FLUSH_AFTER_PREPOP=0|1` (controls flush after genesis pre-pop)
- `MPTDB_PROVIDER_BENCH_SC_STORAGE_TRIE_CACHE_CAPACITY` (SC L2 storage-trie cache capacity)
- `MPTDB_PROVIDER_BENCH_SC_PERSISTED_NODE_CACHE_CAPACITY` (SC persisted-node cache capacity)
- `MPTDB_PROVIDER_BENCH_SC_CROSS_BLOCK_SPARSE_MAX_LAG` (cross-block sparse trie eviction lag)

Interpretation caveat:

- If `USE_PROVIDER_READS` is off, mptdb lane read overhead is understated versus production override path.

## 5.1 Workload significance (integration focus)

For integration performance decisions, use the following priority:

- Decision workload: `erc20_transfer_10pct_contract_pool`
- Diagnostic only (single-contract hotspot): `erc20_transfer`
- Legacy baseline only (not used for decision): `eth_transfer`

Reason:

- `erc20_transfer_10pct_contract_pool` is the closest existing workload to mainnet-style ERC20 behavior:
  - multiple ERC20 contracts
  - prefilled contract storage (non-empty balance mapping)
  - transactions distributed across a contract pool
- `eth_transfer` under-represents complex contract-state behavior and should not drive integration decisions.
- A benchmark with ERC20 calls but no prefilled ERC20 state is not meaningful for our goal and should not be used as a decision benchmark.

## 5.2 Test evidence (code-level)

Contract and state setup proof points in `benches/block_execution.rs`:

- ERC20 runtime bytecode is embedded as a fixed contract:
  - `ERC20_RUNTIME_BYTECODE` (minimal ERC20 transfer + balanceOf)
- Prefilled ERC20 state exists before benchmark execution:
  - `setup_mixed_state()` pre-populates per-contract ERC20-like balance slots
  - `setup_erc20()` explicitly writes holder balances into ERC20 mapping slots
- 10% contract-pool workload uses prefilled holders and distributes writes:
  - `select_active_erc20_contracts()` picks active pool
  - `generate_erc20_block_txs_contract_pool()` chooses sender from prefilled holder set per contract

This is why `erc20_transfer_10pct_contract_pool` is the integration workload to prioritize.

## 5.3 Recommended runs

Use this benchmark group as the standard integration report set:

1. `erc20_transfer_10pct_contract_pool` (decision workload)

Example commands:

```bash
# Primary: ERC20 pool workload
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=0 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool"
```

Primary workload complexity (current default for `erc20_transfer_10pct_contract_pool`):

- `pre_pop_accounts >= 500_000`
- `txs_per_block >= 50_000`
- `contract_kv_per_contract >= 128`

Optional overrides for this workload only:

- `MPTDB_PROVIDER_BENCH_POOL_PREPOP_ACCOUNTS`
- `MPTDB_PROVIDER_BENCH_POOL_TXS_PER_BLOCK`
- `MPTDB_PROVIDER_BENCH_POOL_CONTRACT_KV_PER_CONTRACT`
- `MPTDB_PROVIDER_BENCH_POOL_CONTRACT_RATIO`
- `MPTDB_PROVIDER_BENCH_POOL_NUM_BLOCKS`

Then run A/B toggles on top of the same workload:

- `MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=0|1`
- `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=0|1`

## 6. API boundaries and current limitations

In `MptDbStateProvider` today:

- Supported: `state_root`, `state_root_with_updates` (returns empty `TrieUpdates`), `proof`, `storage_proof`, `storage_root`, `storage_multiproof`
- Explicitly unsupported:
  - `state_root_from_nodes`
  - `state_root_from_nodes_with_updates`
  - `multiproof` (hashed-address target cannot be reversed to raw address)
  - `witness`

In `MptDbStateWriter`:

- `take_state_above` is unsupported (SC does not store per-block execution outcomes)
- `remove_state_above` is supported via SC rollback

## 7. Test map

- Acceptance tests: `src/tests.rs`
  - validates Plan C separation: fallback serves reads, SC serves root/proof, writer updates SC
- Integration benchmark: `benches/block_execution.rs`

Related but different (SC micro profile, not full provider integration):

- `crates/xlayer/mpt-db/mptdb/tests/profile_mptdb_vs_reth.rs`
- `crates/xlayer/mpt-db/mptdb/tests/benchmark_mptdb_vs_reth.rs`

### 7.1 B4.8 alignment snapshot (April 3, 2026)

Purpose:

- keep a provider-aligned SC micro-profile reference for `erc20_transfer_10pct_contract_pool`.

Workload alignment (B4.8 in `mptdb/tests/profile_mptdb_vs_reth.rs`):

- `prepop_accounts = 500000`
- `updates_per_block = 50000`
- `block_count = 10`
- `contract_ratio = 30%`
- `contract_kv_per_contract = 128`
- `active_contract_pool_ratio = 10%`

Commands:

```bash
# mptdb (SC-only profile)
PROTOC=/Users/louisliuxiong/golang/bin/protoc \
cargo test -p mptdb --release --test profile_mptdb_vs_reth \
  profile_b4_8_integration_scale_mpt_only -- --ignored --nocapture --exact

# reth baseline (same B4.8 dataset generator)
PROTOC=/Users/louisliuxiong/golang/bin/protoc \
cargo test -p mptdb --release --test profile_mptdb_vs_reth \
  profile_b4_8_integration_scale_reth_only -- --ignored --nocapture --exact
```

Latest observed result (April 3, 2026):

| lane | per-block |
|---|---:|
| `mptdb` | `729.9 ms` |
| `reth` | `12172.2 ms` |

SC micro-profile speedup: `~16.7x` (`reth / mptdb`).

Important scope note:

- This is SC-only (`apply + commit/root`) and excludes provider-layer EVM/read wrapper costs.
- Use `benches/block_execution.rs` integration benchmark as the final decision metric.

## 8. Practical checklist for future AI changes

Before claiming "mptdb vs reth" integration result:

1. Confirm whether `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1` is enabled.
2. State `MPTDB_PROVIDER_BENCH_MEASURE` mode explicitly.
3. Separate setup/pre-pop time from per-block lifecycle time.
4. Mention whether SC prewarm is on/off.
5. Do not compare SC-only micro-profile numbers against provider integration numbers directly.

## 9. Latest integration results (April 2, 2026)

Measurement settings used in this run:

- Benchmark: `cargo bench --bench block_execution -p mptdb-provider`
- Mode: `MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle`
- Timing window: `MPTDB_PROVIDER_BENCH_WARMUP_SECS=1`, `MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=15`
- Dataset: historical settings before the pool complexity uplift
  (`100000acc_20000tx_10blk_30c_32kv`)

All numbers below are Criterion median converted to `ms/block` (reported `time` is per 10 blocks).

### 9.1 Primary workload: `erc20_transfer_10pct_contract_pool`

| `USE_PROVIDER_READS` | `ENABLE_SC_PREWARM` | mptdb ms/block | reth_mdbx ms/block | mptdb/reth |
|---|---|---:|---:|---:|
| 0 | 0 | 256.36 | 257.24 | 1.00x |
| 0 | 1 | 250.59 | 251.34 | 1.00x |
| 1 | 0 | 248.62 | 248.97 | 1.00x |
| 1 | 1 | 253.90 | 248.97 | 1.02x |

Observed behavior:

- In this workload, mptdb and reth are effectively parity.
- With `USE_PROVIDER_READS=0`, enabling prewarm gave a small improvement for mptdb.
- With `USE_PROVIDER_READS=1`, enabling prewarm regressed slightly in this run.

Note:

- These numbers are historical baselines.
- Current default complexity for `erc20_transfer_10pct_contract_pool` is higher
  (500k/50k/128 floor), so results are not directly comparable.

### 9.2 Legacy baseline (not decision workload): `eth_transfer`

| `USE_PROVIDER_READS` | `ENABLE_SC_PREWARM` | mptdb ms/block | reth_mdbx ms/block | mptdb/reth |
|---|---|---:|---:|---:|
| 0 | 0 | 159.67 | 119.21 | 1.34x |
| 0 | 1 | 159.73 | 122.06 | 1.31x |

Observed behavior:

- In pure ETH transfer baseline, mptdb is slower than reth in current implementation.
- Prewarm had no meaningful gain in this baseline run.
- Keep this result as reference only; prioritize `erc20_transfer_10pct_contract_pool` for integration decisions.

### 9.3 Current separated baseline run (April 2, 2026, heavy pool dataset)

Run policy:

- `reth+mdbx` and `mpt-db` were run separately
- `pkill -f "cargo test|cargo bench|block_execution|mptdb"` before each run
- same benchmark target: `erc20_transfer_10pct_contract_pool`
- this historical `mptdb` run enabled `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1`
  (wrapper-read overhead included; not the default integration mode)
- heavy dataset id:
  `500000acc_50000tx_10blk_30c_128kv_pool15000`

Commands:

```bash
# baseline: reth + mdbx
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=0 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/reth_mdbx"

# mpt-db lane
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/mptdb"
```

Criterion results:

| lane | criterion time (10 blocks) | median ms/block | relative |
|---|---:|---:|---:|
| `reth_mdbx` | `[12.025s, 12.217s, 12.446s]` | `1221.7` | baseline |
| `mptdb` | `[14.346s, 14.749s, 15.251s]` | `1474.9` | `1.21x` vs reth |

Observed:

- Under this heavy ERC20 pool workload, current `mptdb` is about `+20.7%` slower than `reth_mdbx`.
- `mptdb` run had outliers (`2/10`, one mild + one severe), indicating non-trivial runtime jitter.

### 9.4 External-root reuse experiment (retired)

Code path (historical experiment):

- mptdb lane now uses MDBX worker to compute
  `overlay_root_with_updates + write_trie_updates`
- SC main thread runs `apply_bundle_state`, then `commit_with_external_root`
  (skip duplicate account-root hash in SC wal_first path)

Command:

```bash
MPTDB_PROVIDER_BENCH_POOL_PREPOP_ACCOUNTS=120000 \
MPTDB_PROVIDER_BENCH_POOL_TXS_PER_BLOCK=20000 \
MPTDB_PROVIDER_BENCH_POOL_NUM_BLOCKS=10 \
MPTDB_PROVIDER_BENCH_POOL_CONTRACT_KV_PER_CONTRACT=128 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_SC_PROFILE=1 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=15 \
MPTDB_PROVIDER_BENCH_SAMPLE_SIZE=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool"
```

Criterion results (10 blocks):

| lane | criterion time (10 blocks) | median ms/block |
|---|---:|---:|
| `mptdb` | `[5.223s, 5.252s, 5.281s]` | `525.2` |
| `reth_mdbx` | `[3.571s, 3.599s, 3.629s]` | `359.9` |

SC profile highlights (`mptdb`, averaged per block):

- `account_root`: ~`95-100ms`
- `total_commit`: ~`149-155ms`

Result/decision:

- This experiment is not the target integration architecture.
- Target remains: SC-only root/MPT path; no `overlay_root_with_updates + write_trie_updates` in
  mptdb hot path.

### 9.5 EVM vs Commit+Root split (April 2, 2026, medium pool dataset)

Goal:

- clearly separate `EVM execution` from `commit + root calculation` cost.

Definition used in benchmark logs:

- `block_lifecycle = evm + non_evm`
- `evm`: `execute_block_evm(...)`
- `non_evm`:
  - mptdb lane: `write_wall` (SC `write_state` = apply + commit/root, overlapping with MDBX `write_state+commit`)
  - reth lane: `write_state + overlay_root_with_updates + write_trie_updates + commit`

Commands (same dataset / same measurement window):

```bash
MPTDB_PROVIDER_BENCH_POOL_PREPOP_ACCOUNTS=120000 \
MPTDB_PROVIDER_BENCH_POOL_TXS_PER_BLOCK=20000 \
MPTDB_PROVIDER_BENCH_POOL_NUM_BLOCKS=10 \
MPTDB_PROVIDER_BENCH_POOL_CONTRACT_KV_PER_CONTRACT=128 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=10 \
MPTDB_PROVIDER_BENCH_SAMPLE_SIZE=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/mptdb"

MPTDB_PROVIDER_BENCH_POOL_PREPOP_ACCOUNTS=120000 \
MPTDB_PROVIDER_BENCH_POOL_TXS_PER_BLOCK=20000 \
MPTDB_PROVIDER_BENCH_POOL_NUM_BLOCKS=10 \
MPTDB_PROVIDER_BENCH_POOL_CONTRACT_KV_PER_CONTRACT=128 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=10 \
MPTDB_PROVIDER_BENCH_SAMPLE_SIZE=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/reth_mdbx"
```

Criterion results (10 blocks):

| lane | criterion time (10 blocks) | median ms/block |
|---|---:|---:|
| `mptdb` | `[5.180s, 5.203s, 5.227s]` | `520.3` |
| `reth_mdbx` | `[3.487s, 3.521s, 3.571s]` | `352.1` |

Pipeline split (avg/blk, observed range from this run):

| lane | EVM execution | commit+root related (non-EVM) | block lifecycle |
|---|---:|---:|---:|
| `mptdb` | `~115-117ms` | `~396-407ms` (`write_wall`) | `~512-525ms` |
| `reth_mdbx` | `~113-117ms` | `~231-238ms` (`write_state + overlay+trie_updates + commit`) | `~347-353ms` |

Non-EVM detailed split:

- `mptdb` (typical avg/blk):
  - `sc_write` (apply + commit/root): `~396-407ms`
  - worker internals: `write_state ~68-70ms`, `commit ~56-58ms`, `total ~244-248ms`
- `reth_mdbx` (typical avg/blk):
  - `write_state`: `~61-63ms`
  - `overlay+trie_updates`: `~114-116ms`
  - `commit`: `~55-58ms`

Conclusion from this split:

- `EVM execution` is already close between lanes.
- Main gap is in `non-EVM` path, specifically SC `write_state` (commit/root) cost.

### 9.6 Latest Heavy-Dataset Trace + SC Profile (April 2, 2026)

Dataset / run mode:

- workload: `erc20_transfer_10pct_contract_pool`
- dataset id: `500000acc_50000tx_10blk_30c_128kv_pool15000`
- measure mode: `block_lifecycle`
- `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=0`
- `MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1`
- `MPTDB_PROVIDER_BENCH_TRACE=1`
- mptdb lane additionally enabled `MPTDB_PROVIDER_BENCH_SC_PROFILE=1`

Commands used:

```bash
# mptdb (with sc_profile)
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=0 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_SC_PROFILE=1 \
MPTDB_PROVIDER_BENCH_TRACE=1 \
MPTDB_PROVIDER_BENCH_TRACE_ITERS=10 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/mptdb"

# reth_mdbx baseline
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=0 \
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 \
MPTDB_PROVIDER_BENCH_TRACE=1 \
MPTDB_PROVIDER_BENCH_TRACE_ITERS=10 \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=10 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/reth_mdbx"
```

Observed per-block averages from trace output:

| lane | block_lifecycle | EVM | non-EVM hot path |
|---|---:|---:|---:|
| `mptdb` | `~1.45s` | `~406.96ms` | `sc_write/write_wall ~1.04s` |
| `reth_mdbx` | `~1.18s` | `~351.84ms` | `write_state + overlay+trie_updates + commit ~826.88ms` |

mptdb SC profile (avg/blk):

- `apply_bundle_state`: `~510.62ms`
- `storage_roots`: `~49.99ms`
- `account_updates`: `~106.71ms`
- `account_root`: `~307.30ms`
- `wal`: `~8.25ms`
- `total_commit`: `~528.35ms`

Key conclusion:

- Current gap is still in SC hot path (`apply + commit/root`), not WAL/persist.
- Under this heavy pool workload, `mptdb` is slower mainly because:
  - `apply_bundle_state` is large (`~510ms/blk`)
  - `account_root` inside commit is still large (`~307ms/blk`)

### 9.7 Deep Diagnosis: Why B4.8 Is Fast but Provider Integration Is Slow (April 3, 2026)

Problem statement:

- B4.8 SC micro-profile (`mptdb/tests/profile_mptdb_vs_reth.rs`) is much faster.
- Provider integration benchmark (`mptdb-provider/benches/block_execution.rs`) on the same workload shape is much slower.
- Main user concern: EVM phase is close; gap is in `commit/root` stage after EVM.

Short answer:

- In provider integration, the dominant gap is in SC `apply` sub-phases (`sparse_factory_build` + `sparse_apply_changes`), not in the final account-root hash itself.
- Root cause is low segment hit rate in cross-block sparse reuse, which forces expensive tier1/2 fallback construction.
- B4.8 keeps segment readiness stable (sync prewarm + flush), so it avoids that fallback path almost entirely.

#### 9.7.1 A/B evidence (heavy pool workload)

Workload used for all runs below:

- `prepop_accounts=500000`
- `txs_per_block=50000`
- `num_blocks=10`
- `contract_ratio=0.30`
- `contract_kv_per_contract=128`
- benchmark target: `erc20_transfer_10pct_contract_pool/mptdb`

Baseline provider run (`MPTDB_PROVIDER_BENCH_SC_PROFILE=1`, `mdbx_write_mode=Full`):

- `EVM ~403ms/blk`
- `SC write ~3.24s/blk`
- `apply ~2.64s/blk`
- `total_commit ~599ms/blk`
- `storage_accounts ~14463/blk`
- `storage_slots ~99361/blk`
- sparse factory stats:
  - `seg hit/lookups = 1911/14463`
  - `miss = 12552`
  - `tier12 = 12551`
  - `cross_reuse = 12552`
  - `cross_missing_slots = 43382`

B4.8 SC-only profile (same dataset shape):

- `apply ~227ms/blk`
- sparse stats per block:
  - `sseg = 14463/14463`
  - `smiss = 0`
  - `t12 = 0`
  - `creuse = 0`

Interpretation:

- Provider path has very low segment hit rate (`~13%`) and heavy fallback.
- B4.8 has effectively full segment hit rate (`100%`) and near-zero sparse fallback.

#### 9.7.2 Prewarm behavior diagnosis

Async prewarm enabled in provider (`MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1`):

- Segment hit/miss metrics were essentially unchanged from baseline.
- `apply` remained around the same level.

Sync prewarm+flush diagnostic mode (bench-only):

- Enabled `MPTDB_PROVIDER_BENCH_SYNC_PREWARM_AFTER_BLOCK=1`.
- Segment stats became:
  - `seg hit/lookups = 14463/14463`
  - `miss = 0`
  - `tier12 = 0`
- `apply` dropped significantly (to around `~1.81s/blk` in that run).

Interpretation:

- The async prewarm worker is best-effort and does not guarantee next-block segment readiness.
- If segment publish/readiness is forced before next block, sparse fallback collapses and `apply` improves.

Relevant code behavior:

- `ScPrewarmDispatcher` uses `try_lock`, can skip items if SC is busy:
  - `mptdb-provider/src/provider.rs`
- `maybe_refresh_published_view_for_prewarm()` intentionally does not reload segment view:
  - `mptdb-sc/src/mpt/commit_store.rs`
- `prewarm_storage_trie_by_hashed_address()` returns early when published index has no trie yet (wal_first timing window):
  - `mptdb-sc/src/mpt/commit_store.rs`

#### 9.7.3 MDBX parallel write contention check

Additional diagnostics were run to test whether MDBX write overlap is the primary cause:

- `MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full|plain|noop`
- `MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=0|1`

Observed:

- Even with `mdbx_write_mode=noop`, provider `apply` remained high (`~2.6s/blk` class in parallel mode).
- This indicates the first-order bottleneck is not “MDBX write work itself”, but SC sparse apply/factory behavior under integration conditions.

#### 9.7.4 Cross-block sparse toggle check

Run with `MPT_CROSS_BLOCK_SPARSE=0` in provider benchmark:

- Performance regressed severely (`apply` became much larger).
- Conclusion: cross-block sparse reuse is still net-positive; disabling it is not a fix.
- Real issue is reuse mode encountering low segment readiness and paying repeated fallback cost.

#### 9.7.5 Reproduction commands (diagnostic set)

```bash
# Baseline provider diagnostic (full write mode + SC profile)
MPTDB_PROVIDER_BENCH_PREPOP_ACCOUNTS=500000 \
MPTDB_PROVIDER_BENCH_TXS_PER_BLOCK=50000 \
MPTDB_PROVIDER_BENCH_NUM_BLOCKS=10 \
MPTDB_PROVIDER_BENCH_CONTRACT_RATIO=0.30 \
MPTDB_PROVIDER_BENCH_CONTRACT_KV_PER_CONTRACT=128 \
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=6 \
MPTDB_PROVIDER_BENCH_TRACE=1 \
MPTDB_PROVIDER_BENCH_TRACE_ITERS=1 \
MPTDB_PROVIDER_BENCH_SC_PROFILE=1 \
MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/mptdb"

# Async prewarm toggle (expected: little/no change in seg hit under heavy pressure)
MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1 ...

# Sync prewarm+flush diagnostic (bench-only, not production path)
MPTDB_PROVIDER_BENCH_SYNC_PREWARM_AFTER_BLOCK=1 ...

# MDBX write-path isolation
MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=noop ...

# Cross-block sparse off (expected: much slower; diagnostic only)
MPT_CROSS_BLOCK_SPARSE=0 ...
```

#### 9.7.6 Main diagnostic conclusion

- B4.8 “fast” and provider “slow” are both real and consistent once sparse-factory counters are compared.
- The decisive difference is not EVM and not root-hash algorithm mismatch.
- The decisive difference is integration-time sparse segment readiness:
  - B4.8 keeps it stable (`seg ~100%`).
  - Provider heavy run often misses (`seg ~13%`) and falls back to expensive tier1/2 path.

### 9.8 P0 Fix Landed: Path-Limited Fallback Proof Extraction (April 3, 2026)

Scope of code change:

- `mptdb-sc/src/mpt/sparse_storage.rs`
  - Added path-limited storage proof extraction from `SparseStateTrie`:
    - `extract_storage_proof_from_sparse_trie_for_paths`
    - `sparse_nodes_to_decoded_storage_multiproof_for_paths`
  - Added path-limited storage proof conversion from arena:
    - `convert_arena_to_decoded_storage_multiproof_for_paths`
- `mptdb-sc/src/mpt/commit_store.rs`
  - In `try_build_l2_proof`, tier1/tier2/tier3 now prefer dirty-key path-limited proof build.
  - This removes repeated full-trie/full-arena DFS in fallback-heavy provider runs.

#### 9.8.1 Verification results

Heavy provider workload (same as 9.7):

- `prepop_accounts=500000`
- `txs_per_block=50000`
- `num_blocks=10`
- `contract_ratio=0.30`
- `contract_kv_per_contract=128`
- bench target: `erc20_transfer_10pct_contract_pool/mptdb`

After patch (`MPTDB_PROVIDER_BENCH_SC_PROFILE=1`, `trace_iters=1`, first trace sample):

- `EVM ~3.80s/iter` (similar)
- `SC write ~12.00s/iter` (previously ~32.43s/iter class)
- `apply ~7.34s/iter` (previously ~26.4s/iter class)
- `total_commit ~4.64s/iter` (same order as before; not the main reduction source)
- `sparse_factory_build ~1.33s/iter` (down materially from previous multi-second class)

Per-block view from same trace:

- `sc_write ~1.20s/blk`
- `apply ~734ms/blk`
- `total_commit ~464ms/blk`

Interpretation:

- P0 fix removed the biggest pathological overhead in fallback proof construction.
- Remaining gap vs `reth_mdbx` is now much smaller, but still present.

#### 9.8.2 MDBX contention re-check after fix

With `MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=0` and `MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=noop`:

- `sc_write` stayed around `~11.8-12.1s/iter`.
- Therefore residual bottleneck remains inside SC apply/commit path, not MDBX write overlap.

#### 9.8.3 Current status after P0

- The previous “integration performance collapse” (mptdb several times slower) is largely mitigated.
- The dominant remaining cost under heavy provider integration is still SC-side (`apply + commit`),
  with large `cross_missing_slots` and frequent fallback attempts.

### 9.9 Cache tuning + MDBX alignment note (April 3, 2026)

User request:

- Increase cache sizes and align with `reth + mdbx` startup settings where possible.

#### 9.9.1 MDBX “cache size” reality in reth

There is no explicit MDBX block-cache-size knob in reth startup analogous to RocksDB block cache.
reth MDBX path is mmap-based and relies on OS page cache.

Current reth MDBX startup defaults (from `reth_db::mdbx::DatabaseArguments` and env open path):

- map size upper bound: `8 TB`
- growth step: `4 GB`
- page size: default page size (`default_page_size()`, usually `4 KB`)
- `max_readers`: `32000`
- `no_rdahead = true`
- `rp_augment_limit = 256 * 1024`

So for this benchmark, “align cache size with reth+mdbx” means:

- MDBX side already uses the same reth defaults.
- Tunable cache levers are mainly on SC (`MptConfig`) side.

#### 9.9.2 SC cache knobs added to benchmark

`benches/block_execution.rs` now supports:

- `MPTDB_PROVIDER_BENCH_SC_STORAGE_TRIE_CACHE_CAPACITY`
- `MPTDB_PROVIDER_BENCH_SC_PERSISTED_NODE_CACHE_CAPACITY`
- `MPTDB_PROVIDER_BENCH_SC_CROSS_BLOCK_SPARSE_MAX_LAG`

Benchmark defaults are increased to:

- `sc_storage_cache = 200000` (was effectively `50000`)
- `sc_persisted_cache = 2000000` (was `500000`)
- `sc_cross_lag = 64` (was `8`)

#### 9.9.3 A/B result on integration workload

Workload: `erc20_transfer_10pct_contract_pool` with
`500000acc_50000tx_10blk_30c_128kv_pool15000`.

Old cache profile (`50k / 500k / 8`) on same code:

- `sc_write ~12.3-12.5s/iter`
- `apply ~741-753ms/blk`

New default cache profile (`200k / 2M / 64`):

- `sc_write ~10.3s/iter`
- `apply ~532-537ms/blk`

Effect:

- `sc_write` reduced by roughly `~17%`
- `apply` reduced by roughly `~28-30%`
- `sparse_factory` miss/t12 counters were largely unchanged, so the gain is mainly from cache-hit/memory locality improvements in SC internals.
