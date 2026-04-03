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
