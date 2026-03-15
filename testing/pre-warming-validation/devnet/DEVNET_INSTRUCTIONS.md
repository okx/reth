# Pre-Warming Devnet/Testnet Comparison Guide

## Overview

Compare node performance **with** and **without** the pre-warming feature on a live devnet/testnet.

---

## Prerequisites

- `curl` and `python3` installed on the node machine
- Metrics endpoint accessible (port 9001)
- RPC endpoint accessible (port 8545)
- Ensure `devnet_comparison.sh` is available on the machine where node is running

---

## Pre-warming CLI Flags

When enabling pre-warming, add these flags to your node startup command:

```bash
--txpool.pre-warming true \
--txpool.pre-warming-workers <NUM_CPUS> \
--txpool.pre-fetch-workers <NUM_CPUS>
```

Choose the best possible number of CPUs for your machine.

| Flag | Description | Default |
|------|-------------|---------|
| `--txpool.pre-warming` | Enable/disable pre-warming | `false` |
| `--txpool.pre-warming-workers` | Parallel simulation workers | All CPUs |
| `--txpool.pre-fetch-workers` | Parallel prefetch workers | All CPUs |

---

## Testing Workflow

### Phase 1: Capture Baseline (Pre-warming DISABLED)

1. Ensure node is running **without** `--txpool.pre-warming true`

2. Verify metrics endpoint:
```bash
curl -s http://localhost:9001/metrics | grep "reth_" | head -3
```

3. Run capture:
```bash
./devnet_comparison.sh <HOST> <METRICS_PORT> <DURATION_MINUTES> <OUTPUT_FILE>

# Example: capture for 30 minutes
./devnet_comparison.sh localhost 9001 30 results_prewarm_OFF.json
```

The script captures metrics at start and end of the duration, then calculates the delta.

---

### Phase 2: Capture with Pre-warming ENABLED

1. Restart node **with** pre-warming flags:
```bash
--txpool.pre-warming true \
--txpool.pre-warming-workers <NUM_CPUS> \
--txpool.pre-fetch-workers <NUM_CPUS>
```

2. Wait 5-10 minutes for stabilization

3. Verify pre-warming is active:
```bash
curl -s http://localhost:9001/metrics | grep "pre_warming" | head -3
```

4. Run capture:
```bash
./devnet_comparison.sh <HOST> <METRICS_PORT> <DURATION_MINUTES> <OUTPUT_FILE>

# Example: capture for 30 minutes (use same duration as Phase 1)
./devnet_comparison.sh localhost 9001 30 results_prewarm_ON.json
```

---

### Phase 3: Compare Results

```bash
./devnet_comparison.sh --compare results_prewarm_OFF.json results_prewarm_ON.json
```

**Output includes:**
- TPS comparison
- Cache hit rate comparison
- Block execution time comparison
- State root calculation time comparison
- Pre-warming statistics (simulations, prefetch ops)
- **Key findings with verdict**

---

## Quick Reference

| Action | Command |
|--------|---------|
| Capture OFF (30 min) | `./devnet_comparison.sh localhost 9001 30 results_OFF.json` |
| Capture ON (30 min) | `./devnet_comparison.sh localhost 9001 30 results_ON.json` |
| Compare | `./devnet_comparison.sh --compare results_OFF.json results_ON.json` |

---

## Expected Outcome

With pre-warming enabled, expect:

- Higher cache hit rate
- Faster block execution
- Faster state root calculation
- Simulations > 0 (indicates pre-warming is working)
- Prefetch operations > 0 (indicates prefetch is working)

---

## Metrics Reference

### Always-On Metrics (tracked regardless of pre-warming)

| Metric | Description |
|--------|-------------|
| `reth_payloads_cached_reads_hits` | Cache hits in CachedReads during EVM execution |
| `reth_payloads_cached_reads_misses` | Cache misses in CachedReads during EVM execution |

### Pre-warming Specific Metrics (only when pre-warming is ON)

| Metric | Description |
|--------|-------------|
| `reth_txpool_pre_warming_simulations_completed` | Number of transaction simulations completed |
| `reth_txpool_pre_warming_prefetch_operations` | Number of prefetch operations performed |
| `reth_txpool_pre_warming_prefetch_accounts` | Number of accounts prefetched |
| `reth_txpool_pre_warming_cache_hits` | Pre-warming specific cache hits |
| `reth_txpool_pre_warming_cache_misses` | Pre-warming specific cache misses |

---

## Troubleshooting

| Issue | Check |
|-------|-------|
| Cannot connect to metrics | `curl -s http://localhost:9001/metrics \| head -1` |
| No pre-warming metrics | Verify `--txpool.pre-warming true` in node flags |
| Low cache hit rate | Wait longer after restart, ensure traffic is flowing |
| Simulations = 0 | Ensure ERC20 transactions are being sent (not just ETH transfers) |

---

## What to Report

After comparison, the script outputs a verdict. Key metrics to report:

```
Pre-warming Performance Results:
- Cache Hit Rate: 62.5% → 95.0% (+32.5%)
- Block Execution: -14.6% (faster)
- State Root: -16.2% (faster)
- Simulations: 5,006 completed
- Verdict: BENEFICIAL - recommend enabling
```
