#!/bin/bash
# Get specific pre-warming metrics from DevNet with cache hit/miss percentage

DEVNET_IP="${1:-localhost}"
METRICS_PORT="${2:-9001}"

METRICS=$(curl -s http://${DEVNET_IP}:${METRICS_PORT}/metrics)

if [ -z "$METRICS" ]; then
    echo "ERROR: No response from http://${DEVNET_IP}:${METRICS_PORT}/metrics"
    echo ""
    echo "Usage: $0 <IP> <PORT>"
    echo "Example: $0 192.168.1.100 9001"
    exit 1
fi

# Extract key values
ACCOUNT_HITS=$(echo "$METRICS" | grep "reth_sync_caching_account_cache_hits" | grep -v "^#" | awk '{print $2}' | head -1)
STORAGE_HITS=$(echo "$METRICS" | grep "reth_sync_caching_storage_cache_hits" | grep -v "^#" | awk '{print $2}' | head -1)
CACHE_MISSES=$(echo "$METRICS" | grep "reth_txpool_pre_warming_cache_misses" | grep -v "^#" | awk '{print $2}' | head -1)
SIMULATIONS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_simulations_completed" | grep -v "^#" | awk '{print $2}' | head -1)
CACHE_ENTRIES=$(echo "$METRICS" | grep "reth_txpool_pre_warming_cache_entries" | grep -v "^#" | awk '{print $2}' | head -1)
CACHE_KEYS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_cache_keys_total" | grep -v "^#" | awk '{print $2}' | head -1)
PREFETCH_ACCOUNTS=$(echo "$METRICS" | grep "reth_txpool_pre_warming_prefetch_accounts" | grep -v "^#" | awk '{print $2}' | head -1)
PREFETCH_STORAGE=$(echo "$METRICS" | grep "reth_txpool_pre_warming_prefetch_storage_slots" | grep -v "^#" | awk '{print $2}' | head -1)

# Default to 0 if empty
ACCOUNT_HITS="${ACCOUNT_HITS:-0}"
STORAGE_HITS="${STORAGE_HITS:-0}"
CACHE_MISSES="${CACHE_MISSES:-0}"
SIMULATIONS="${SIMULATIONS:-0}"
CACHE_ENTRIES="${CACHE_ENTRIES:-0}"
CACHE_KEYS="${CACHE_KEYS:-0}"
PREFETCH_ACCOUNTS="${PREFETCH_ACCOUNTS:-0}"
PREFETCH_STORAGE="${PREFETCH_STORAGE:-0}"

# Calculate totals
TOTAL_HITS=$(echo "$ACCOUNT_HITS + $STORAGE_HITS" | bc 2>/dev/null || echo "0")
TOTAL_ACCESS=$(echo "$TOTAL_HITS + $CACHE_MISSES" | bc 2>/dev/null || echo "0")

# Calculate hit rate
if [ "$TOTAL_ACCESS" != "0" ] && [ "$TOTAL_ACCESS" != "" ]; then
    HIT_RATE=$(echo "scale=1; $TOTAL_HITS * 100 / $TOTAL_ACCESS" | bc 2>/dev/null || echo "0")
    MISS_RATE=$(echo "scale=1; $CACHE_MISSES * 100 / $TOTAL_ACCESS" | bc 2>/dev/null || echo "0")
else
    HIT_RATE="0"
    MISS_RATE="0"
fi

# Print report
echo "=============================================="
echo "  PRE-WARMING METRICS REPORT"
echo "  Host: $DEVNET_IP:$METRICS_PORT"
echo "  Time: $(date '+%Y-%m-%d %H:%M:%S')"
echo "=============================================="
echo ""
echo "SIMULATION"
echo "  Completed:        $SIMULATIONS"
echo "  Cache Entries:    $CACHE_ENTRIES"
echo "  Total Keys:       $CACHE_KEYS"
echo ""
echo "PREFETCH"
echo "  Accounts:         $PREFETCH_ACCOUNTS"
echo "  Storage Slots:    $PREFETCH_STORAGE"
echo ""
echo "CACHE PERFORMANCE"
echo "  Account Hits:     $ACCOUNT_HITS"
echo "  Storage Hits:     $STORAGE_HITS"
echo "  Total Hits:       $TOTAL_HITS"
echo "  Misses:           $CACHE_MISSES"
echo "  ------------------------------"
echo "  HIT RATE:         ${HIT_RATE}%"
echo "  MISS RATE:        ${MISS_RATE}%"
echo "=============================================="

