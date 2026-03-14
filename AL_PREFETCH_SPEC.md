# Spec: Access-List-Driven Prefetch-Only Mode

**Branch target:** `feature/al-prefetch-only` (off `dev`)
**Repo:** `reth-al-feature` — clean dev clone, no pre-warming infrastructure
**Status:** In Progress
**Date:** 2026-03-14

---

## 1. Purpose

The current pre-warming system simulates every transaction in the background using a
full EVM to discover which storage slots it will access. This works well, but has cost:
~10 ms CPU per transaction, background rayon workers, a DashMap cache, and ~30 ms
prefetch overhead at block build time.

This feature tests a fundamentally simpler hypothesis:

> **If transactions already carry correct EIP-2930 access lists, can we skip
> simulation entirely and achieve the same cache hit rate — with lower latency,
> less infrastructure, and measurable proof via metrics?**

The goal is not just to build the feature. The goal is to **measure everything** —
TPS, transaction latency (arrival → inclusion), cache hit/miss, block build breakdown,
prefetch overhead — and produce a data-driven comparison across all modes.

---

## 2. Current Architecture (Simulation-Based)

```
 TRANSACTION ARRIVAL
       │
       ▼
 ┌─────────────────────┐
 │   Transaction Pool  │  record_tx_arrival(hash) → DashMap<TxHash, Instant>
 └──────────┬──────────┘
            │ trigger_simulation()
            ▼
 ┌──────────────────────────────────────────────────┐
 │   SimulationWorkerPool  (rayon thread pool)       │
 │                                                  │
 │   simulate() → TrackingDatabase → EVM run         │
 │             → ExtractedKeys (accounts + slots)    │
 └──────────────────────┬───────────────────────────┘
                        │ store_tx_keys(hash, keys)
                        ▼
            ┌───────────────────────┐
            │   PreWarmedCache      │
            │   DashMap<TxHash,     │
            │   Arc<ExtractedKeys>> │
            └──────────┬────────────┘
                       │
 BLOCK BUILD TIME      │
       │               │ get_all_prewarmed_keys() → merge all keys
       ▼               ▼
 ┌────────────────────────────────────────┐
 │   prefetch_with_arcs_sync()            │
 │   std::thread::scope parallel MDBX     │
 │   reads → CachedReads populated        │
 └──────────────────┬─────────────────────┘
                    │
                    ▼
 ┌─────────────────────────────────────────┐
 │   EVM execution                         │
 │   All accessed state: cache hits        │
 │   take_tx_arrival_time(hash) → dwell    │
 └─────────────────────────────────────────┘
```

---

## 3. New Architecture (Access-List-Driven Prefetch-Only)

```
 TRANSACTION ARRIVAL
       │
       ▼
 ┌─────────────────────┐
 │   Transaction Pool  │  record_tx_arrival(hash) → DashMap<TxHash, Instant>
 └──────────┬──────────┘
            │ NO simulation triggered
            │ TX sits in pool with its access_list field intact
            │
 BLOCK BUILD TIME
       │
       ▼
 ┌───────────────────────────────────────────────────┐
 │   extract_keys_from_al(pool)                       │
 │                                                   │
 │   for tx in pool.pending_transactions():          │
 │       if let Some(al) = tx.access_list():         │
 │           for item in al:                         │
 │               keys.add_account(item.address)      │
 │               keys.add_storage_slot(...)          │
 │   → ExtractedKeys (merged, deduplicated)          │
 └──────────────────────┬────────────────────────────┘
                        │  (same function as before)
                        ▼
 ┌────────────────────────────────────────┐
 │   prefetch_with_arcs_sync()            │
 │   std::thread::scope parallel MDBX     │
 │   reads → CachedReads populated        │
 └──────────────────┬─────────────────────┘
                    │
                    ▼
 ┌─────────────────────────────────────────┐
 │   EVM execution                         │
 │   All accessed state: cache hits        │
 │   take_tx_arrival_time(hash) → dwell    │
 └─────────────────────────────────────────┘
```

**What is removed:** SimulationWorkerPool, PreWarmedCache, rayon thread pool,
TrackingDatabase, DashMap eviction, snapshot state management.

**What stays:** record_tx_arrival, take_tx_arrival_time, prefetch_with_arcs_sync,
CachedReads, all Prometheus metrics.

---

## 4. The External AL Injector (Iteration 2 Preview)

For transactions that do NOT carry an EIP-2930 access list (normal production traffic),
a lightweight injector runs at TX arrival time — **no EVM, no simulation**:

```
 TX arrives (no access list)
       │
       ▼
 ┌──────────────────────────────────────────┐
 │   AL Injector (single background thread) │
 │                                          │
 │   selector == ERC20 transfer?            │
 │     → keccak256(sender   ‖ slot_0)       │
 │     → keccak256(recipient ‖ slot_0)      │
 │   selector == ERC20 approve?             │
 │     → keccak256(owner     ‖ slot_1)      │
 │     → keccak256(spender   ‖ slot_1)      │
 │   unknown selector?                      │
 │     → skip                               │
 └──────────────────┬───────────────────────┘
                    │
                    ▼
        ┌─────────────────────────┐
        │  SyntheticALRegistry    │
        │  TxHash → AL keys (TTL) │
        └──────────────────────────┘
                    │
 BLOCK BUILD TIME   │
       │            ▼
       └──► extract_keys_from_al() reads both:
            - tx.access_list()          (real EIP-2930)
            - SyntheticALRegistry.get() (injected keys)
```

**This injector is Iteration 2 scope.** Iteration 1 uses only real access lists.

---

## 5. Metrics — The Full Picture

This is the core of the experiment. Every mode change must be validated
quantitatively. Below is every metric tracked, what it measures, and how to
read it.

---

### 5.1 Throughput

| Metric | Source | What It Tells You |
|--------|--------|-------------------|
| **Avg TPS** | `adventure log: Average BTPS` (final cumulative value) | True sustained throughput — total TX confirmed / elapsed seconds |
| **Max TPS** | adventure log | Peak burst throughput |
| **Min TPS** | adventure log | Worst-case trough (cold start, GC spikes) |
| **Blocks/sec** | `(final_block − initial_block) / wall_time` | Block production rate; 2.5/s = 400ms slots |
| **TXs confirmed** | adventure `totalTxCount` | Ground truth: TXs finalised in sealed blocks |

**Target for AL prefetch:** Avg TPS ≥ 5,500 (no regression vs OFF baseline of 5,693)

---

### 5.2 Transaction Latency (Arrival → Inclusion)

| Metric | Prometheus Name | What It Tells You |
|--------|----------------|-------------------|
| **Avg dwell time** | `reth_txpool_pre_warming_tx_pool_dwell_time_sum / _count` | How long a TX waits in pool before being sealed in a block |
| **Dwell count** | `_count` delta | Number of TXs measured (should match TXs confirmed) |

**How it is measured:**
```
record_tx_arrival(hash)  ← called at pool insertion (pool/mod.rs)
        ↓ time passes (TX queued in mempool)
take_tx_arrival_time(hash) ← called at block seal (builder.rs)
        ↓
dwell_secs = inclusion_time - arrival_time
metrics.tx_pool_dwell_time.record(dwell_secs)
```

**Why this matters:**
- High dwell time = transactions queued for many blocks before selection
- Very low dwell time = TX selected in the same block it arrived (ideal)
- In AL prefetch mode, dwell time should be similar to or lower than simulation mode
  because there is no simulation delay before keys are usable

**In metrics.json:**
```json
"tx_latency": {
  "avg_ms": 185.4,
  "count": 792995
}
```

---

### 5.3 Cache Effectiveness

| Metric | Prometheus Name | What It Tells You |
|--------|----------------|-------------------|
| **Cache hits** | `reth_payloads_cached_reads_hits` (delta) | State reads satisfied from CachedReads (no MDBX I/O) |
| **Cache misses** | `reth_payloads_cached_reads_misses` (delta) | State reads that fell through to MDBX during execution |
| **Hit rate** | `hits / (hits + misses) × 100` | Key indicator — target ≥ 95% for AL prefetch to be effective |

**What hit rate means in practice:**

| Hit Rate | Interpretation |
|----------|----------------|
| < 20%    | Prefetch not working — wrong keys or no prefetch |
| 20–60%   | Partial coverage — some keys missing or wrong slots |
| 60–90%   | Good coverage — heuristics working for most TXs |
| ≥ 95%    | Excellent — nearly all state pre-loaded, MDBX I/O during execution minimal |
| ~100%    | Perfect — access list exactly matches execution trace |

**In metrics.json:**
```json
"cache": {
  "hits": 4821043,
  "misses": 53891,
  "hit_rate_percent": 98.9
}
```

---

### 5.4 Block Build Timing Breakdown

This is the most detailed view of where time is spent per block.

| Metric | Prometheus Name | What It Tells You |
|--------|----------------|-------------------|
| **TX execution** | `reth_block_timing_build_exec_mempool_transactions` | Time to run all TXs in the EVM (sum/count → avg ms) |
| **State root** | `reth_block_timing_build_calc_state_root` | Time to compute Merkle trie root after execution |
| **Total build** | `reth_block_timing_build_total` | End-to-end block build time per slot |
| **Prefetch/block** | `reth_txpool_pre_warming_prefetch_duration` / total build count | Prefetch overhead amortised across all build_payload() calls |

**How each component relates to TPS:**

```
400ms block slot
├── prefetch        (~30ms with pre-warming, ~2ms target for AL prefetch)
├── TX execution    (~136ms ON, ~158ms OFF)
├── state root      (~80ms ON, ~133ms OFF)
└── remaining       (~154ms for batcher, network, consensus)
```

State root is superlinear in unique accounts touched: more TXs/block → more trie paths
updated → disproportionately larger state root time. Reducing TX exec time via cache
hits also indirectly allows more TXs/block → more state root work. This is the tension.

**Slot utilisation:** `total_build_avg / block_time_ms × 100`
- Target: < 60% (leaves headroom for batcher + network)
- Current ON: 218ms / 400ms = 54.6% ✓
- Current OFF: 298ms / 400ms = 74.6% — tight

**In metrics.json:**
```json
"block_timing": {
  "prefetch_per_block_avg_ms": 30.8,
  "tx_execution_avg_ms": 136.7,
  "state_root_avg_ms": 80.1,
  "total_block_build_avg_ms": 218.3,
  "slot_utilisation_pct": 54.6,
  "breakdown_pct": {
    "prefetch": 14.1,
    "tx_execution": 62.6,
    "state_root": 36.7
  }
}
```

---

### 5.5 Prefetch Performance

| Metric | Prometheus Name | What It Tells You |
|--------|----------------|-------------------|
| **Prefetch duration (per op)** | `reth_txpool_pre_warming_prefetch_duration` | Time for one full prefetch call (parallel MDBX reads) |
| **Prefetch operations** | `reth_txpool_pre_warming_prefetch_operations` | How many times prefetch ran (≈ blocks / 2 due to guard) |
| **Accounts prefetched** | `reth_txpool_pre_warming_prefetch_accounts` | Account state entries loaded from MDBX |
| **Storage slots prefetched** | `reth_txpool_pre_warming_prefetch_storage_slots` | Storage entries loaded from MDBX |

**New metrics for AL prefetch mode (Iteration 1):**

| Metric | Prometheus Name | What It Tells You |
|--------|----------------|-------------------|
| **AL TX count** | `reth_txpool_al_prefetch_tx_count` | Pending TXs with access lists at each prefetch call |
| **Keys extracted** | `reth_txpool_al_prefetch_keys_extracted` | Total keys read from access lists per call |
| **Key extraction time** | `reth_txpool_al_prefetch_duration` | Time to iterate pool and read access lists (μs range) |

**Expected difference:** Current prefetch takes 30.8ms because MDBX reads dominate.
AL key extraction should be < 1ms (pure memory iteration). The MDBX read time stays
the same — what changes is the pipeline: keys are now available at build time rather
than after simulation completes.

---

### 5.6 Simulation Metrics (Reference / Disabled in AL Mode)

| Metric | Prometheus Name | What It Tells You |
|--------|----------------|-------------------|
| **Simulations completed** | `reth_txpool_pre_warming_simulations_completed` | Workers successfully simulated TXs |
| **Simulations failed** | `reth_txpool_pre_warming_simulations_failed` | Timeouts, panics, EVM errors |
| **With access list** | `reth_txpool_pre_warming_simulations_with_access_list` | TXs that hit PRIORITY 0 (AL fast path) |
| **Simulation avg** | `reth_txpool_pre_warming_simulation_duration` | Per-TX simulation time (ms) |
| **Sim CPU total** | sum of simulation_duration | Total CPU seconds consumed by workers |

In AL prefetch-only mode (simulation OFF), all of these should read **0**.
This is itself a verification: if `simulations_completed > 0`, something is wrong.

---

### 5.7 Complete Metrics.json Layout

Every benchmark run produces this file. This is the canonical record for comparison:

```json
{
  "timestamp": "2026-03-14T10:00:00+00:00",
  "prewarming_enabled": false,
  "al_prefetch_enabled": true,
  "access_list_enabled": true,
  "duration_seconds": 180,
  "blocks_processed": 450,
  "transactions_processed": 810000,

  "avg_tps": 5687,
  "max_tps": 6102,
  "min_tps": 4891,
  "blocks_per_sec": 2.5,

  "tx_latency": {
    "avg_ms": 185.4,
    "count": 810000
  },

  "block_timing": {
    "prefetch_per_block_avg_ms": 2.1,
    "tx_execution_avg_ms": 134.0,
    "state_root_avg_ms": 78.0,
    "total_block_build_avg_ms": 214.0,
    "block_time_ms": 400.0,
    "slot_utilisation_pct": 53.5
  },

  "cache": {
    "hits": 4900000,
    "misses": 52000,
    "hit_rate_percent": 98.9
  },

  "al_prefetch": {
    "tx_count_avg": 4800,
    "keys_extracted_avg": 9600,
    "extraction_avg_us": 420
  },

  "simulation": {
    "completed": 0,
    "failed": 0,
    "with_access_list": 0,
    "coverage_pct": "0"
  },

  "prefetch": {
    "duration_per_op_avg_ms": 2.1,
    "operations": 225,
    "accounts": 1080000,
    "storage_slots": 1620000
  }
}
```

---

## 6. Benchmark Scenarios

Every claim must be backed by a benchmark run. These are the required scenarios:

| Scenario | PRE_WARMING | AL_PREFETCH | USE_ACCESS_LIST | Purpose |
|----------|-------------|-------------|-----------------|---------|
| **A: Baseline OFF** | false | false | false | Reference floor |
| **B: Current ON** | true | false | false | Current best (heuristic) |
| **C: Current ON + AL** | true | false | true | Shows AL via simulation |
| **D: AL Prefetch** | false | true | true | New feature — Iteration 1 |
| **E: AL Prefetch + Inject** | false | true | false | Iteration 2 (synthetic AL) |

**Minimum required runs to publish results:** A, B, D (compare three modes).

---

## 7. Implementation Plan

### Iteration 1 — Prove AL Prefetch

**Branch:** `feature/al-prefetch-only`

| Step | File | Change |
|------|------|--------|
| 1 | `config.rs` | Add `al_prefetch_only: bool` read from `TXPOOL_AL_PREFETCH_ONLY` env |
| 2 | `bridge.rs` | Add `extract_keys_from_al(pool) → Option<ExtractedKeys>` |
| 3 | `metrics.rs` | Add `al_prefetch_tx_count`, `al_prefetch_keys_extracted`, `al_prefetch_duration` |
| 4 | `builder.rs` | Add AL prefetch branch in `build_payload()` alongside existing branch |
| 5 | `benchmark-run.sh` | Capture `AL_PREFETCH_ENABLED` env var + new metrics |
| 6 | Run Scenarios A, B, D | Produce 3 `metrics.json` files |
| 7 | Update `PREWARMING_BENCHMARK_RESULTS.md` | Add AL prefetch row to comparison table |

**Definition of done:**
- Scenario D cache hit rate ≥ 95%
- AL key extraction time < 2 ms (vs simulation ~10 ms/TX)
- TPS not worse than Scenario A (baseline)
- `simulations_completed == 0` in Scenario D (workers truly off)

### Iteration 2 — Synthetic AL for Non-AL Transactions

**Branch:** `feature/al-prefetch-synthetic`

| Step | File | Change |
|------|------|--------|
| 1 | `al_injector.rs` | New file: lightweight heuristic injector |
| 2 | `registry.rs` | Add `SYNTHETIC_AL_REGISTRY: OnceLock<Arc<DashMap<TxHash, SyntheticAL>>>` |
| 3 | `pool/mod.rs` | Start injector thread on init, call on TX arrival |
| 4 | `bridge.rs` | Extend `extract_keys_from_al` to merge synthetic registry |
| 5 | `config.rs` | Add `al_injector: bool` from `TXPOOL_AL_INJECTOR` env |
| 6 | Run Scenario E | Compare vs Scenario B (current heuristic) |

---

## 8. Expected Results Summary

| Scenario | TPS | TX Exec | State Root | Total Build | Hit Rate | Prefetch | Dwell Time |
|----------|-----|---------|------------|-------------|----------|----------|------------|
| A: OFF | 5,693 | 158.7ms | 133.1ms | 298.2ms | 13.5% | 0ms | tbd |
| B: Pre-warming ON | 5,687 | 136.7ms | 80.1ms | 218.3ms | 98.9% | 30.8ms | tbd |
| D: AL Prefetch | ~5,700 | ~135ms | ~78ms | ~215ms | ≥95% | **~2ms** | tbd |
| E: AL + Inject | ~5,700 | ~135ms | ~78ms | ~215ms | ≥95% | **~2ms** | tbd |

**Key differentiator for AL Prefetch:** The prefetch overhead drops from ~30ms to ~2ms
because key extraction (reading access list fields) is pure memory work with no MDBX
I/O, versus simulation which must run EVM code. The MDBX read phase stays the same.

---

## 9. Open Questions

| # | Question | Notes |
|---|----------|-------|
| 1 | Should AL prefetch run in parallel with simulation (both ON) or exclusively? | Start exclusive for clean comparison |
| 2 | What happens to TXs with partial AL (some slots missing)? | Measure miss rate, document which slots were absent |
| 3 | Should `extract_keys_from_al` cap the number of TXs scanned? | Start with full pool; add cap if latency > 2ms |
| 4 | How does dwell time change between simulation and AL prefetch modes? | Key question — AL has no simulation delay |
| 5 | What is the right TTL for synthetic AL entries (Iter 2)? | Match block time: 400ms |
