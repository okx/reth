# Pre-Warming Benchmark Report: 500K Transactions

**Date:** March 6, 2026  
**Benchmark Type:** Full Load Devnet Simulation v2  
**Target:** 500,000 transactions per phase

---

## Executive Summary

This report documents the benchmark strategy and results for testing the pre-warming feature at scale (500K transactions) to measure:

1. **Cache Hit Rate** improvement with pre-warming enabled vs disabled
2. **Block Execution Time** reduction
3. **State Root Calculation Time** improvement
4. **TPS (Transactions Per Second)** under load

---

## Test Configuration

### Load Parameters

| Parameter | Value | Notes |
|-----------|-------|-------|
| Total Transactions | 500,000 | Per phase (OFF and ON) |
| Parallel Senders | 10 | Max available funded accounts |
| Txns per Sender | 50,000 | Evenly distributed |
| TX Type | `mixed` | 50% ETH, 25% ERC20 transfer, 25% ERC20 transferFrom |
| Block Time | 1 second | Dev mode setting |

### Worker Configuration

| Parameter | Value | Notes |
|-----------|-------|-------|
| Pre-warming Workers | 12 | Max CPU cores |
| Pre-fetch Workers | 12 | Max CPU cores |

### Estimated Runtime

| TX Type | Est. TPS (10 senders) | Time per Phase | Total (both phases) |
|---------|----------------------|----------------|---------------------|
| `eth` | ~250-400 TPS | ~20-35 min | ~40-70 min |
| `mixed` | ~150-250 TPS | ~35-55 min | ~70-110 min |
| `erc20` | ~80-150 TPS | ~55-100 min | ~110-200 min |

**Selected:** `mixed` mode (~70-110 min total)

---

## Test Strategy

### Phase 1: Pre-warming DISABLED (Baseline)

1. Start fresh `op-reth` node with clean datadir
2. Pre-warming flag: `--txpool.pre-warming false`
3. Deploy ERC20 contract
4. Send 500K mixed transactions via 10 parallel senders
5. Wait for blocks to finalize
6. Capture metrics:
   - `reth_payloads_cached_reads_hits`
   - `reth_payloads_cached_reads_misses`
   - `reth_block_timing_build_exec_mempool_transactions_*`
   - `reth_block_timing_build_calc_state_root_*`

### Phase 2: Pre-warming ENABLED

1. Start fresh `op-reth` node with clean datadir
2. Pre-warming flag: `--txpool.pre-warming true`
3. Worker flags:
   - `--txpool.pre-warming-workers 12`
   - `--txpool.pre-fetch-workers 12`
4. Deploy ERC20 contract
5. Send 500K mixed transactions via 10 parallel senders
6. Wait for blocks to finalize
7. Capture same metrics plus:
   - `reth_txpool_pre_warming_simulations_completed`
   - `reth_txpool_pre_warming_prefetch_operations`
   - `reth_txpool_pre_warming_prefetch_accounts`

---

## Command to Execute

```bash
cd /Users/lakshmikanth/Documents/optimisation/reth/testing/pre-warming-validation/devnet-v2

pkill -9 op-reth 2>/dev/null || true

./full_load_devnet_simulation_v2.sh \
  --txns 500000 \
  --senders 10 \
  --tx-type mixed \
  --prewarm-workers 12 \
  --prefetch-workers 12
```

### Generate Report After Completion

```bash
DIR=$(ls -td /Users/lakshmikanth/Documents/optimisation/reth/.devnet-sim-v2-* | head -1)
./devnet_v2_report.sh "$DIR"
```

---

## Results

> **Status:** PENDING - To be filled after benchmark completion

### Cache Performance

| Metric | Pre-warming OFF | Pre-warming ON | Change |
|--------|-----------------|----------------|--------|
| Cache Hits | - | - | - |
| Cache Misses | - | - | - |
| Hit Rate | - | - | - |

### Block Timing

| Metric | Pre-warming OFF | Pre-warming ON | Change |
|--------|-----------------|----------------|--------|
| Block Execution (avg) | - ms | - ms | - |
| State Root Calc (avg) | - ms | - ms | - |

### Throughput

| Metric | Pre-warming OFF | Pre-warming ON | Change |
|--------|-----------------|----------------|--------|
| TPS | - | - | - |
| Total Duration | - min | - min | - |
| Transactions Sent | - | - | - |
| Transactions Failed | - | - | - |

### Pre-warming Statistics (ON phase only)

| Metric | Value |
|--------|-------|
| Simulations Completed | - |
| Prefetch Operations | - |
| Accounts Prefetched | - |

---

## Transaction Breakdown

| Type | Count (OFF) | Count (ON) |
|------|-------------|------------|
| ETH Transfers | - | - |
| ERC20 Transfers | - | - |
| Total | 500,000 | 500,000 |

---

## Key Findings

> To be completed after benchmark run

1. **Cache Hit Rate Improvement:** __%
2. **Block Execution Time Reduction:** __%
3. **State Root Calculation Improvement:** __%
4. **Pre-warming Overhead:** __ TPS difference

---

## Conclusion

> To be completed after benchmark run

---

## Appendix

### A. Metrics Keys Reference

| Metric | Description |
|--------|-------------|
| `reth_payloads_cached_reads_hits` | Execution cache hits (always-on) |
| `reth_payloads_cached_reads_misses` | Execution cache misses (always-on) |
| `reth_txpool_pre_warming_simulations_completed` | Pre-warming simulations run |
| `reth_txpool_pre_warming_prefetch_operations` | Prefetch operations executed |
| `reth_txpool_pre_warming_prefetch_accounts` | Accounts prefetched from MDBX |
| `reth_block_timing_build_exec_mempool_transactions_*` | Block execution timing |
| `reth_block_timing_build_calc_state_root_*` | State root calculation timing |

### B. Build Command

```bash
cd /Users/lakshmikanth/Documents/optimisation/reth
cargo build --release --package op-reth --features pre-warming
```

### C. Results Directory Structure

```
.devnet-sim-v2-YYYYMMDD_HHMMSS/
├── results_off.json      # Phase 1 metrics (pre-warming OFF)
├── results_on.json       # Phase 2 metrics (pre-warming ON)
├── summary.txt           # Human-readable summary
├── node_off.log          # Node logs (OFF phase)
├── node_on.log           # Node logs (ON phase)
├── sender_0.out          # Sender 0 output
├── sender_1.out          # Sender 1 output
├── ...
└── sender_9.out          # Sender 9 output
```

### D. Test Environment

| Component | Version/Details |
|-----------|-----------------|
| OS | macOS |
| CPUs | 12 |
| op-reth | Custom build with pre-warming feature |
| Rust | Release build with optimizations |
| Python | 3.x (for transaction sending) |

