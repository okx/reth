#!/bin/bash

DEVNET_IP="${1:-localhost}"
METRICS_PORT="${2:-9001}"

echo "=============================================="
echo "  Pre-Warming & Cache Metrics Report"
echo "  Host: $DEVNET_IP:$METRICS_PORT"
echo "  Time: $(date '+%Y-%m-%d %H:%M:%S')"
echo "=============================================="
echo ""

# Fetch all metrics
METRICS=$(curl -s http://${DEVNET_IP}:${METRICS_PORT}/metrics)

if [ -z "$METRICS" ]; then
    echo "ERROR: Could not fetch metrics from http://${DEVNET_IP}:${METRICS_PORT}/metrics"
    exit 1
fi

# Debug: Show raw pre-warming metrics
echo "RAW PRE-WARMING METRICS:"
echo "----------------------------------------------"
echo "$METRICS" | grep "reth_txpool_pre_warming" | grep -v "^#" | head -15
echo ""

echo "RAW CACHE METRICS (sync caching):"
echo "----------------------------------------------"
echo "$METRICS" | grep "reth_sync_caching" | grep -v "^#" | grep -v "precompile" | head -10
echo ""

# Extract PRE-WARMING values
SIMULATIONS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_simulations_completed" | grep -v "^#" | awk '{print $2}' | head -1)
SIMULATIONS_FAILED=$(echo "$METRICS" | grep "reth_txpool_pre_warming_simulations_failed" | grep -v "^#" | awk '{print $2}' | head -1)
CACHE_ENTRIES=$(echo "$METRICS" | grep "reth_txpool_pre_warming_cache_entries" | grep -v "^#" | awk '{print $2}' | head -1)
CACHE_KEYS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_cache_keys_total" | grep -v "^#" | awk '{print $2}' | head -1)
PREFETCH_OPS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_prefetch_operations" | grep -v "^#" | awk '{print $2}' | head -1)
PREFETCH_ACCOUNTS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_prefetch_accounts" | grep -v "^#" | awk '{print $2}' | head -1)
PREFETCH_STORAGE=$(echo "$METRICS" | grep "reth_txpool_pre_warming_prefetch_storage_slots" | grep -v "^#" | awk '{print $2}' | head -1)
CACHE_MISSES=$(echo "$METRICS" | grep "reth_txpool_pre_warming_cache_misses" | grep -v "^#" | awk '{print $2}' | head -1)

# Extract SYNC CACHING values (actual cache hits during EVM execution)
ACCOUNT_CACHE_HITS=$(echo "$METRICS" | grep "reth_sync_caching_account_cache_hits" | grep -v "^#" | awk '{print $2}' | head -1)
STORAGE_CACHE_HITS=$(echo "$METRICS" | grep "reth_sync_caching_storage_cache_hits" | grep -v "^#" | awk '{print $2}' | head -1)
CODE_CACHE_HITS=$(echo "$METRICS" | grep "reth_sync_caching_code_cache_hits" | grep -v "^#" | awk '{print $2}' | head -1)

# Handle empty values
SIMULATIONS="${SIMULATIONS:-0}"
SIMULATIONS_FAILED="${SIMULATIONS_FAILED:-0}"
CACHE_ENTRIES="${CACHE_ENTRIES:-0}"
CACHE_KEYS="${CACHE_KEYS:-0}"
PREFETCH_OPS="${PREFETCH_OPS:-0}"
PREFETCH_ACCOUNTS="${PREFETCH_ACCOUNTS:-0}"
PREFETCH_STORAGE="${PREFETCH_STORAGE:-0}"
CACHE_MISSES="${CACHE_MISSES:-0}"
ACCOUNT_CACHE_HITS="${ACCOUNT_CACHE_HITS:-0}"
STORAGE_CACHE_HITS="${STORAGE_CACHE_HITS:-0}"
CODE_CACHE_HITS="${CODE_CACHE_HITS:-0}"

# Calculate total cache hits
TOTAL_CACHE_HITS=$(echo "$ACCOUNT_CACHE_HITS + $STORAGE_CACHE_HITS + $CODE_CACHE_HITS" | bc 2>/dev/null || echo "0")

# Print report
echo ""
echo "=============================================="
echo "  PRE-WARMING SIMULATION"
echo "=============================================="
printf "  Simulations Completed:  %s\n" "$SIMULATIONS"
printf "  Simulations Failed:     %s\n" "$SIMULATIONS_FAILED"
printf "  Cache Entries:          %s\n" "$CACHE_ENTRIES"
printf "  Total Keys Cached:      %s\n" "$CACHE_KEYS"
echo ""

echo "=============================================="
echo "  PREFETCH OPERATIONS"
echo "=============================================="
printf "  Prefetch Operations:    %s\n" "$PREFETCH_OPS"
printf "  Accounts Prefetched:    %s\n" "$PREFETCH_ACCOUNTS"
printf "  Storage Slots Prefetched: %s\n" "$PREFETCH_STORAGE"
echo ""

echo "=============================================="
echo "  CACHE HITS (EVM Execution)"
echo "=============================================="
printf "  Account Cache Hits:     %s\n" "$ACCOUNT_CACHE_HITS"
printf "  Storage Cache Hits:     %s\n" "$STORAGE_CACHE_HITS"
printf "  Code Cache Hits:        %s\n" "$CODE_CACHE_HITS"
printf "  -----------------------------------\n"
printf "  TOTAL CACHE HITS:       %s\n" "$TOTAL_CACHE_HITS"
echo ""

echo "=============================================="
echo "  SUMMARY"
echo "=============================================="
printf "  Keys Pre-warmed:        %s\n" "$CACHE_KEYS"
printf "  Total Cache Hits:       %s\n" "$TOTAL_CACHE_HITS"
printf "  Cache Misses:           %s\n" "$CACHE_MISSES"

# Calculate hit rate if we have data
if [ "$TOTAL_CACHE_HITS" != "0" ] || [ "$CACHE_MISSES" != "0" ]; then
    TOTAL_ACCESS=$(echo "$TOTAL_CACHE_HITS + $CACHE_MISSES" | bc 2>/dev/null || echo "0")
    if [ "$TOTAL_ACCESS" != "0" ]; then
        HIT_RATE=$(echo "scale=1; $TOTAL_CACHE_HITS * 100 / $TOTAL_ACCESS" | bc 2>/dev/null || echo "N/A")
        printf "  Hit Rate:               %s%%\n" "$HIT_RATE"
    fi
fi
echo ""
echo "=============================================="