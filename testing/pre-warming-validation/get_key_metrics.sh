#!/bin/bash
#===============================================================================
#  UNIFIED METRICS REPORT
#===============================================================================
#  Captures all pre-warming and block timing metrics from Prometheus endpoint
#
#  Usage:
#    ./get_key_metrics.sh                    # localhost:9001
#    ./get_key_metrics.sh 192.168.1.100      # custom IP
#    ./get_key_metrics.sh 192.168.1.100 9001 # custom IP and port
#===============================================================================

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

#-------------------------------------------------------------------------------
# Helper function to extract metric value
#-------------------------------------------------------------------------------
get_val() {
    local val=$(echo "$METRICS" | grep "^$1 " | grep -v "^#" | awk '{print $2}' | head -1)
    echo "${val:-0}"
}

# Get histogram sum and count for average calculation (using awk for reliability)
get_histogram_avg_ms() {
    local metric_prefix="$1"

    # Extract sum and count
    local sum=$(echo "$METRICS" | grep "${metric_prefix}_sum " | awk '{print $2}' | head -1)
    local count=$(echo "$METRICS" | grep "${metric_prefix}_count " | awk '{print $2}' | head -1)

    # Default to 0 if empty
    sum="${sum:-0}"
    count="${count:-0}"

    # Use awk for calculation (more reliable than bc)
    echo "$sum $count" | awk '{
        if ($2 > 0) {
            avg_us = ($1 / $2) * 1000000
            if (avg_us >= 1000) {
                printf "%.2fms", avg_us / 1000
            } else {
                printf "%dus", int(avg_us)
            }
        } else {
            print "N/A"
        }
    }'
}

#-------------------------------------------------------------------------------
# Extract Pre-Warming Metrics
# Use CachedReads metrics (reth_txpool_pre_warming_cache_*) NOT ExecutionCache (reth_sync_caching_*)
#-------------------------------------------------------------------------------
CACHE_HITS=$(get_val "reth_txpool_pre_warming_cache_hits")
CACHE_MISSES=$(get_val "reth_txpool_pre_warming_cache_misses")

SIMULATIONS_TRIGGERED=$(get_val "reth_txpool_pre_warming_simulations_triggered")
SIMULATIONS_COMPLETED=$(get_val "reth_txpool_pre_warming_simulations_completed")
SIMULATIONS_FAILED=$(get_val "reth_txpool_pre_warming_simulations_failed")
SIMULATIONS_DROPPED=$(get_val "reth_txpool_pre_warming_simulations_dropped")

CACHE_ENTRIES=$(get_val "reth_txpool_pre_warming_cache_entries")
CACHE_KEYS=$(get_val "reth_txpool_pre_warming_cache_keys_total")
CACHE_EVICTIONS=$(get_val "reth_txpool_pre_warming_cache_evictions")

PREFETCH_OPS=$(get_val "reth_txpool_pre_warming_prefetch_operations")
PREFETCH_ACCOUNTS=$(get_val "reth_txpool_pre_warming_prefetch_accounts")
PREFETCH_STORAGE=$(get_val "reth_txpool_pre_warming_prefetch_storage_slots")
PREFETCH_CONTRACTS=$(get_val "reth_txpool_pre_warming_prefetch_contracts")

#-------------------------------------------------------------------------------
# Extract Block Timing Metrics (averages in ms)
#-------------------------------------------------------------------------------
BUILD_PRE_EXEC_AVG=$(get_histogram_avg_ms "reth_block_timing_build_apply_pre_execution_changes")
BUILD_SEQ_TXS_AVG=$(get_histogram_avg_ms "reth_block_timing_build_exec_sequencer_transactions")
BUILD_SELECT_AVG=$(get_histogram_avg_ms "reth_block_timing_build_select_mempool_transactions")
BUILD_EXEC_AVG=$(get_histogram_avg_ms "reth_block_timing_build_exec_mempool_transactions")
BUILD_STATE_ROOT_AVG=$(get_histogram_avg_ms "reth_block_timing_build_calc_state_root")
BUILD_TOTAL_AVG=$(get_histogram_avg_ms "reth_block_timing_build_total")

INSERT_VALIDATE_AVG=$(get_histogram_avg_ms "reth_block_timing_insert_validate_and_execute")
INSERT_TREE_AVG=$(get_histogram_avg_ms "reth_block_timing_insert_insert_to_tree")
INSERT_TOTAL_AVG=$(get_histogram_avg_ms "reth_block_timing_insert_total")

#-------------------------------------------------------------------------------
# Extract Timing Histograms (pre-warming specific)
#-------------------------------------------------------------------------------
SIM_DURATION_AVG=$(get_histogram_avg_ms "reth_txpool_pre_warming_simulation_duration")
PREFETCH_DURATION_AVG=$(get_histogram_avg_ms "reth_txpool_pre_warming_prefetch_duration")

#-------------------------------------------------------------------------------
# Calculate Cache Hit Rates
#-------------------------------------------------------------------------------
# EVM Cache (sync caching - actual execution cache)
EVM_TOTAL_HITS=$(echo "$ACCOUNT_HITS + $STORAGE_HITS" | bc 2>/dev/null || echo "0")
EVM_TOTAL_MISSES=$(echo "$ACCOUNT_MISSES + $STORAGE_MISSES" | bc 2>/dev/null || echo "0")
EVM_TOTAL_ACCESS=$(echo "$EVM_TOTAL_HITS + $EVM_TOTAL_MISSES" | bc 2>/dev/null || echo "0")

if [ "$EVM_TOTAL_ACCESS" != "0" ] && [ "$EVM_TOTAL_ACCESS" != "" ]; then
    EVM_HIT_RATE=$(echo "scale=1; $EVM_TOTAL_HITS * 100 / $EVM_TOTAL_ACCESS" | bc 2>/dev/null || echo "0")
else
    EVM_HIT_RATE="N/A"
fi

# Pre-warming Cache
PW_TOTAL_ACCESS=$(echo "$CACHE_HITS + $CACHE_MISSES" | bc 2>/dev/null || echo "0")
if [ "$PW_TOTAL_ACCESS" != "0" ] && [ "$PW_TOTAL_ACCESS" != "" ]; then
    PW_HIT_RATE=$(echo "scale=1; $CACHE_HITS * 100 / $PW_TOTAL_ACCESS" | bc 2>/dev/null || echo "0")
else
    PW_HIT_RATE="N/A"
fi

# Simulation Success Rate
SIM_TOTAL=$(echo "$SIMULATIONS_COMPLETED + $SIMULATIONS_FAILED" | bc 2>/dev/null || echo "0")
if [ "$SIM_TOTAL" != "0" ] && [ "$SIM_TOTAL" != "" ]; then
    SIM_SUCCESS_RATE=$(echo "scale=1; $SIMULATIONS_COMPLETED * 100 / $SIM_TOTAL" | bc 2>/dev/null || echo "0")
else
    SIM_SUCCESS_RATE="N/A"
fi

#-------------------------------------------------------------------------------
# Print Report
#-------------------------------------------------------------------------------
echo ""
echo "╔══════════════════════════════════════════════════════════════════════════════╗"
echo "║               UNIFIED PRE-WARMING & BLOCK TIMING METRICS                     ║"
echo "╠══════════════════════════════════════════════════════════════════════════════╣"
echo "║  Host: $DEVNET_IP:$METRICS_PORT"
echo "║  Time: $(date '+%Y-%m-%d %H:%M:%S')"
echo "╚══════════════════════════════════════════════════════════════════════════════╝"
echo ""

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  PRE-WARMING: SIMULATION                                                     │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Triggered:" "$SIMULATIONS_TRIGGERED" "Avg Duration:" "${SIM_DURATION_AVG}"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Completed:" "$SIMULATIONS_COMPLETED" "Success Rate:" "${SIM_SUCCESS_RATE}%"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Failed:" "$SIMULATIONS_FAILED" "Dropped:" "$SIMULATIONS_DROPPED"
echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  PRE-WARMING: PREFETCH                                                       │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Operations:" "$PREFETCH_OPS" "Avg Duration:" "${PREFETCH_DURATION_AVG}"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Accounts Fetched:" "$PREFETCH_ACCOUNTS" "Contracts:" "$PREFETCH_CONTRACTS"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Storage Slots:" "$PREFETCH_STORAGE" "" ""
echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  PRE-WARMING: CACHE                                                          │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Cache Entries:" "$CACHE_ENTRIES" "Total Keys:" "$CACHE_KEYS"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Cache Hits:" "$CACHE_HITS" "Cache Misses:" "$CACHE_MISSES"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Evictions:" "$CACHE_EVICTIONS" "HIT RATE:" "${PW_HIT_RATE}%"
echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  EVM EXECUTION: CACHE (sync_caching)                                         │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Account Hits:" "$ACCOUNT_HITS" "Account Misses:" "$ACCOUNT_MISSES"
printf "│  %-25s %15s  │  %-25s %10s │\n" "Storage Hits:" "$STORAGE_HITS" "Storage Misses:" "$STORAGE_MISSES"
printf "│  %-25s %15s  │  %-25s %10s │\n" "TOTAL HITS:" "$EVM_TOTAL_HITS" "TOTAL MISSES:" "$EVM_TOTAL_MISSES"
printf "│  %-25s %50s │\n" "" ""
printf "│  %-25s %15s                                        │\n" "EVM HIT RATE:" "${EVM_HIT_RATE}%"
echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""

echo "┌──────────────────────────────────────────────────────────────────────────────┐"
echo "│  BLOCK TIMING                                                                │"
echo "├──────────────────────────────────────────────────────────────────────────────┤"
printf "│  %-35s %40s │\n" "Block Execution Time:" "${BUILD_EXEC_AVG}"
printf "│  %-35s %40s │\n" "State Root Calculation:" "${BUILD_STATE_ROOT_AVG}"
printf "│  %-35s %40s │\n" "Total Block Build Time:" "${BUILD_TOTAL_AVG}"
echo "└──────────────────────────────────────────────────────────────────────────────┘"
echo ""


echo "══════════════════════════════════════════════════════════════════════════════"
echo "  KEY PERFORMANCE INDICATORS"
echo "══════════════════════════════════════════════════════════════════════════════"
echo ""
echo "  📊 EVM Cache Hit Rate:       ${EVM_HIT_RATE}%"
echo "  🔄 Simulation Success:       ${SIM_SUCCESS_RATE}%"
echo "  ⏱️  Avg Simulation Time:     ${SIM_DURATION_AVG}"
echo "  ⏱️  Avg Prefetch Time:       ${PREFETCH_DURATION_AVG}"
echo ""
echo "  ⚡ Block Execution Time:     ${BUILD_EXEC_AVG}"
echo "  🌳 State Root Calc Time:     ${BUILD_STATE_ROOT_AVG}"
echo "  📦 Total Block Build Time:   ${BUILD_TOTAL_AVG}"
echo ""
echo "══════════════════════════════════════════════════════════════════════════════"
