#!/bin/bash
#
# Capture Per-Transaction Timing Metrics for Pre-warming
#
# This script captures detailed timing metrics from Prometheus:
# - Simulation time (per transaction)
# - Prefetch time (per block)
# - Block execution time
#
# Usage:
#   ./capture_timing_metrics.sh [metrics_host] [metrics_port]
#
# Example:
#   ./capture_timing_metrics.sh localhost 9001
#

set -e

METRICS_HOST="${1:-localhost}"
METRICS_PORT="${2:-9001}"
METRICS_URL="http://${METRICS_HOST}:${METRICS_PORT}/metrics"

echo "╔══════════════════════════════════════════════════════════════════════════════╗"
echo "║      PRE-WARMING TIMING METRICS                                              ║"
echo "╚══════════════════════════════════════════════════════════════════════════════╝"
echo ""
echo "  Metrics URL: $METRICS_URL"
echo "  Timestamp:   $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo ""

# Fetch all metrics once
METRICS=$(curl -s "$METRICS_URL" 2>/dev/null)

if [ -z "$METRICS" ]; then
    echo "❌ Failed to fetch metrics from $METRICS_URL"
    exit 1
fi

# ═══════════════════════════════════════════════════════════════════════════════
# SIMULATION TIMING (Per Transaction)
# ═══════════════════════════════════════════════════════════════════════════════

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  SIMULATION TIMING (Per Transaction)                                        │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"

# Extract simulation_duration histogram
SIM_COUNT=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulation_duration_count" | awk '{print $2}')
SIM_SUM=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulation_duration_sum" | awk '{print $2}')

if [ -n "$SIM_COUNT" ] && [ -n "$SIM_SUM" ] && [ "$SIM_COUNT" != "0" ]; then
    SIM_AVG=$(echo "scale=6; $SIM_SUM / $SIM_COUNT * 1000" | bc)
    echo "│  Simulations Completed:  $(printf "%'d" ${SIM_COUNT%.*})"
    echo "│  Total Simulation Time:  ${SIM_SUM}s"
    echo "│  Avg Simulation Time:    ${SIM_AVG}ms per transaction"
else
    echo "│  Simulations Completed:  ${SIM_COUNT:-0}"
    echo "│  Avg Simulation Time:    N/A (no simulations yet)"
fi

# Percentiles
SIM_P50=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_simulation_duration{quantile="0.5"}' | awk '{print $2}')
SIM_P90=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_simulation_duration{quantile="0.9"}' | awk '{print $2}')
SIM_P99=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_simulation_duration{quantile="0.99"}' | awk '{print $2}')

if [ -n "$SIM_P50" ]; then
    SIM_P50_MS=$(echo "scale=3; $SIM_P50 * 1000" | bc)
    SIM_P90_MS=$(echo "scale=3; $SIM_P90 * 1000" | bc)
    SIM_P99_MS=$(echo "scale=3; $SIM_P99 * 1000" | bc)
    echo "│  P50 Simulation Time:    ${SIM_P50_MS}ms"
    echo "│  P90 Simulation Time:    ${SIM_P90_MS}ms"
    echo "│  P99 Simulation Time:    ${SIM_P99_MS}ms"
fi

echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

# ═══════════════════════════════════════════════════════════════════════════════
# PREFETCH TIMING (Per Block)
# ═══════════════════════════════════════════════════════════════════════════════

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  PREFETCH TIMING (Per Block)                                                │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"

PREFETCH_COUNT=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_duration_count" | awk '{print $2}')
PREFETCH_SUM=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_duration_sum" | awk '{print $2}')
PREFETCH_OPS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_operations" | awk '{print $2}')

if [ -n "$PREFETCH_COUNT" ] && [ -n "$PREFETCH_SUM" ] && [ "$PREFETCH_COUNT" != "0" ]; then
    PREFETCH_AVG=$(echo "scale=6; $PREFETCH_SUM / $PREFETCH_COUNT * 1000" | bc)
    echo "│  Prefetch Operations:    $(printf "%'d" ${PREFETCH_OPS%.*})"
    echo "│  Total Prefetch Time:    ${PREFETCH_SUM}s"
    echo "│  Avg Prefetch Time:      ${PREFETCH_AVG}ms per block"
else
    echo "│  Prefetch Operations:    ${PREFETCH_OPS:-0}"
    echo "│  Avg Prefetch Time:      N/A (no prefetch yet)"
fi

# Percentiles
PREFETCH_P50=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_prefetch_duration{quantile="0.5"}' | awk '{print $2}')
PREFETCH_P90=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_prefetch_duration{quantile="0.9"}' | awk '{print $2}')
PREFETCH_P99=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_prefetch_duration{quantile="0.99"}' | awk '{print $2}')

if [ -n "$PREFETCH_P50" ]; then
    PREFETCH_P50_MS=$(echo "scale=3; $PREFETCH_P50 * 1000" | bc)
    PREFETCH_P90_MS=$(echo "scale=3; $PREFETCH_P90 * 1000" | bc)
    PREFETCH_P99_MS=$(echo "scale=3; $PREFETCH_P99 * 1000" | bc)
    echo "│  P50 Prefetch Time:      ${PREFETCH_P50_MS}ms"
    echo "│  P90 Prefetch Time:      ${PREFETCH_P90_MS}ms"
    echo "│  P99 Prefetch Time:      ${PREFETCH_P99_MS}ms"
fi

# Prefetch data volumes
PREFETCH_ACCOUNTS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_accounts" | grep -v "#" | awk '{print $2}')
PREFETCH_STORAGE=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_storage_slots" | grep -v "#" | awk '{print $2}')
PREFETCH_CONTRACTS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_contracts" | grep -v "#" | awk '{print $2}')

echo "│"
echo "│  Data Prefetched:"
echo "│    Accounts:             $(printf "%'d" ${PREFETCH_ACCOUNTS%.*})"
echo "│    Storage Slots:        $(printf "%'d" ${PREFETCH_STORAGE%.*})"
echo "│    Contracts:            $(printf "%'d" ${PREFETCH_CONTRACTS%.*})"

echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

# ═══════════════════════════════════════════════════════════════════════════════
# BLOCK EXECUTION TIMING
# ═══════════════════════════════════════════════════════════════════════════════

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  BLOCK EXECUTION TIMING                                                     │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"

# Block execution time
BLOCK_EXEC_COUNT=$(echo "$METRICS" | grep "^reth_block_timing_build_exec_mempool_transactions_count" | awk '{print $2}')
BLOCK_EXEC_SUM=$(echo "$METRICS" | grep "^reth_block_timing_build_exec_mempool_transactions_sum" | awk '{print $2}')

if [ -n "$BLOCK_EXEC_COUNT" ] && [ -n "$BLOCK_EXEC_SUM" ] && [ "$BLOCK_EXEC_COUNT" != "0" ]; then
    BLOCK_EXEC_AVG=$(echo "scale=6; $BLOCK_EXEC_SUM / $BLOCK_EXEC_COUNT * 1000" | bc)
    echo "│  Blocks Executed:        $(printf "%'d" ${BLOCK_EXEC_COUNT%.*})"
    echo "│  Total Execution Time:   ${BLOCK_EXEC_SUM}s"
    echo "│  Avg Block Execution:    ${BLOCK_EXEC_AVG}ms per block"

    # Calculate per-TX execution time (estimated)
    if [ -n "$SIM_COUNT" ] && [ "$SIM_COUNT" != "0" ]; then
        TXS_PER_BLOCK=$(echo "scale=2; $SIM_COUNT / $BLOCK_EXEC_COUNT" | bc)
        TX_EXEC_AVG=$(echo "scale=6; $BLOCK_EXEC_AVG / $TXS_PER_BLOCK" | bc)
        echo "│"
        echo "│  Estimated Per-TX Metrics:"
        echo "│    Avg TXs per Block:    ${TXS_PER_BLOCK}"
        echo "│    Avg TX Execution:     ${TX_EXEC_AVG}ms per transaction"
    fi
else
    echo "│  Blocks Executed:        ${BLOCK_EXEC_COUNT:-0}"
    echo "│  Avg Block Execution:    N/A"
fi

# State root time
STATE_ROOT_COUNT=$(echo "$METRICS" | grep "^reth_block_timing_state_root_count" | awk '{print $2}')
STATE_ROOT_SUM=$(echo "$METRICS" | grep "^reth_block_timing_state_root_sum" | awk '{print $2}')

if [ -n "$STATE_ROOT_COUNT" ] && [ -n "$STATE_ROOT_SUM" ] && [ "$STATE_ROOT_COUNT" != "0" ]; then
    STATE_ROOT_AVG=$(echo "scale=6; $STATE_ROOT_SUM / $STATE_ROOT_COUNT * 1000" | bc)
    echo "│"
    echo "│  State Root Calculations: $(printf "%'d" ${STATE_ROOT_COUNT%.*})"
    echo "│  Total State Root Time:   ${STATE_ROOT_SUM}s"
    echo "│  Avg State Root Time:     ${STATE_ROOT_AVG}ms per block"
fi

echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

# ═══════════════════════════════════════════════════════════════════════════════
# CACHE METRICS
# ═══════════════════════════════════════════════════════════════════════════════

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  CACHE METRICS                                                              │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"

CACHE_HITS=$(echo "$METRICS" | grep "^reth_payloads_cached_reads_hits" | grep -v "#" | awk '{print $2}')
CACHE_MISSES=$(echo "$METRICS" | grep "^reth_payloads_cached_reads_misses" | grep -v "#" | awk '{print $2}')

CACHE_HITS=${CACHE_HITS:-0}
CACHE_MISSES=${CACHE_MISSES:-0}

TOTAL_ACCESS=$(echo "$CACHE_HITS + $CACHE_MISSES" | bc)
if [ "$TOTAL_ACCESS" != "0" ]; then
    HIT_RATE=$(echo "scale=2; $CACHE_HITS * 100 / $TOTAL_ACCESS" | bc)
else
    HIT_RATE="N/A"
fi

echo "│  Cache Hits:             $(printf "%'d" ${CACHE_HITS%.*})"
echo "│  Cache Misses:           $(printf "%'d" ${CACHE_MISSES%.*})"
echo "│  Cache Hit Rate:         ${HIT_RATE}%"

echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

# ═══════════════════════════════════════════════════════════════════════════════
# SUMMARY
# ═══════════════════════════════════════════════════════════════════════════════

echo "╔══════════════════════════════════════════════════════════════════════════════╗"
echo "║  SUMMARY                                                                     ║"
echo "╠══════════════════════════════════════════════════════════════════════════════╣"

if [ -n "$SIM_AVG" ]; then
    printf "║  %-30s %10s ms  (per TX, background)    ║\n" "Avg Simulation Time:" "$SIM_AVG"
fi
if [ -n "$TX_EXEC_AVG" ]; then
    printf "║  %-30s %10s ms  (per TX, during block)  ║\n" "Avg TX Execution:" "$TX_EXEC_AVG"
fi
if [ -n "$PREFETCH_AVG" ]; then
    printf "║  %-30s %10s ms  (per block)             ║\n" "Avg Prefetch Time:" "$PREFETCH_AVG"
fi
if [ -n "$BLOCK_EXEC_AVG" ]; then
    printf "║  %-30s %10s ms  (per block)             ║\n" "Avg Block Execution:" "$BLOCK_EXEC_AVG"
fi
if [ -n "$STATE_ROOT_AVG" ]; then
    printf "║  %-30s %10s ms  (per block)             ║\n" "Avg State Root:" "$STATE_ROOT_AVG"
fi
printf "║  %-30s %10s %%                           ║\n" "Cache Hit Rate:" "$HIT_RATE"

echo "╚══════════════════════════════════════════════════════════════════════════════╝"

