#!/bin/bash
#
# Pre-Warming Performance Monitor
#
# Monitors simulation time, prefetch time, and calculates key metrics
# for TPS optimization.
#
# Usage: ./monitor_prewarming_perf.sh [interval_seconds] [duration_seconds]
#

METRICS_URL="${METRICS_URL:-http://localhost:9001/metrics}"
INTERVAL="${1:-5}"
DURATION="${2:-60}"

echo "╔══════════════════════════════════════════════════════════════════════════════╗"
echo "║      PRE-WARMING PERFORMANCE MONITOR                                         ║"
echo "╚══════════════════════════════════════════════════════════════════════════════╝"
echo ""
echo "  Metrics URL: $METRICS_URL"
echo "  Interval: ${INTERVAL}s | Duration: ${DURATION}s"
echo ""

# Function to get metric value
get_metric() {
    local metric_name="$1"
    curl -s "$METRICS_URL" 2>/dev/null | grep "^${metric_name} " | awk '{print $2}'
}

# Initial values
PREV_SIM_SUM=$(get_metric "reth_txpool_pre_warming_simulation_duration_sum")
PREV_SIM_COUNT=$(get_metric "reth_txpool_pre_warming_simulation_duration_count")
PREV_PREFETCH_SUM=$(get_metric "reth_txpool_pre_warming_prefetch_duration_sum")
PREV_PREFETCH_COUNT=$(get_metric "reth_txpool_pre_warming_prefetch_duration_count")
PREV_SIMULATIONS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
PREV_PREFETCH_OPS=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
PREV_CACHE_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
PREV_CACHE_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")

START_TIME=$(date +%s)
ITERATIONS=0

echo "┌──────────┬────────────┬────────────┬────────────┬────────────┬────────────┬──────────┐"
echo "│ Time     │ Sim/s      │ Avg Sim    │ Prefetch/s │ Avg Prefetch│ Cache Hit │ TPS Est  │"
echo "│          │            │ (µs)       │            │ (ms)       │ Rate      │          │"
echo "├──────────┼────────────┼────────────┼────────────┼────────────┼────────────┼──────────┤"

while true; do
    sleep "$INTERVAL"

    # Current values
    SIM_SUM=$(get_metric "reth_txpool_pre_warming_simulation_duration_sum")
    SIM_COUNT=$(get_metric "reth_txpool_pre_warming_simulation_duration_count")
    PREFETCH_SUM=$(get_metric "reth_txpool_pre_warming_prefetch_duration_sum")
    PREFETCH_COUNT=$(get_metric "reth_txpool_pre_warming_prefetch_duration_count")
    SIMULATIONS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
    PREFETCH_OPS=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
    CACHE_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
    CACHE_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")

    # Calculate deltas
    DELTA_SIM_SUM=$(echo "$SIM_SUM - $PREV_SIM_SUM" | bc -l 2>/dev/null || echo "0")
    DELTA_SIM_COUNT=$(echo "$SIM_COUNT - $PREV_SIM_COUNT" | bc 2>/dev/null || echo "0")
    DELTA_PREFETCH_SUM=$(echo "$PREFETCH_SUM - $PREV_PREFETCH_SUM" | bc -l 2>/dev/null || echo "0")
    DELTA_PREFETCH_COUNT=$(echo "$PREFETCH_COUNT - $PREV_PREFETCH_COUNT" | bc 2>/dev/null || echo "0")
    DELTA_SIMULATIONS=$(echo "$SIMULATIONS - $PREV_SIMULATIONS" | bc 2>/dev/null || echo "0")
    DELTA_PREFETCH_OPS=$(echo "$PREFETCH_OPS - $PREV_PREFETCH_OPS" | bc 2>/dev/null || echo "0")
    DELTA_HITS=$(echo "$CACHE_HITS - $PREV_CACHE_HITS" | bc 2>/dev/null || echo "0")
    DELTA_MISSES=$(echo "$CACHE_MISSES - $PREV_CACHE_MISSES" | bc 2>/dev/null || echo "0")

    # Calculate rates
    SIM_RATE=$(echo "scale=1; $DELTA_SIMULATIONS / $INTERVAL" | bc 2>/dev/null || echo "0")
    PREFETCH_RATE=$(echo "scale=2; $DELTA_PREFETCH_OPS / $INTERVAL" | bc 2>/dev/null || echo "0")

    # Calculate average times
    if [ "$DELTA_SIM_COUNT" -gt 0 ] 2>/dev/null; then
        AVG_SIM_US=$(echo "scale=0; ($DELTA_SIM_SUM / $DELTA_SIM_COUNT) * 1000000" | bc 2>/dev/null || echo "0")
    else
        AVG_SIM_US="0"
    fi

    if [ "$DELTA_PREFETCH_COUNT" -gt 0 ] 2>/dev/null; then
        AVG_PREFETCH_MS=$(echo "scale=2; ($DELTA_PREFETCH_SUM / $DELTA_PREFETCH_COUNT) * 1000" | bc 2>/dev/null || echo "0")
    else
        AVG_PREFETCH_MS="0"
    fi

    # Calculate cache hit rate
    TOTAL_ACCESS=$(echo "$DELTA_HITS + $DELTA_MISSES" | bc 2>/dev/null || echo "0")
    if [ "$TOTAL_ACCESS" -gt 0 ] 2>/dev/null; then
        HIT_RATE=$(echo "scale=1; ($DELTA_HITS * 100) / $TOTAL_ACCESS" | bc 2>/dev/null || echo "0")
    else
        HIT_RATE="N/A"
    fi

    # Estimate theoretical max TPS based on simulation time
    # If avg simulation takes X µs, max simulations/sec = 1,000,000 / X * num_workers
    NUM_WORKERS=$(get_metric "reth_txpool_pre_warming_worker_count" 2>/dev/null || echo "6")
    if [ -z "$NUM_WORKERS" ] || [ "$NUM_WORKERS" = "0" ]; then
        NUM_WORKERS=6
    fi

    if [ "$AVG_SIM_US" -gt 0 ] 2>/dev/null; then
        MAX_SIM_TPS=$(echo "scale=0; (1000000 / $AVG_SIM_US) * $NUM_WORKERS" | bc 2>/dev/null || echo "N/A")
    else
        MAX_SIM_TPS="N/A"
    fi

    # Get timestamp
    ELAPSED=$(($(date +%s) - START_TIME))
    TIMESTAMP=$(printf "%02d:%02d" $((ELAPSED / 60)) $((ELAPSED % 60)))

    # Print row
    printf "│ %-8s │ %10s │ %10s │ %10s │ %10s │ %8s%% │ %8s │\n" \
        "$TIMESTAMP" "$SIM_RATE" "$AVG_SIM_US" "$PREFETCH_RATE" "$AVG_PREFETCH_MS" "$HIT_RATE" "$MAX_SIM_TPS"

    # Update previous values
    PREV_SIM_SUM="$SIM_SUM"
    PREV_SIM_COUNT="$SIM_COUNT"
    PREV_PREFETCH_SUM="$PREFETCH_SUM"
    PREV_PREFETCH_COUNT="$PREFETCH_COUNT"
    PREV_SIMULATIONS="$SIMULATIONS"
    PREV_PREFETCH_OPS="$PREFETCH_OPS"
    PREV_CACHE_HITS="$CACHE_HITS"
    PREV_CACHE_MISSES="$CACHE_MISSES"

    ITERATIONS=$((ITERATIONS + 1))

    # Check duration
    if [ "$ELAPSED" -ge "$DURATION" ]; then
        break
    fi
done

echo "└──────────┴────────────┴────────────┴────────────┴────────────┴────────────┴──────────┘"
echo ""

# Calculate overall averages
TOTAL_SIM_SUM=$(get_metric "reth_txpool_pre_warming_simulation_duration_sum")
TOTAL_SIM_COUNT=$(get_metric "reth_txpool_pre_warming_simulation_duration_count")
TOTAL_PREFETCH_SUM=$(get_metric "reth_txpool_pre_warming_prefetch_duration_sum")
TOTAL_PREFETCH_COUNT=$(get_metric "reth_txpool_pre_warming_prefetch_duration_count")

if [ "$TOTAL_SIM_COUNT" -gt 0 ] 2>/dev/null; then
    OVERALL_AVG_SIM=$(echo "scale=2; ($TOTAL_SIM_SUM / $TOTAL_SIM_COUNT) * 1000000" | bc 2>/dev/null || echo "0")
else
    OVERALL_AVG_SIM="0"
fi

if [ "$TOTAL_PREFETCH_COUNT" -gt 0 ] 2>/dev/null; then
    OVERALL_AVG_PREFETCH=$(echo "scale=2; ($TOTAL_PREFETCH_SUM / $TOTAL_PREFETCH_COUNT) * 1000" | bc 2>/dev/null || echo "0")
else
    OVERALL_AVG_PREFETCH="0"
fi

echo "══════════════════════════════════════════════════════════════════════════════"
echo "  OVERALL STATISTICS"
echo "══════════════════════════════════════════════════════════════════════════════"
echo ""
echo "  Simulation Duration:"
echo "    Total Simulations:    $TOTAL_SIM_COUNT"
echo "    Total Time:           ${TOTAL_SIM_SUM}s"
echo "    Average per TX:       ${OVERALL_AVG_SIM} µs"
echo ""
echo "  Prefetch Duration:"
echo "    Total Prefetch Ops:   $TOTAL_PREFETCH_COUNT"
echo "    Total Time:           ${TOTAL_PREFETCH_SUM}s"
echo "    Average per Block:    ${OVERALL_AVG_PREFETCH} ms"
echo ""

# TPS optimization recommendations
echo "══════════════════════════════════════════════════════════════════════════════"
echo "  TPS OPTIMIZATION ANALYSIS"
echo "══════════════════════════════════════════════════════════════════════════════"
echo ""

# Calculate bottlenecks
if [ "$(echo "$OVERALL_AVG_SIM > 500" | bc 2>/dev/null)" = "1" ]; then
    echo "  ⚠ SIMULATION BOTTLENECK: ${OVERALL_AVG_SIM}µs per TX is HIGH"
    echo "    → Increase --txpool.pre-warming-workers (current: $NUM_WORKERS)"
    echo "    → Consider simplifying simulation (skip complex contracts)"
    echo ""
fi

if [ "$(echo "$OVERALL_AVG_PREFETCH > 5" | bc 2>/dev/null)" = "1" ]; then
    echo "  ⚠ PREFETCH BOTTLENECK: ${OVERALL_AVG_PREFETCH}ms per block is HIGH"
    echo "    → Increase --txpool.pre-fetch-workers"
    echo "    → Check MDBX disk I/O performance"
    echo ""
fi

# Calculate theoretical limits
echo "  Theoretical Limits (with current config):"
if [ "$OVERALL_AVG_SIM" != "0" ] && [ -n "$NUM_WORKERS" ]; then
    MAX_SIMULATION_TPS=$(echo "scale=0; (1000000 / $OVERALL_AVG_SIM) * $NUM_WORKERS" | bc 2>/dev/null || echo "N/A")
    echo "    Max Simulation Throughput: $MAX_SIMULATION_TPS TPS"
    echo "    (Based on ${OVERALL_AVG_SIM}µs avg × $NUM_WORKERS workers)"
fi

echo ""
echo "  To increase TPS:"
echo "    1. Reduce simulation time → simplify key extraction"
echo "    2. Increase parallel workers → more CPU utilization"
echo "    3. Reduce prefetch time → faster MDBX access / SSD"
echo "    4. Improve cache hit rate → fewer MDBX queries"
echo ""

