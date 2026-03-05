# Devnet/Testnet Pre-Warming Comparison Guide

## Overview

For live devnet/testnet environments, testing is different from local benchmarks:
- Node runs **continuously** (no restarts during test)
- Feature is enabled/disabled via **deployment** (not CLI flag)
- Metrics are captured **over a time period** (not per transaction burst)

## Script: `devnet_comparison.sh`

### Phase 1: Capture Metrics (Pre-warming OFF)

1. Deploy node **WITHOUT** pre-warming feature:
   ```bash
   op-reth node --datadir /data --txpool.pre-warming false ...
   ```

2. Let the node run with production traffic for a while

3. Capture metrics:
   ```bash
   ./devnet_comparison.sh <DEVNET_IP> <METRICS_PORT> <DURATION_MIN> results_off.json
   ```
   
   Example:
   ```bash
   ./devnet_comparison.sh 192.168.1.100 9001 30 results_prewarm_off.json
   ```

### Phase 2: Capture Metrics (Pre-warming ON)

1. Deploy node **WITH** pre-warming feature:
   ```bash
   op-reth node --datadir /data --txpool.pre-warming true \
     --txpool.pre-warming-workers 12 \
     --txpool.pre-fetch-workers 12 ...
   ```

2. Let the node run with production traffic for the same duration

3. Capture metrics:
   ```bash
   ./devnet_comparison.sh 192.168.1.100 9001 30 results_prewarm_on.json
   ```

### Phase 3: Compare Results

```bash
./devnet_comparison.sh --compare results_prewarm_off.json results_prewarm_on.json
```

Output:
```
══════════════════════════════════════════════════════════════════════════════
  DEVNET PRE-WARMING COMPARISON REPORT
══════════════════════════════════════════════════════════════════════════════

  Test Duration: 30 minutes each
  OFF Captured: 2026-03-04T10:00:00
  ON Captured:  2026-03-04T14:00:00

┌──────────────────────────────────────────────────────────────────────────────┐
│  TRANSACTIONS PER SECOND (TPS)                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│  Pre-warming OFF:       45.50 TPS                                           │
│  Pre-warming ON:        52.30 TPS                                           │
│  Change:               +14.9%                                               │
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│  CACHE HIT RATE                                                             │
├──────────────────────────────────────────────────────────────────────────────┤
│  Pre-warming OFF:       65.0%                                               │
│  Pre-warming ON:        97.5%                                               │
│  Change:               +32.5%                                               │
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│  BLOCK TIMING                                                               │
├──────────────────────────────────────────────────────────────────────────────┤
│  Block Execution (OFF):     0.85 ms                                         │
│  Block Execution (ON):      0.62 ms                                         │
│  Change:                  -27.1%                                            │
│                                                                             │
│  State Root (OFF):          1.20 ms                                         │
│  State Root (ON):           0.95 ms                                         │
│  Change:                  -20.8%                                            │
└──────────────────────────────────────────────────────────────────────────────┘

══════════════════════════════════════════════════════════════════════════════
  SUMMARY
══════════════════════════════════════════════════════════════════════════════

  📊 Cache Hit Rate Change:    +32.5%
  ⚡ TPS Change:               +14.9%
  🕐 Block Execution Change:   -27.1%
  🌳 State Root Change:        -20.8%

══════════════════════════════════════════════════════════════════════════════
```

## Metrics Captured

| Metric | Description |
|--------|-------------|
| **Cache Hit Rate** | `reth_sync_caching_*` - EVM execution cache performance |
| **Block Execution Time** | `reth_block_timing_build_exec_mempool_transactions` |
| **State Root Time** | `reth_block_timing_build_calc_state_root` |
| **Simulations Completed** | `reth_txpool_pre_warming_simulations_completed` |
| **Prefetch Operations** | `reth_txpool_pre_warming_prefetch_operations` |

## Best Practices for Devnet Testing

1. **Same traffic load**: Ensure both tests have similar transaction volumes
2. **Same duration**: Use identical capture durations (e.g., 30 min each)
3. **Wait for warmup**: Let node run for 5-10 minutes before capturing
4. **Multiple runs**: Run multiple captures and average results
5. **Peak hours**: Test during peak traffic for realistic results

## Quick Commands

```bash
# Check if metrics endpoint is reachable
curl http://<DEVNET_IP>:9001/metrics | head -5

# Quick metrics check (using get_key_metrics.sh)
./get_key_metrics.sh <DEVNET_IP> 9001

# 10-minute capture
./devnet_comparison.sh <DEVNET_IP> 9001 10 results.json

# 1-hour capture (for production-like testing)
./devnet_comparison.sh <DEVNET_IP> 9001 60 results.json
```

