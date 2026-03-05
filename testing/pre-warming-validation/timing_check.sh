#!/bin/bash
#===============================================================================
#  TIMING CHECK - Quick view of pre-warming timing metrics
#===============================================================================

METRICS_URL="${1:-http://localhost:9001/metrics}"

METRICS=$(curl -s "$METRICS_URL" 2>/dev/null)

if [ -z "$METRICS" ]; then
    echo "❌ Failed to fetch metrics from $METRICS_URL"
    echo "   Make sure node is running with --metrics 0.0.0.0:9001"
    exit 1
fi

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║         PRE-WARMING TIMING METRICS                           ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Simulation duration
SIM_SUM=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulation_duration_sum " | awk '{print $2}')
SIM_COUNT=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulation_duration_count " | awk '{print $2}')
SIM_P50=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_simulation_duration{quantile="0.5"}' | awk '{print $2}')
SIM_P99=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_simulation_duration{quantile="0.99"}' | awk '{print $2}')

echo "🔄 SIMULATION PHASE"
echo "   ├─ Count:  ${SIM_COUNT:-0}"
if [ -n "$SIM_COUNT" ] && [ "$SIM_COUNT" != "0" ] && [ "$SIM_COUNT" != "0.0" ]; then
    SIM_AVG=$(echo "scale=3; $SIM_SUM / $SIM_COUNT * 1000" | bc 2>/dev/null || echo "N/A")
    SIM_P50_MS=$(echo "scale=3; ${SIM_P50:-0} * 1000" | bc 2>/dev/null || echo "N/A")
    SIM_P99_MS=$(echo "scale=3; ${SIM_P99:-0} * 1000" | bc 2>/dev/null || echo "N/A")
    echo "   ├─ Avg:    ${SIM_AVG} ms"
    echo "   ├─ P50:    ${SIM_P50_MS} ms"
    echo "   └─ P99:    ${SIM_P99_MS} ms"
else
    echo "   └─ (No simulations recorded yet)"
fi

echo ""

# Prefetch duration
PRE_SUM=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_duration_sum " | awk '{print $2}')
PRE_COUNT=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_duration_count " | awk '{print $2}')
PRE_P50=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_prefetch_duration{quantile="0.5"}' | awk '{print $2}')
PRE_P99=$(echo "$METRICS" | grep 'reth_txpool_pre_warming_prefetch_duration{quantile="0.99"}' | awk '{print $2}')

echo "📥 PREFETCH PHASE (MDBX Fetch)"
echo "   ├─ Count:  ${PRE_COUNT:-0}"
if [ -n "$PRE_COUNT" ] && [ "$PRE_COUNT" != "0" ] && [ "$PRE_COUNT" != "0.0" ]; then
    PRE_AVG=$(echo "scale=3; $PRE_SUM / $PRE_COUNT * 1000" | bc 2>/dev/null || echo "N/A")
    PRE_P50_MS=$(echo "scale=3; ${PRE_P50:-0} * 1000" | bc 2>/dev/null || echo "N/A")
    PRE_P99_MS=$(echo "scale=3; ${PRE_P99:-0} * 1000" | bc 2>/dev/null || echo "N/A")
    echo "   ├─ Avg:    ${PRE_AVG} ms"
    echo "   ├─ P50:    ${PRE_P50_MS} ms"
    echo "   └─ P99:    ${PRE_P99_MS} ms"
else
    echo "   └─ (No prefetch operations recorded yet)"
fi

echo ""

# Counters
SIMS_COMPLETED=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_completed " | awk '{print $2}')
SIMS_FAILED=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_simulations_failed " | awk '{print $2}')
PREFETCH_OPS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_operations " | awk '{print $2}')
PREFETCH_ACCOUNTS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_accounts " | awk '{print $2}')
PREFETCH_STORAGE=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_prefetch_storage_slots " | awk '{print $2}')

echo "📊 OPERATION COUNTS"
echo "   ├─ Simulations Completed: ${SIMS_COMPLETED:-0}"
echo "   ├─ Simulations Failed:    ${SIMS_FAILED:-0}"
echo "   ├─ Prefetch Operations:   ${PREFETCH_OPS:-0}"
echo "   ├─ Accounts Prefetched:   ${PREFETCH_ACCOUNTS:-0}"
echo "   └─ Storage Slots Fetched: ${PREFETCH_STORAGE:-0}"

echo ""

# Cache effectiveness
CACHE_HITS=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}')
CACHE_MISSES=$(echo "$METRICS" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}')
HITS=${CACHE_HITS%.*}
MISSES=${CACHE_MISSES%.*}
HITS=${HITS:-0}
MISSES=${MISSES:-0}
TOTAL=$((HITS + MISSES))

echo "💾 CACHE EFFECTIVENESS"
echo "   ├─ Hits:    $HITS"
echo "   ├─ Misses:  $MISSES"
if [ "$TOTAL" -gt 0 ]; then
    HIT_RATE=$(echo "scale=1; $HITS * 100 / $TOTAL" | bc)
    echo "   └─ Hit Rate: ${HIT_RATE}%"
else
    echo "   └─ Hit Rate: N/A (no accesses)"
fi

echo ""

# EVM execution cache
EVM_ACCOUNT_HITS=$(echo "$METRICS" | grep "^reth_sync_caching_account_cache_hits " | awk '{print $2}')
EVM_STORAGE_HITS=$(echo "$METRICS" | grep "^reth_sync_caching_storage_cache_hits " | awk '{print $2}')
EVM_ACCOUNT_MISSES=$(echo "$METRICS" | grep "^reth_sync_caching_account_cache_misses " | awk '{print $2}')
EVM_STORAGE_MISSES=$(echo "$METRICS" | grep "^reth_sync_caching_storage_cache_misses " | awk '{print $2}')

EVM_HITS=$((${EVM_ACCOUNT_HITS%.*:-0} + ${EVM_STORAGE_HITS%.*:-0}))
EVM_MISSES=$((${EVM_ACCOUNT_MISSES%.*:-0} + ${EVM_STORAGE_MISSES%.*:-0}))
EVM_TOTAL=$((EVM_HITS + EVM_MISSES))

echo "⚡ EVM EXECUTION CACHE"
echo "   ├─ Account Hits:   ${EVM_ACCOUNT_HITS:-0}"
echo "   ├─ Storage Hits:   ${EVM_STORAGE_HITS:-0}"
echo "   ├─ Account Misses: ${EVM_ACCOUNT_MISSES:-0}"
echo "   ├─ Storage Misses: ${EVM_STORAGE_MISSES:-0}"
if [ "$EVM_TOTAL" -gt 0 ]; then
    EVM_HIT_RATE=$(echo "scale=1; $EVM_HITS * 100 / $EVM_TOTAL" | bc)
    echo "   └─ EVM Hit Rate: ${EVM_HIT_RATE}%"
else
    echo "   └─ EVM Hit Rate: N/A (no accesses)"
fi

echo ""
echo "══════════════════════════════════════════════════════════════"

