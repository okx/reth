# Devnet Pre-Warming Monitoring Guide

This guide provides instructions for extracting and monitoring pre-warming metrics from a running op-reth node on devnet.

## Prerequisites

- op-reth node running with `--metrics 0.0.0.0:9001`
- Pre-warming enabled: `--txpool.pre-warming true`
- curl installed
- python3 installed (for statistical calculations)

---

## Automated Continuous Monitoring (Recommended)

Use the `monitor_devnet.sh` script for continuous monitoring with automatic report generation:

### Quick Start

```bash
# Default: Monitor localhost:9001, sample every 30s, report every 5 mins
./monitor_devnet.sh

# Monitor devnet with custom settings
./monitor_devnet.sh --url http://devnet-ip:9001/metrics --interval 60 --report-interval 3600

# Run for 2 hours then generate final report
./monitor_devnet.sh --duration 7200

# Show all options
./monitor_devnet.sh --help
```

### What It Does

1. **Samples metrics** at regular intervals (default: 30 seconds)
2. **Stores raw data** in CSV format for analysis
3. **Generates periodic reports** with calculated statistics
4. **Shows live dashboard** with real-time metrics
5. **Calculates meaningful rates**:
   - Cache Hit Rate (%)
   - Simulation Success Rate (%)
   - Prefetch Rate (ops/sec)
   - Block Processing Rate

### Output Files

```
devnet_monitoring_YYYYMMDD_HHMMSS/
├── raw_metrics.csv       # All sampled data
├── latest_report.txt     # Most recent report
└── report_history.log    # Summary of all reports
```

### Sample Report Output

```
================================================================================
                    PRE-WARMING PERFORMANCE REPORT
================================================================================
Generated: 2026-03-04 10:30:00
Period: 60.0 minutes (120 samples)
--------------------------------------------------------------------------------

📊 THROUGHPUT
--------------------------------------------------------------------------------
  Blocks Processed:     3600
  Block Rate:           1.00/sec

🔄 SIMULATIONS
--------------------------------------------------------------------------------
  Completed:            15420
  Failed:               23
  Success Rate:         99.8%
  Simulation Rate:      4.28/sec

💾 PRE-WARMING CACHE
--------------------------------------------------------------------------------
  Cache Hits:           45230
  Cache Misses:         12450
  Hit Rate:             78.4%

📥 PREFETCH OPERATIONS
--------------------------------------------------------------------------------
  Total Operations:     15420
  Accounts Prefetched:  62580
  Prefetch Rate:        4.28/sec

⚡ EVM EXECUTION CACHE (Actual Performance)
--------------------------------------------------------------------------------
  EVM Cache Hits:       89340
  EVM Cache Misses:     23120
  EVM Hit Rate:         79.4%

📈 HEALTH INDICATORS
--------------------------------------------------------------------------------
  Simulation Health:    ✅ GOOD (99.8% success)
  Cache Efficiency:     ✅ GOOD (78.4% hit rate)
  EVM Cache Efficiency: ✅ GOOD (79.4% hit rate)
================================================================================
```

---

## Quick Reference: Metrics Endpoint

```bash
# Base URL for metrics
METRICS_URL="http://localhost:9001/metrics"

# Or use your devnet IP
METRICS_URL="http://<DEVNET_IP>:9001/metrics"
```

---

## Key Metrics Overview

| Metric Name | Type | Description |
|-------------|------|-------------|
| `reth_txpool_pre_warming_simulations_completed` | Counter | Successful pre-warming simulations |
| `reth_txpool_pre_warming_simulations_failed` | Counter | Failed simulations |
| `reth_txpool_pre_warming_simulations_triggered` | Counter | Total simulations triggered |
| `reth_txpool_pre_warming_simulations_dropped` | Counter | Simulations dropped (queue full) |
| `reth_txpool_pre_warming_cache_hits` | Counter | Pre-warming cache hits |
| `reth_txpool_pre_warming_cache_misses` | Counter | Pre-warming cache misses |
| `reth_txpool_pre_warming_cache_entries` | Gauge | Current entries in cache |
| `reth_txpool_pre_warming_cache_keys_total` | Counter | Total keys in cache |
| `reth_txpool_pre_warming_cache_evictions` | Counter | Cache evictions |
| `reth_txpool_pre_warming_prefetch_operations` | Counter | Prefetch operations executed |
| `reth_txpool_pre_warming_prefetch_accounts` | Counter | Accounts prefetched from MDBX |
| `reth_txpool_pre_warming_prefetch_storage_slots` | Counter | Storage slots prefetched |
| `reth_txpool_pre_warming_prefetch_contracts` | Counter | Contract bytecode prefetched |
| `reth_sync_caching_account_cache_hits` | Counter | EVM execution account cache hits |
| `reth_sync_caching_storage_cache_hits` | Counter | EVM execution storage cache hits |
| `reth_sync_caching_account_cache_misses` | Counter | EVM execution account cache misses |
| `reth_sync_caching_storage_cache_misses` | Counter | EVM execution storage cache misses |

---

## Extraction Commands

### 1. Get All Pre-Warming Metrics

```bash
curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming" | grep -v "^#"
```

### 2. Get Sync Caching Metrics (EVM Execution Cache)

```bash
curl -s http://localhost:9001/metrics | grep "reth_sync_caching" | grep -v "^#"
```

### 3. Get Combined Key Metrics (One-liner)

```bash
curl -s http://localhost:9001/metrics | grep -E "reth_txpool_pre_warming|reth_sync_caching" | grep -v "^#" | grep -v "histogram" | grep -v "quantile"
```

---

## Individual Metric Extraction

### Cache Hits

```bash
# Pre-warming cache hits
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}'

# EVM account cache hits (actual execution cache)
curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_account_cache_hits " | awk '{print $2}'

# EVM storage cache hits
curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_storage_cache_hits " | awk '{print $2}'
```

### Cache Misses

```bash
# Pre-warming cache misses
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}'

# EVM account cache misses
curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_account_cache_misses " | awk '{print $2}'

# EVM storage cache misses
curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_storage_cache_misses " | awk '{print $2}'
```

### Simulations

```bash
# Completed simulations
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_simulations_completed " | awk '{print $2}'

# Failed simulations
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_simulations_failed " | awk '{print $2}'

# Triggered simulations
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_simulations_triggered " | awk '{print $2}'

# Dropped simulations
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_simulations_dropped " | awk '{print $2}'
```

### Prefetch Operations

```bash
# Total prefetch operations
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_prefetch_operations " | awk '{print $2}'

# Accounts prefetched
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_prefetch_accounts " | awk '{print $2}'

# Storage slots prefetched
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_prefetch_storage_slots " | awk '{print $2}'

# Contracts prefetched
curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_prefetch_contracts " | awk '{print $2}'
```

---

## Calculated Metrics

### Cache Hit Rate (%)

```bash
# Using pre-warming metrics
HITS=$(curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}')
MISSES=$(curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}')
TOTAL=$((${HITS%.*} + ${MISSES%.*}))
if [ "$TOTAL" -gt 0 ]; then
    HIT_RATE=$(echo "scale=2; ${HITS%.*} * 100 / $TOTAL" | bc)
    echo "Cache Hit Rate: ${HIT_RATE}%"
else
    echo "Cache Hit Rate: N/A (no accesses)"
fi
```

### EVM Execution Cache Hit Rate (More Accurate)

```bash
# Account + Storage combined
ACCOUNT_HITS=$(curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_account_cache_hits " | awk '{print $2}')
STORAGE_HITS=$(curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_storage_cache_hits " | awk '{print $2}')
ACCOUNT_MISSES=$(curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_account_cache_misses " | awk '{print $2}')
STORAGE_MISSES=$(curl -s http://localhost:9001/metrics | grep "^reth_sync_caching_storage_cache_misses " | awk '{print $2}')

TOTAL_HITS=$((${ACCOUNT_HITS%.*} + ${STORAGE_HITS%.*}))
TOTAL_MISSES=$((${ACCOUNT_MISSES%.*} + ${STORAGE_MISSES%.*}))
TOTAL=$((TOTAL_HITS + TOTAL_MISSES))

if [ "$TOTAL" -gt 0 ]; then
    HIT_RATE=$(echo "scale=2; $TOTAL_HITS * 100 / $TOTAL" | bc)
    echo "EVM Cache Hit Rate: ${HIT_RATE}%"
else
    echo "EVM Cache Hit Rate: N/A"
fi
```

### Simulation Success Rate (%)

```bash
COMPLETED=$(curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_simulations_completed " | awk '{print $2}')
FAILED=$(curl -s http://localhost:9001/metrics | grep "^reth_txpool_pre_warming_simulations_failed " | awk '{print $2}')
TOTAL=$((${COMPLETED%.*} + ${FAILED%.*}))
if [ "$TOTAL" -gt 0 ]; then
    SUCCESS_RATE=$(echo "scale=2; ${COMPLETED%.*} * 100 / $TOTAL" | bc)
    echo "Simulation Success Rate: ${SUCCESS_RATE}%"
else
    echo "Simulation Success Rate: N/A"
fi
```

---

## TPS Calculation

TPS must be calculated from transaction counts over time:

```bash
# Get block number and transaction count at time T1
BLOCK_T1=$(curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | grep -o '"result":"[^"]*"' | cut -d'"' -f4)

# Wait 60 seconds
sleep 60

# Get block number at time T2
BLOCK_T2=$(curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | grep -o '"result":"[^"]*"' | cut -d'"' -f4)

# Calculate blocks processed
BLOCKS=$((16#${BLOCK_T2#0x} - 16#${BLOCK_T1#0x}))
echo "Blocks in 60s: $BLOCKS"
```

For precise TPS, count transactions in each block or use the pending transaction pool metrics.

---

## Continuous Monitoring Script Template

```bash
#!/bin/bash
# Save as: monitor_prewarming.sh

METRICS_URL="${1:-http://localhost:9001/metrics}"
INTERVAL="${2:-30}"
OUTPUT_FILE="prewarming_metrics_$(date +%Y%m%d_%H%M%S).csv"

# CSV Header
echo "timestamp,sim_completed,sim_failed,cache_hits,cache_misses,hit_rate,prefetch_ops,prefetch_accounts" > "$OUTPUT_FILE"

while true; do
    METRICS=$(curl -s "$METRICS_URL")
    TIMESTAMP=$(date +%s)
    
    SIM_COMPLETED=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_completed " | awk '{print $2}')
    SIM_FAILED=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_failed " | awk '{print $2}')
    CACHE_HITS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}')
    CACHE_MISSES=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}')
    PREFETCH_OPS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_operations " | awk '{print $2}')
    PREFETCH_ACCOUNTS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_accounts " | awk '{print $2}')
    
    # Calculate hit rate
    TOTAL=$((${CACHE_HITS%.*} + ${CACHE_MISSES%.*}))
    if [ "$TOTAL" -gt 0 ]; then
        HIT_RATE=$(echo "scale=2; ${CACHE_HITS%.*} * 100 / $TOTAL" | bc)
    else
        HIT_RATE="0"
    fi
    
    # Write to CSV
    echo "$TIMESTAMP,$SIM_COMPLETED,$SIM_FAILED,$CACHE_HITS,$CACHE_MISSES,$HIT_RATE,$PREFETCH_OPS,$PREFETCH_ACCOUNTS" >> "$OUTPUT_FILE"
    
    # Print to console
    echo "[$(date '+%H:%M:%S')] Sims: $SIM_COMPLETED | Failed: $SIM_FAILED | Hits: $CACHE_HITS | Misses: $CACHE_MISSES | HitRate: ${HIT_RATE}%"
    
    sleep "$INTERVAL"
done
```

Usage:
```bash
chmod +x monitor_prewarming.sh
./monitor_prewarming.sh http://devnet-ip:9001/metrics 30
```

---

## Prometheus Queries (For Grafana Dashboards)

### Cache Hit Rate Over Time

```promql
rate(reth_txpool_pre_warming_cache_hits[5m]) / 
(rate(reth_txpool_pre_warming_cache_hits[5m]) + rate(reth_txpool_pre_warming_cache_misses[5m])) * 100
```

### EVM Cache Hit Rate

```promql
(rate(reth_sync_caching_account_cache_hits[5m]) + rate(reth_sync_caching_storage_cache_hits[5m])) /
(rate(reth_sync_caching_account_cache_hits[5m]) + rate(reth_sync_caching_storage_cache_hits[5m]) + 
 rate(reth_sync_caching_account_cache_misses[5m]) + rate(reth_sync_caching_storage_cache_misses[5m])) * 100
```

### Simulation Throughput

```promql
rate(reth_txpool_pre_warming_simulations_completed[5m])
```

### Simulation Failure Rate

```promql
rate(reth_txpool_pre_warming_simulations_failed[5m]) / 
rate(reth_txpool_pre_warming_simulations_triggered[5m]) * 100
```

### Prefetch Operations Rate

```promql
rate(reth_txpool_pre_warming_prefetch_operations[5m])
```

---

## Health Check Summary Command

One command to get all key metrics:

```bash
#!/bin/bash
METRICS=$(curl -s http://localhost:9001/metrics)

echo "=========================================="
echo "  PRE-WARMING HEALTH CHECK"
echo "=========================================="
echo ""
echo "SIMULATIONS:"
echo "  Triggered:  $(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_triggered " | awk '{print $2}')"
echo "  Completed:  $(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_completed " | awk '{print $2}')"
echo "  Failed:     $(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_failed " | awk '{print $2}')"
echo "  Dropped:    $(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_dropped " | awk '{print $2}')"
echo ""
echo "CACHE:"
echo "  Entries:    $(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_entries " | awk '{print $2}')"
echo "  Keys:       $(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_keys_total " | awk '{print $2}')"
echo "  Hits:       $(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}')"
echo "  Misses:     $(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}')"
echo "  Evictions:  $(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_evictions " | awk '{print $2}')"
echo ""
echo "PREFETCH:"
echo "  Operations: $(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_operations " | awk '{print $2}')"
echo "  Accounts:   $(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_accounts " | awk '{print $2}')"
echo "  Storage:    $(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_storage_slots " | awk '{print $2}')"
echo "  Contracts:  $(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_contracts " | awk '{print $2}')"
echo ""
echo "EVM EXECUTION CACHE:"
echo "  Account Hits:   $(echo "$METRICS" | grep "^reth_sync_caching_account_cache_hits " | awk '{print $2}')"
echo "  Account Misses: $(echo "$METRICS" | grep "^reth_sync_caching_account_cache_misses " | awk '{print $2}')"
echo "  Storage Hits:   $(echo "$METRICS" | grep "^reth_sync_caching_storage_cache_hits " | awk '{print $2}')"
echo "  Storage Misses: $(echo "$METRICS" | grep "^reth_sync_caching_storage_cache_misses " | awk '{print $2}')"
echo "=========================================="
```

---

## Alerting Thresholds (Suggested)

| Metric | Warning | Critical |
|--------|---------|----------|
| Cache Hit Rate | < 50% | < 20% |
| Simulation Failure Rate | > 10% | > 25% |
| Prefetch Operations (per min) | < 10 | 0 |
| Cache Evictions (per min) | > 100 | > 500 |

---

## Notes

1. **Metrics Port**: Default is 9001, configurable via `--metrics 0.0.0.0:<PORT>`
2. **Log Patterns**: For log-based monitoring, use `--log.stdout.filter debug` to enable detailed pre-warming logs
3. **TPS Calculation**: Requires transaction counting over time intervals, not a direct metric
4. **EVM Cache vs Pre-warming Cache**: 
   - `reth_sync_caching_*` = Actual EVM execution cache (more accurate for performance)
   - `reth_txpool_pre_warming_*` = Pre-warming system metrics

