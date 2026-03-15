# Transaction Execution Timing & Tracing Guide

## Overview

This document describes how to capture timing information for different phases of transaction execution with pre-warming enabled.

## Architecture

Timing is captured via two systems:

1. **Pre-Warming Metrics** (`reth_txpool_pre_warming_*`) - Simulation and prefetch timing
2. **Block Timing Metrics** (`block_timing_*`) - Block build and execution timing

Both expose Prometheus histograms and are logged for analysis.

## Timing Phases

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    TRANSACTION LIFECYCLE TIMING                              │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│  1. SIMULATION PHASE (Background, per-transaction)                          │
│     ├─ Triggered when TX enters mempool                                     │
│     └─ Metric: reth_txpool_pre_warming_simulation_duration                  │
│                                                                              │
│  2. PREFETCH PHASE (Before block execution)                                 │
│     ├─ MDBX Query Time: Time to fetch values from database                  │
│     └─ Metric: reth_txpool_pre_warming_prefetch_duration                    │
│                                                                              │
│  3. BLOCK BUILD PHASE (Captured by BlockTimingContext)                      │
│     ├─ block_timing_build_apply_pre_execution_changes                       │
│     ├─ block_timing_build_exec_sequencer_transactions                       │
│     ├─ block_timing_build_select_mempool_transactions                       │
│     ├─ block_timing_build_exec_mempool_transactions                         │
│     ├─ block_timing_build_calc_state_root (includes state root calc)        │
│     └─ block_timing_build_total                                             │
│                                                                              │
│  4. BLOCK INSERT PHASE                                                       │
│     ├─ block_timing_insert_validate_and_execute                             │
│     ├─ block_timing_insert_insert_to_tree                                   │
│     └─ block_timing_insert_total                                            │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Available Metrics (Prometheus)

### Pre-Warming: Simulation Timing
```
reth_txpool_pre_warming_simulation_duration{quantile="0.5"}
reth_txpool_pre_warming_simulation_duration{quantile="0.9"}
reth_txpool_pre_warming_simulation_duration{quantile="0.99"}
reth_txpool_pre_warming_simulation_duration_sum
reth_txpool_pre_warming_simulation_duration_count
```

### Pre-Warming: Prefetch Timing
```
reth_txpool_pre_warming_prefetch_duration{quantile="0.5"}
reth_txpool_pre_warming_prefetch_duration{quantile="0.9"}
reth_txpool_pre_warming_prefetch_duration{quantile="0.99"}
reth_txpool_pre_warming_prefetch_duration_sum
reth_txpool_pre_warming_prefetch_duration_count
```

### Pre-Warming: Cache Effectiveness
```
reth_txpool_pre_warming_cache_hits
reth_txpool_pre_warming_cache_misses
reth_txpool_pre_warming_simulations_completed
reth_txpool_pre_warming_simulations_failed
reth_txpool_pre_warming_prefetch_accounts
reth_txpool_pre_warming_prefetch_storage_slots
```

### EVM Execution Cache
```
reth_sync_caching_account_cache_hits
reth_sync_caching_account_cache_misses
reth_sync_caching_storage_cache_hits
reth_sync_caching_storage_cache_misses
```

### Block Timing: Build Phase
```
block_timing_build_apply_pre_execution_changes{quantile="0.5"}
block_timing_build_exec_sequencer_transactions{quantile="0.5"}
block_timing_build_select_mempool_transactions{quantile="0.5"}
block_timing_build_exec_mempool_transactions{quantile="0.5"}
block_timing_build_calc_state_root{quantile="0.5"}
block_timing_build_total{quantile="0.5"}
```

### Block Timing: Insert Phase
```
block_timing_insert_validate_and_execute{quantile="0.5"}
block_timing_insert_insert_to_tree{quantile="0.5"}
block_timing_insert_total{quantile="0.5"}
```

## Log Patterns for Timing Extraction

Run node with `--log.stdout.filter info` to see timing logs (default level is sufficient).

### Simulation Timing (per transaction)
```bash
# Extract simulation times from logs
grep "SIMULATION: Complete" chain.log
```

Example log:
```
[INFO] SIMULATION: Complete worker_id=3 tx_hash=0x123... simulation_duration_us=1523
```

### Prefetch Timing (per block)
```bash
# Extract prefetch times from logs
grep "PREFETCH:" chain.log
```

Example logs:
```
[INFO] PREFETCH: MDBX queries completed accounts_fetched=15 storage_fetched=42 bytecode_fetched=3 mdbx_query_ms=12
[INFO] PREFETCH: Metrics recorded prefetch_duration_ms=15 mdbx_query_ms=12
```

Fields:
- `simulation_duration_us` - Simulation time in microseconds
- `prefetch_duration_ms` - Total prefetch time in milliseconds
- `mdbx_query_ms` - Time spent querying MDBX database in milliseconds

### Calculating Derived Metrics

**Average Simulation Time:**
```bash
curl -s http://localhost:9001/metrics | grep "simulation_duration_sum\|simulation_duration_count" | awk '
/sum/ {sum=$2}
/count/ {count=$2}
END {if(count>0) printf "Avg Simulation: %.3f ms\n", (sum/count)*1000}'
```

**Average Prefetch Time:**
```bash
curl -s http://localhost:9001/metrics | grep "prefetch_duration_sum\|prefetch_duration_count" | awk '
/sum/ {sum=$2}
/count/ {count=$2}
END {if(count>0) printf "Avg Prefetch: %.3f ms\n", (sum/count)*1000}'
```

## Quick Timing Check Script

```bash
#!/bin/bash
# timing_check.sh - Quick check of pre-warming timing metrics

METRICS=$(curl -s http://localhost:9001/metrics)

echo "=== PRE-WARMING TIMING METRICS ==="
echo ""

# Simulation duration
SIM_SUM=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulation_duration_sum " | awk '{print $2}')
SIM_COUNT=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulation_duration_count " | awk '{print $2}')
if [ -n "$SIM_COUNT" ] && [ "$SIM_COUNT" != "0" ]; then
    SIM_AVG=$(echo "scale=3; $SIM_SUM / $SIM_COUNT * 1000" | bc)
    echo "Simulation:"
    echo "  Count:    $SIM_COUNT"
    echo "  Avg Time: ${SIM_AVG} ms"
else
    echo "Simulation: No data yet"
fi

echo ""

# Prefetch duration
PRE_SUM=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_duration_sum " | awk '{print $2}')
PRE_COUNT=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_duration_count " | awk '{print $2}')
if [ -n "$PRE_COUNT" ] && [ "$PRE_COUNT" != "0" ]; then
    PRE_AVG=$(echo "scale=3; $PRE_SUM / $PRE_COUNT * 1000" | bc)
    echo "Prefetch:"
    echo "  Count:    $PRE_COUNT"
    echo "  Avg Time: ${PRE_AVG} ms"
else
    echo "Prefetch: No data yet"
fi

echo ""

# Cache effectiveness
HITS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}')
MISSES=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}')
TOTAL=$((${HITS%.*} + ${MISSES%.*}))
if [ "$TOTAL" -gt 0 ]; then
    HIT_RATE=$(echo "scale=1; ${HITS%.*} * 100 / $TOTAL" | bc)
    echo "Cache:"
    echo "  Hits:     $HITS"
    echo "  Misses:   $MISSES"
    echo "  Hit Rate: ${HIT_RATE}%"
else
    echo "Cache: No data yet"
fi
```

## Adding Custom Timing (For Developers)

To add timing to execution phase, instrument the payload builder:

```rust
// In crates/optimism/payload/src/builder.rs

// Before execution
let execution_start = std::time::Instant::now();

// ... execute transactions ...

// After execution
let execution_duration = execution_start.elapsed();
tracing::info!(
    target: "payload_builder",
    execution_duration_ms = execution_duration.as_millis(),
    transactions_executed = tx_count,
    "Block execution complete"
);
```

## Summary Table

| Phase | Metric Name | Type | Description |
|-------|-------------|------|-------------|
| Simulation | `simulation_duration` | Histogram | Time per transaction simulation |
| Prefetch | `prefetch_duration` | Histogram | Time to prefetch from MDBX |
| Block Build | `build_calc_state_root` | Histogram | State root calculation time |
| Block Build | `build_exec_mempool_transactions` | Histogram | Mempool TX execution time |
| Block Build | `build_total` | Histogram | Total block build time |
| Block Insert | `insert_total` | Histogram | Total block insertion time |
| Cache | `cache_hits/misses` | Counter | Pre-warming cache effectiveness |
| EVM Cache | `sync_caching_*` | Counter | Actual execution cache |

## Quick Command: Get All Metrics

```bash
# Unified metrics report
./testing/pre-warming-validation/get_key_metrics.sh localhost 9001

# Or from devnet
./testing/pre-warming-validation/get_key_metrics.sh <DEVNET_IP> 9001
```

## Notes

1. All duration histograms use **seconds** as the unit
2. The `get_key_metrics.sh` script converts to milliseconds for readability
3. Run with `--log.stdout.filter info` to see timing logs
4. Block timing is captured via `BlockTimingContext` in payload builder
5. Pre-warming timing is captured in worker_pool.rs and bridge.rs

