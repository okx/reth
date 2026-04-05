# mptdb-provider Design Notes (Plan C)

This directory contains the reth integration adapter for mpt-db SC (state-commit engine).

Goal: keep reth integration stable while moving root/proof to `mptdb-sc`, and progressively moving account/storage hot reads+writes to `mptdb-ss`.

## 1. Architecture in one page

Plan C split:

- EVM reads (`basic_account`, `storage`, `bytecode`, `block_hash`):
  - default: from reth fallback provider (MDBX PlainState path)
  - optional primary-state mode: account/storage from `mptdb-ss`
- State root + account/storage proof:
  - from `MptCommitStore` (SC)
- State writes:
  - `MptDbStateWriter` writes SC
  - optional mirror: `MptDbStateWriter` writes SS changeset per committed version
  - reth native writer still writes MDBX PlainState tables in its own pipeline

Code entry points:

- `src/provider.rs`: `MptDbStateProvider`
- `src/factory.rs`: `MptDbStateProviderFactory`
- `src/writer.rs`: `MptDbStateWriter`
- `benches/block_execution.rs`: integration-style block lifecycle benchmark

## 2. Read/Write ownership (important)

Read ownership:

- `basic_account` / `storage`:
  - default: delegate to `fallback` provider
  - primary-state mode: read SS first, fallback only when SS version is unavailable
- `state_root` delegates to SC
- `proof` / `storage_proof` / `storage_root` delegate to SC

Write ownership:

- `MptDbStateWriter::write_state` -> `sc.apply_bundle_state()` + `sc.commit()`
- optional primary-state mirror: write SS `bundle_to_ss_changeset` at committed SC version
- `remove_state_above` in primary-state mode also aligns SS `latest_version` to rollback target
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
  - primary-state mode: `MPTDB_PROVIDER_BENCH_PRIMARY_STATE=1`
    - forces provider-read path
    - `basic_account/storage` prefer SS
    - SC commit mirrors SS changeset write
  - per block: EVM -> parallel { SC `write_state` (apply+commit/root), MDBX `write_state+commit` } -> wait MDBX done
- `reth_mdbx` lane:
  - EVM reads from MDBX
  - serial `write_state + overlay_root_with_updates + write_trie_updates + commit`

Key env knobs:

- `MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle|end_to_end`
- `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1` (measure wrapper/lock overhead)
- `MPTDB_PROVIDER_BENCH_PRIMARY_STATE=1` (enable SS primary-state read/write path in mptdb lane)
- `MPTDB_PROVIDER_BENCH_PRIMARY_STATE_READS=0|1` (only when primary-state is on; `0` keeps SS mirror writes but routes EVM reads to fallback for diagnosis)
- `MPTDB_PROVIDER_BENCH_ENABLE_SC_PREWARM=1`
- `MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=0|1` (diagnostic A/B: serial vs parallel MDBX writer)
- `MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full|plain|noop` (diagnostic isolation of MDBX write path)
- `MPTDB_PROVIDER_BENCH_SYNC_PREWARM_AFTER_BLOCK=1` (diagnostic only: force per-block sync prewarm+flush)
- `MPTDB_PROVIDER_BENCH_FLUSH_AFTER_PREPOP=0|1` (controls flush after genesis pre-pop)
- `MPTDB_PROVIDER_BENCH_SC_STORAGE_TRIE_CACHE_CAPACITY` (SC L2 storage-trie cache capacity)
- `MPTDB_PROVIDER_BENCH_SC_PERSISTED_NODE_CACHE_CAPACITY` (SC persisted-node cache capacity)
- `MPTDB_PROVIDER_BENCH_SC_CROSS_BLOCK_SPARSE_MAX_LAG` (cross-block sparse trie eviction lag)
- `MPT_VERIFY_WAL_SPARSE_ACCOUNT_ROOT=1` (diagnostic parity check: compute both sparse-root and account-trie root)

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
- `MPTDB_PROVIDER_BENCH_SYNC_PREWARM_AFTER_BLOCK=0|1`

Fixed integration-mode constraints (April 4, 2026 update):

- `mptdb` lane EVM reads are fixed to MDBX plain-state reads.
- `mptdb` lane MDBX writes are fixed to plain-state tables only
  (`PlainAccountState` + `PlainStorageState`).
- `MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS` and `MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE`
  are no longer benchmark switches.

## 5.4 B5.0 Peak Integration Stress (provider benchmark only)

To stress integration-path throughput at a larger scale, `block_execution.rs` now includes:

- benchmark group: `b5_0_peak_integration_10x`
- lanes: `mptdb` and `reth_mdbx` (same provider integration lifecycle path)

Run:

```bash
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
cargo bench --bench block_execution -p mptdb-provider -- "b5_0_peak_integration_10x"
```

B5.0-specific knobs:

- `MPTDB_PROVIDER_BENCH_B5_0_PREPOP_ACCOUNTS` (default: `1_000_000`)
- `MPTDB_PROVIDER_BENCH_B5_0_NUM_BLOCKS` (default: `100`)
- `MPTDB_PROVIDER_BENCH_B5_0_TXS_PER_BLOCK` (default: `20_000`)
- `MPTDB_PROVIDER_BENCH_B5_0_CONTRACT_RATIO` (default: `0.30`)
- `MPTDB_PROVIDER_BENCH_B5_0_CONTRACT_KV_PER_CONTRACT` (default: `64`)
- `MPTDB_PROVIDER_BENCH_B5_0_ACTIVE_CONTRACT_POOL_RATIO` (default: `0.10`)

Notes:

- This is an integration benchmark in `mptdb-provider`; it is not an SC-only micro-profile.
- For quick smoke checks, reduce the B5.0 knobs and keep the same benchmark group name/filter.

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
  - validates default Plan C separation: fallback serves reads, SC serves root/proof, writer updates SC
  - validates primary-state mode:
    - SS account/storage reads when `prefer_ss_reads = true`
    - fallback-on-unavailable-version behavior
    - rollback updates SS `latest_version`
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

## 9.3 Primary-state quick A/B (April 4, 2026)

Scope:

- Workload: `erc20_transfer_10pct_contract_pool`
- Dataset: current bench defaults (`100000acc_20000tx_10blk_30c_32kv_pool3000`)
- Mode: `MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle`
- This is a short trace run (`WARMUP_SECS=1`, `MEASUREMENT_SECS=2`) and uses repeated
  `avg/blk(block-lifecycle)` log lines (not final long Criterion convergence).

Commands:

```bash
# mptdb baseline (provider reads on, primary-state off)
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=2 \
MPTDB_PROVIDER_BENCH_TRACE=1 \
MPTDB_PROVIDER_BENCH_TRACE_ITERS=1 \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1 \
MPTDB_PROVIDER_BENCH_PRIMARY_STATE=0 \
MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/mptdb"

# mptdb primary-state on
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=2 \
MPTDB_PROVIDER_BENCH_TRACE=1 \
MPTDB_PROVIDER_BENCH_TRACE_ITERS=1 \
MPTDB_PROVIDER_BENCH_USE_PROVIDER_READS=1 \
MPTDB_PROVIDER_BENCH_PRIMARY_STATE=1 \
MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/mptdb"

# reth baseline
MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle \
MPTDB_PROVIDER_BENCH_WARMUP_SECS=1 \
MPTDB_PROVIDER_BENCH_MEASUREMENT_SECS=2 \
MPTDB_PROVIDER_BENCH_TRACE=1 \
MPTDB_PROVIDER_BENCH_TRACE_ITERS=1 \
cargo bench --bench block_execution -p mptdb-provider -- "erc20_transfer_10pct_contract_pool/reth_mdbx"
```

Observed summary (from trace averages):

| lane | avg/blk | evm | write/commit wall |
|---|---:|---:|---:|
| `mptdb` (`provider_reads=1`, `primary_state=0`) | `~192 ms` | `~113 ms` | `~78 ms` |
| `mptdb` (`provider_reads=1`, `primary_state=1`) | `~372 ms` | `~266 ms` | `~106 ms` |
| `mptdb` (`provider_reads=1`, `primary_state=1`, `primary_state_reads=0`) | `~215 ms` | `~108 ms` | `~107 ms` |
| `reth_mdbx` | `~245 ms` | `~106 ms` | `~138 ms` (`write + root+commit`) |

Quick conclusion:

- With current implementation, enabling `primary_state` with SS reads on makes the
  mptdb lane significantly slower than both `mptdb primary_state=0` and `reth_mdbx`.
- When `primary_state=1` but `primary_state_reads=0` (SS writes on, SS reads off),
  performance returns close to baseline. This isolates the dominant regression to
  the SS read path (not SC+SS write path).
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

At that time, benchmark defaults were increased to:

- `sc_storage_cache = 200000` (was effectively `50000`)
- `sc_persisted_cache = 2000000` (was `500000`)
- `sc_cross_lag = 64` (was `8`)

#### 9.9.3 A/B result on integration workload

Workload: `erc20_transfer_10pct_contract_pool` with
`500000acc_50000tx_10blk_30c_128kv_pool15000`.

Old cache profile (`50k / 500k / 8`) on same code:

- `sc_write ~12.3-12.5s/iter`
- `apply ~741-753ms/blk`

Then-default cache profile (`200k / 2M / 64`):

- `sc_write ~10.3s/iter`
- `apply ~532-537ms/blk`

Effect:

- `sc_write` reduced by roughly `~17%`
- `apply` reduced by roughly `~28-30%`
- `sparse_factory` miss/t12 counters were largely unchanged, so the gain is mainly from cache-hit/memory locality improvements in SC internals.

Current default profile (April 4, 2026) is reverted to:

- `sc_storage_cache = 50000`
- `sc_persisted_cache = 500000`
- `sc_cross_lag = 8`

### 9.10 P1 Fix Landed: Cross-Missing Proof Gating (April 3, 2026)

Problem after P0 + cache tuning:

- Provider heavy run still had large `cross_missing_slots` in cross-block reuse mode.
- Even when those missing slots could be inserted on already-revealed in-memory paths,
  SC still spent time preparing fallback reveal/proof path.

Scope of code change:

- `mptdb-sc/src/mpt/sparse_storage.rs`
  - Added `storage_key_requires_provider_reveal(storage_trie, slot_key)`:
    - Returns `true` only when path analysis indicates Hash-blinded/unknown nodes may require provider reveal.
    - Returns `false` for branch-miss / fully-revealed in-memory paths.
- `mptdb-sc/src/mpt/commit_store.rs`
  - In cross-block reuse path, for `cross_missing_slots`:
    - Build fallback proof only for `proof_keys` that actually require provider reveal.
    - If `proof_keys` is empty, inject `DecodedStorageMultiProof::empty()` as explicit prebuilt proof
      (to satisfy sparse-apply reveal planning for existing accounts without forcing fallback build).
  - Added profile counter:
    - `sparse_factory_cross_missing_proof_slots`
- `mptdb-provider/benches/block_execution.rs`
  - Exposed and printed `cross_missing_proof_slots` in sparse-factory trace and avg lines.

#### 9.10.1 Verification run (same provider workload)

Workload:

- `erc20_transfer_10pct_contract_pool/mptdb`
- `500000acc_50000tx_10blk_30c_128kv_pool15000`
- `MPTDB_PROVIDER_BENCH_SC_PROFILE=1`
- `MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle`
- `MPTDB_PROVIDER_BENCH_TRACE=1`
- `MPTDB_PROVIDER_BENCH_TRACE_ITERS=1`

First trace sample after P1:

- `EVM ~4.02s/iter`
- `SC write ~7.78s/iter`
- `apply ~2.98s/iter` (`~298ms/blk`)
- `total_commit ~4.78s/iter` (`~478ms/blk`)
- `avg/blk(block-lifecycle) ~1.18s`

Sparse factory counters (per-block):

- `cross_reuse ~12552`
- `cross_missing_slots ~43382`
- `cross_missing_proof_slots ~0`
- `t3=0`, `t12=0`

Interpretation:

- Under this integration-scale workload, cross-missing slots are predominantly
  branch-miss / already-revealed-path inserts, so provider-backed proof build was unnecessary.
- P1 removes this residual sparse-factory overhead and further reduces `apply + sc_write`.

#### 9.10.2 Reference comparison vs reth_mdbx (same workload, first trace sample)

- `reth_mdbx avg/blk(block-lifecycle) ~1.20s`
- `mptdb(P1) avg/blk(block-lifecycle) ~1.18s`

This is first-trace diagnostic comparison (not full criterion convergence), but confirms
P1 eliminated the previous integration slow-path on this workload.

#### 9.10.3 Correctness guard

- Re-ran `profile_b4_8_integration_scale_mpt_only` (`10 blocks`) after P1: pass.
- This verifies root/state path remains valid under the optimization.

### 9.11 P1.1 Follow-up: No-Reveal Fast Path for Cross-Missing Slots (April 3, 2026)

Observation after 9.10:

- `cross_missing_proof_slots` was already `~0`, meaning most cross-missing slots did not need
  provider reveal/proof nodes.
- But Step-1 in `apply_all_storage_changes_sparse` still paid non-trivial overhead by carrying
  these accounts through storage reveal planning.

Code change:

- `mptdb-sc/src/mpt/sparse_storage.rs`
  - Added `SegmentTrieNodeProviderFactory.no_reveal_accounts`.
  - In Step-1 storage reveal planning, accounts in `no_reveal_accounts` are skipped directly.
- `mptdb-sc/src/mpt/commit_store.rs`
  - In cross-reuse path, when `proof_keys.is_empty()`, mark account in
    `factory.no_reveal_accounts` (instead of forcing an empty prebuilt proof path).
  - Kept `cross_missing_proof_slots` counter for diagnostics.

#### 9.11.1 Verification (parallel mode, integration workload)

Workload: same `erc20_transfer_10pct_contract_pool/mptdb`
(`500000acc_50000tx_10blk_30c_128kv_pool15000`, `block_lifecycle`, `SC_PROFILE=1`).

Before 9.11 (after 9.10):

- `avg/blk ~1.18s`
- `sc_write ~7.73-7.78s/iter`
- `apply ~293-298ms/blk`

After 9.11:

- `avg/blk ~1.11-1.13s`
- `sc_write ~7.16-7.34s/iter`
- `apply ~239-247ms/blk`
- `cross_missing_proof_slots ~0` (unchanged, as expected)

Interpretation:

- Additional gain comes from removing unnecessary Step-1 reveal bookkeeping for accounts whose
  storage updates can proceed entirely on already-revealed in-memory paths.

#### 9.11.2 Current reference vs reth_mdbx

Same workload, first trace diagnostics:

- `mptdb`: `~1.11-1.13s/blk`
- `reth_mdbx`: `~1.21-1.23s/blk`

So mptdb now holds a clearer lead in provider integration mode under this workload.

### 9.12 P1.2 Follow-up: Skip Segment Lookup for No-Reveal Accounts (April 3, 2026)

Observation:

- In cross-reuse path, many accounts already had `proof_keys.is_empty()` and were marked
  `no_reveal_accounts`.
- But segment lookup was still executed before this decision, adding unnecessary Step-1 overhead.

Code change:

- `mptdb-sc/src/mpt/commit_store.rs`
  - Reordered cross-reuse flow:
    - first compute `missing_count` + `proof_keys`
    - if `proof_keys.is_empty()`, mark `no_reveal_accounts` and skip account immediately
    - only then try segment lookup / proof fallback for accounts that actually need reveal
  - Refactored segment lookup into
    `try_segment_lookup_for_sparse_factory(...)` for clearer borrow/lifetime boundaries.

Verification (same provider workload and bench mode):

- target: `erc20_transfer_10pct_contract_pool/mptdb`
- criterion result after this patch:
  - `time: [2.4743 s 2.4966 s 2.5231 s]`
  - `change: [-9.2061% -8.1427% -6.9834%] (p = 0.00)`
  - `Performance has improved.`

Interpretation:

- This is a pure sparse-factory planning reduction; it removes work that was provably unnecessary
  for no-reveal accounts.
- `cross_missing_proof_slots` remains `~0`, and sparse-factory counters stay consistent.

### 9.13 RocksDB Tuning Check Against sei-chain Parameters (April 3, 2026)

We also tested a parameter set aligned with local `sei-chain` RocksDB style:

- enabled in `mptdb-engine/src/engine.rs`
  - `increase_parallelism(available_parallelism)`
  - `optimize_level_style_compaction(512MB)`
  - `set_target_file_size_multiplier(2)`
  - `set_level_compaction_dynamic_level_bytes(true)`
  - `set_compression_options_parallel_threads(4)`
  - bottommost zstd + dictionary train options
  - block table: hybrid ribbon filter + binary-search index + filter-memory optimization
  - plain DB block cache increased to `1GB`

Result on the same provider workload:

- criterion:
  - `time: [2.4862 s 2.5148 s 2.5442 s]`
  - `change: [-0.8212% +0.7317% +2.2738%] (p = 0.38)`
  - `No change in performance detected.`

Conclusion:

- Current bottleneck is still SC-side apply/commit CPU path under wal-first integration, not a
  dominant RocksDB option miss in this benchmark profile.

### 9.14 P1.3 Follow-up: wal_first Default to Sparse-Root (April 3, 2026)

Problem after 9.12/9.13:

- SC commit still paid duplicate root work in wal_first mode:
  - sparse trie path already has enough data to derive state root
  - commit still defaulted to legacy `account_trie.root_hash_only_parallel_account(...)`

Code change:

- `mptdb-sc/src/mpt/commit_store.rs`
  - In wal_first branch, default `use_sparse_root=true`.
  - Kept `MPT_VERIFY_WAL_SPARSE_ACCOUNT_ROOT=1` for strict parity diagnostics.

Verification (provider workload):

- target: `erc20_transfer_10pct_contract_pool/mptdb`
- after P1.3:
  - `time: [2.4620 s 2.4878 s 2.5156 s]`
  - SC profile (avg/blk, representative):
    - `total_commit ~84-86ms`
    - `account_root ~30-33ms` (lower than previous default path)

Control:

- `reth_mdbx` on same run window:
  - `time: [2.5245 s 2.5531 s 2.5846 s]`

Parity diagnostic:

- With `MPT_VERIFY_WAL_SPARSE_ACCOUNT_ROOT=1`, no mismatch error observed.
- Expected overhead is large in this mode (it intentionally computes both roots).

B4.8 regression guard:

- Re-ran `profile_b4_8_integration_scale_mpt_only` (`10 blocks`): pass.

### 9.15 P1.4 Follow-up: Deferred Sparse Snapshot Coalescing (April 3, 2026)

Diagnosis (from new SC trace):

- Added `MPT_ACCOUNT_ROOT_TRACE=1` in `mptdb-sc/src/mpt/commit_store.rs`.
- On `erc20_transfer_10pct_contract_pool/mptdb`, `account_root` hot path split showed:
  - `sparse_root_ms ~5-7ms`
  - `sparse_deferred_snapshot_ms ~9-20ms` (dominant)
- So the residual commit bottleneck was not root hash itself, but per-block cloning of
  ~3k `SerialSparseTrie` snapshots for deferred segment materialization.

Code change:

- `mptdb-sc/src/mpt/commit_store.rs`
  - Introduced coalesced deferred-publish root backlog:
    - `sparse_deferred_publish_roots: HashMap<B256, B256>`
  - In wal_first + deferred-segment mode:
    - stop cloning sparse tries for every touched account every block
    - keep only latest `(hashed_addr -> storage_root)` in backlog
    - materialize snapshots periodically (default interval policy):
      - if pending targets `< 2048`: every block (`interval=1`)
      - if pending targets `>= 2048`: every 4 blocks (`interval=4`)
    - force a drain when pending targets reach `20000` (safety bound)
  - New override knob:
    - `MPT_WAL_SPARSE_MATERIALIZE_INTERVAL=<N>` (`N>=1`)

Provider verification (`erc20_transfer_10pct_contract_pool/mptdb`):

- before P1.4:
  - criterion: `time ~[1.996s, 2.024s, 2.066s]`
  - SC profile (avg/blk):
    - `account_root ~20-21ms`
    - `total_commit ~30-31ms`
- after P1.4:
  - criterion: `time ~[1.880s, 1.895s, 1.911s]`
  - SC profile (avg/blk):
    - `account_root ~9-10ms`
    - `total_commit ~18-19ms`

reth control (same window):

- `erc20_transfer_10pct_contract_pool/reth_mdbx`
  - criterion: `time ~[2.461s, 2.487s, 2.516s]`

Current interpretation:

- P1.4 removes the dominant deferred snapshot clone overhead from SC commit hot path.
- On the decision workload, mptdb block lifecycle is now ~`24%` faster than reth+mdbx in this run
  window (`~1.90s` vs `~2.49s` for 10 blocks).

Regression guard (SC micro-profiles):

- `profile_b4_6_mpt_only`: pass
  - per-block `~243.3ms`, commit `~61.7ms`
- `profile_b4_7_mainnet_realistic_mpt_only`: pass
  - per-block `~38.4ms`, commit `~9.4ms`
- `profile_b4_8_integration_scale_mpt_only`: pass
  - per-block `~233.0ms`, commit `~84.3ms`

### 9.16 Re-confirmed Root Cause of `write_wall` (April 3, 2026)

Goal:

- Explain why `block_lifecycle ~189ms/blk` can still look "too high" even after SC root optimization.
- Pin down whether the remaining non-EVM time is SC root/commit or MDBX write overlap.

Method:

- Same decision workload: `erc20_transfer_10pct_contract_pool/mptdb`
  (`100000acc_20000tx_10blk_30c_32kv_pool3000` in this run window).
- Same measure mode: `MPTDB_PROVIDER_BENCH_MEASURE=block_lifecycle`.
- Added fine-grained trace fields in benchmark:
  - `write_breakdown(prepare, enqueue, mdbx_wait, mdbx_worker, mdbx_serial, residual)`
  - and inside MDBX worker: `mdbx_worker(write, commit)`.
- Ran A/B with:
  - `MPTDB_PROVIDER_BENCH_MDBX_WRITE_MODE=full|plain|noop`
  - `MPTDB_PROVIDER_BENCH_PARALLEL_MDBX_WRITE=1|0`.

Key evidence (`full` mode, parallel MDBX write):

- Representative ranges (10 blocks per iter):
  - `evm ~1.12-1.15s/iter` (`~112-115ms/blk`)
  - `sc_write ~0.63-0.65s/iter` (`~63-65ms/blk`)
  - `write_wall ~0.76-0.79s/iter` (`~76-79ms/blk`)
  - `mdbx_worker ~0.75-0.78s/iter`
    - `write_state ~0.49-0.51s/iter`
    - `commit ~0.25-0.27s/iter`
  - `mdbx_wait ~0.12-0.15s/iter`
  - `prepare ~1.2-1.3ms/iter`, `enqueue ~45-60us/iter`, `residual ~few us`
- SC internal profile in the same runs:
  - `total_commit ~18-19ms/blk`
  - `account_root ~9-10ms/blk`

Control (`noop` mode, parallel MDBX write):

- `write_wall` collapses to almost `sc_write`:
  - `sc_write ~0.61-0.64s/iter`
  - `write_wall ~0.62-0.64s/iter`
- MDBX side is near-zero:
  - `mdbx_worker ~0.25ms-1.08ms/iter`
  - `mdbx_wait ~30-50us/iter`

Control (`full` mode, serial MDBX write):

- `write_wall` jumps to `~1.33-1.37s/iter` (`~133-137ms/blk`), much worse than parallel mode.
- This validates that parallel overlap is already helping; serial is not a fix.

Interpretation:

- In current provider benchmark semantics, `write_wall` includes waiting for MDBX side completion.
- After P1.4, SC root path is no longer the dominant residual (`account_root ~9-10ms/blk`).
- The dominant non-EVM remainder is MDBX full write path under overlap:
  - first-order cost = `MDBX write_state + MDBX commit`
  - not SC root hash.
- This supersedes the earlier coarse inference in `9.7.3` that MDBX overlap was not first-order;
  finer breakdown shows MDBX full-write is now the largest remaining block-lifecycle component.

Practical conclusion for architecture discussion:

- mptdb (SC/RocksDB) write+root path itself is fast in this workload window.
- Remaining `write_wall` inflation mainly comes from dual-write integration shape
  (SC write + MDBX full write in the same block lifecycle).
- If provider moves PlainState ownership to mptdb (or minimizes MDBX full-write responsibility),
  current `write_wall` ceiling can drop materially without touching EVM logic.

### 9.17 Migration Plan: `mptdb-primary-state` (Draft, April 3, 2026)

Objective:

- Remove MDBX Full-write from hot block lifecycle while preserving correctness and reorg behavior.
- Make mptdb the primary owner of state read/write on provider path.

#### 9.17.1 Phase 1: `mptdb-primary-state` switch (functional cut-in)

Proposed switches:

- `MPTDB_PROVIDER_PRIMARY_STATE=0|1` (default `0`)
  - `0`: current behavior (MDBX primary read/write + SC root path).
  - `1`: provider read/write prefers mptdb primary state path.
- `MPTDB_PROVIDER_MDBX_SHADOW_MODE=full|plain|noop` (default `full` in shadow stage)
  - `full`: keep existing MDBX write for safe rollout.
  - `plain`: minimal PlainState shadow write.
  - `noop`: no MDBX state write (target mode after full cutover).

Execution semantics when `PRIMARY_STATE=1`:

- EVM reads prioritize mptdb state provider (MDBX only as fallback path during rollout).
- Post-EVM state writes go to mptdb primary state tables and SC root pipeline first.
- MDBX write lane is controlled only by `MDBX_SHADOW_MODE` (for migration safety, not correctness source).

Acceptance criteria (Phase 1):

- Existing integration tests all pass.
- `erc20_transfer_10pct_contract_pool` keeps or improves current `block_lifecycle`.
- No correctness mismatch in baseline sanity checks.

#### 9.17.2 Phase 2: Shadow accounting window (dual-write + sampled reconciliation)

Purpose:

- Keep migration safety while reducing blast radius before final MDBX cut.

Proposed reconciliation strategy:

- Continue dual-write for a defined burn-in window.
- Per block, sample and compare:
  - touched accounts: nonce/balance/code_hash
  - touched storage slots
  - random global accounts/slots (fixed seed for reproducibility)
- Keep strict mismatch logs with block number and key identity.

Policy:

- Benchmark/test mode: mismatch is hard-fail.
- Runtime rollout mode: mismatch is hard-fail by default; optional temporary warn-only gate can exist behind explicit flag.

Exit criteria (Phase 2):

- Zero mismatch for agreed burn-in window.
- Stable perf under `PRIMARY_STATE=1` with `MDBX_SHADOW_MODE=plain|noop` A/B.

#### 9.17.3 Phase 3: Reorg/history parity via SC WAL

Purpose:

- Ensure removing MDBX primary state writes does not break rollback semantics.

Required capabilities:

- WAL carries enough pre-image/change data to reconstruct per-block revert.
- Implement and validate rollback API semantics equivalent to current provider expectations.
- Define retention/pruning policy so WAL rollback horizon is explicit and testable.

Validation:

- Deterministic reorg tests (single-step and multi-step rollback/replay).
- History query parity checks for supported range.

Exit criteria (Phase 3):

- Reorg/history tests pass under `PRIMARY_STATE=1` and `MDBX_SHADOW_MODE=noop`.
- No behavior regression against current provider contract.

#### 9.17.4 Phase 4: Final cutover

Actions:

- Default `PRIMARY_STATE=1`.
- Default `MDBX_SHADOW_MODE=noop`.
- Remove/retire MDBX primary responsibility for `PlainAccountState` and `PlainStorageState`.

Success metric (integration focus):

- On `erc20_transfer_10pct_contract_pool`, `write_wall` should materially drop from current
  dual-write baseline (where `mdbx_worker(write+commit)` is dominant).
- Keep B4.6/B4.7/B4.8 profile guardrails green.

Risk control:

- Keep parity diagnostics and rollback tooling, but do not re-enable legacy commit mode.

### 9.18 Decision Update: Shift Focus to WAL-first Plain Materialization (April 4, 2026)

Latest observations on `erc20_transfer_10pct_contract_pool`:

- With `PRIMARY_STATE=1` and `PRIMARY_STATE_READS=0`, mptdb lane is currently faster than
  reth_mdbx in block lifecycle runs (same benchmark window/settings).
- With `PRIMARY_STATE_READS=1`, total time regresses mainly in EVM read path (SS read mode),
  while SC commit/root itself is not the dominant contributor.
- In SC profile runs, `total_commit` is around `~18-19ms/blk` in this dataset window.

Important measurement caveat:

- `MPTDB_PROVIDER_BENCH_SC_PROFILE=1` currently uses
  `apply_execution_outcome + commit_with_profile` in the bench loop.
- That path is SC-focused and does not represent full end-to-end SS mirror write cost.
- Use non-profile `write_state` runs for final block lifecycle decisions.

Conclusion:

- Further SS read-path tuning is currently lower ROI for integration throughput.
- Primary optimization target should shift to reducing synchronized plain-state persistence cost
  in the hot block lifecycle.

Next implementation direction (P-next):

1. Introduce WAL-first plain state journal in provider path:
   - append per-block plain-state delta to sequential WAL first (durable boundary at block level).
2. Async plain materialization worker:
   - replay WAL deltas to MDBX plain tables out of hot path, in strict block-number order.
3. Keep correctness guardrails during migration:
   - maintain `applied_block` watermark for replay progress,
   - sampled shadow reconciliation for touched accounts/slots,
   - explicit rollback/reorg semantics driven by WAL horizon policy.
4. Benchmark target:
   - materially reduce `write_wall` under `PRIMARY_STATE=1`,
   - keep B4.6/B4.7/B4.8 profile guardrails green.

### 9.19 Prefill Alignment Finding (April 4, 2026)

Current diagnosis:

- The main instability is now in integration prefill path alignment, not in the normal
  `erc20_transfer_10pct_contract_pool` write pipeline itself.
- Under extreme B5.0 scale (`1,000,000` prefill accounts, `100` blocks), mptdb lane hit:
  `update_storage_leaf ... attempted to update blind node ...` during SC `write_state`.
- The same code path is stable on:
  - `erc20_transfer_10pct_contract_pool` default size (`100k/20k/10blk`),
  - reduced `erc20_transfer_10pct_contract_pool` (`20k/5k/5blk`),
  - reduced B5.0 (`100k/20k/10blk`).

Interpretation:

- Integration benchmark prefill semantics are currently not fully aligned with the SC-side
  prefill expectations under extreme scale.
- The practical next step is to align provider prefill behavior with SC canonical prefill
  semantics (chunking/order/reveal readiness/flush boundary), then re-run B5.0 and
  `erc20_transfer_10pct_contract_pool` as the decision workload.

Action item:

- Treat prefill alignment as P0 before further throughput tuning conclusions.
- Only compare mptdb vs reth_mdbx after prefill alignment is fixed and B5.0 no longer
  triggers blind-node updates.
