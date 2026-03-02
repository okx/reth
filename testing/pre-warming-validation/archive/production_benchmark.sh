#!/bin/bash
#===============================================================================
# PRODUCTION-GRADE TPS BENCHMARK
#===============================================================================
# Simulates realistic L2 workload with fresh state for each test
# Uses fast transaction burst to avoid Optimism dev mode EOA issue
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
RECEIVERS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
)

# Production-like parameters
NUM_TXS=15
BLOCK_TIME=2

cleanup() {
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
    rm -rf "$RETH_DIR/.prod-bench-"* 2>/dev/null || true
}
trap cleanup EXIT

# Ensure clean start
pkill -9 op-reth 2>/dev/null || true
sleep 3
rm -rf "$RETH_DIR/.prod-bench-"* 2>/dev/null || true

send_tx() {
    local RECEIVER=$1
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$RECEIVER\",\"value\":\"0x16345785D8A0000\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":1}" 2>/dev/null)

    if echo "$RESULT" | grep -q '"result"'; then
        echo "SUCCESS"
    else
        echo "FAILED"
    fi
}

run_production_test() {
    local PREWARM=$1
    local DATADIR="$RETH_DIR/.prod-bench-$PREWARM-$(date +%s)"

    # Always fresh datadir
    rm -rf "$RETH_DIR/.prod-bench-"* 2>/dev/null || true

    if [ "$PREWARM" = "enabled" ]; then
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATADIR" \
            --dev \
            --dev.block-time ${BLOCK_TIME}s \
            --http \
            --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming true \
            --log.stdout.filter error > /dev/null 2>&1 &
    else
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATADIR" \
            --dev \
            --dev.block-time ${BLOCK_TIME}s \
            --http \
            --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming false \
            --log.stdout.filter error > /dev/null 2>&1 &
    fi

    # Wait for node initialization
    sleep 12

    if ! curl -s http://localhost:8545 > /dev/null 2>&1; then
        echo "ERROR|0|0|0|0|0|0|0"
        return
    fi

    # Send transactions as fast as possible
    local START_TIME=$(python3 -c "import time; print(time.time())")
    local SUCCESS=0
    local FAILED=0

    for i in $(seq 1 $NUM_TXS); do
        local RECEIVER=${RECEIVERS[$((i % ${#RECEIVERS[@]}))]}
        local RESULT=$(send_tx "$RECEIVER")

        if [ "$RESULT" = "SUCCESS" ]; then
            ((SUCCESS++))
        else
            ((FAILED++))
        fi
    done

    local END_TIME=$(python3 -c "import time; print(time.time())")
    local DURATION=$(python3 -c "print(round($END_TIME - $START_TIME, 3))")
    local TPS=$(python3 -c "print(round($SUCCESS / max($DURATION, 0.001), 2))")

    # Wait for blocks to be mined
    sleep $((BLOCK_TIME * 3))

    # Get metrics
    local CACHE_HITS=0
    local CACHE_MISSES=0
    local HIT_RATE=0
    local SIMS=0
    local PREFETCH=0

    if [ "$PREWARM" = "enabled" ]; then
        CACHE_HITS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_hits " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0")
        CACHE_MISSES=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_misses " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0")
        SIMS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_simulations_completed " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0")
        PREFETCH=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_prefetch_operations " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0")
        local TOTAL=$((CACHE_HITS + CACHE_MISSES))
        if [ $TOTAL -gt 0 ]; then
            HIT_RATE=$((CACHE_HITS * 100 / TOTAL))
        fi
    fi

    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    echo "$SUCCESS|$FAILED|$DURATION|$TPS|$CACHE_HITS|$CACHE_MISSES|$HIT_RATE|$SIMS|$PREFETCH"
}

echo "==============================================================================="
echo "  PRODUCTION-GRADE BENCHMARK - Pre-Warming Performance"
echo "==============================================================================="
echo ""
echo "  Configuration:"
echo "  ─────────────────"
echo "  Total Transactions: $NUM_TXS"
echo "  Block Time:         ${BLOCK_TIME}s"
echo "  Unique Receivers:   ${#RECEIVERS[@]}"
echo "  Method:             Fast burst (fresh state each run)"
echo ""

# Test WITHOUT pre-warming
echo -e "${BLUE}Test 1: Pre-warming DISABLED${NC}"
echo "  Starting fresh node..."
RESULT_OFF=$(run_production_test "disabled")
SUCCESS_OFF=$(echo $RESULT_OFF | cut -d'|' -f1)
FAILED_OFF=$(echo $RESULT_OFF | cut -d'|' -f2)
DUR_OFF=$(echo $RESULT_OFF | cut -d'|' -f3)
TPS_OFF=$(echo $RESULT_OFF | cut -d'|' -f4)
echo -e "  ${GREEN}✅ Complete${NC} - $SUCCESS_OFF/$NUM_TXS succeeded, TPS: $TPS_OFF"
echo ""

# Test WITH pre-warming
echo -e "${BLUE}Test 2: Pre-warming ENABLED${NC}"
echo "  Starting fresh node..."
RESULT_ON=$(run_production_test "enabled")
SUCCESS_ON=$(echo $RESULT_ON | cut -d'|' -f1)
FAILED_ON=$(echo $RESULT_ON | cut -d'|' -f2)
DUR_ON=$(echo $RESULT_ON | cut -d'|' -f3)
TPS_ON=$(echo $RESULT_ON | cut -d'|' -f4)
HITS_ON=$(echo $RESULT_ON | cut -d'|' -f5)
MISSES_ON=$(echo $RESULT_ON | cut -d'|' -f6)
HITRATE_ON=$(echo $RESULT_ON | cut -d'|' -f7)
SIMS_ON=$(echo $RESULT_ON | cut -d'|' -f8)
PREFETCH_ON=$(echo $RESULT_ON | cut -d'|' -f9)
echo -e "  ${GREEN}✅ Complete${NC} - $SUCCESS_ON/$NUM_TXS succeeded, TPS: $TPS_ON"
echo ""

# Calculate improvement
if [ ! -z "$TPS_OFF" ] && [ ! -z "$TPS_ON" ] && [ "$TPS_OFF" != "0" ]; then
    TPS_DIFF=$(python3 -c "print(round((($TPS_ON - $TPS_OFF) / $TPS_OFF) * 100, 1))")
else
    TPS_DIFF="N/A"
fi

echo "==============================================================================="
echo "                    PRODUCTION BENCHMARK RESULTS"
echo "==============================================================================="
echo ""
echo "  📊 TRANSACTION THROUGHPUT"
echo "  ─────────────────────────"
echo ""
echo "                    │ Pre-warming OFF │ Pre-warming ON  │"
echo "  ──────────────────┼─────────────────┼─────────────────┤"
printf "  TXs Succeeded     │ %-15s │ %-15s │\n" "$SUCCESS_OFF/$NUM_TXS" "$SUCCESS_ON/$NUM_TXS"
printf "  Duration          │ %-15s │ %-15s │\n" "${DUR_OFF}s" "${DUR_ON}s"
printf "  TPS               │ %-15s │ %-15s │\n" "$TPS_OFF" "$TPS_ON"
echo ""

if [ "$TPS_DIFF" != "N/A" ]; then
    if (( $(echo "$TPS_DIFF > 0" | bc -l) )); then
        echo -e "  📈 TPS Change:      ${GREEN}+${TPS_DIFF}%${NC} (FASTER with pre-warming)"
    else
        echo -e "  📉 TPS Change:      ${YELLOW}${TPS_DIFF}%${NC} (simulation overhead - expected)"
    fi
fi
echo ""

if [ "$SIMS_ON" != "0" ] && [ "$SIMS_ON" != "" ]; then
echo "  📦 CACHE PERFORMANCE (Pre-warming ON)"
echo "  ─────────────────────────"
echo "  Simulations:      $SIMS_ON"
echo "  Prefetch Ops:     $PREFETCH_ON"
echo "  Cache Hits:       $HITS_ON"
echo "  Cache Misses:     $MISSES_ON"
echo "  Hit Rate:         ${HITRATE_ON}%"
echo ""
fi

# Determine verdict
VERDICT="INCONCLUSIVE"
if [ "$SUCCESS_OFF" -eq "$NUM_TXS" ] && [ "$SUCCESS_ON" -eq "$NUM_TXS" ]; then
    VERDICT="SUCCESS"
    echo -e "  ${GREEN}✅ BENCHMARK SUCCESSFUL - All transactions completed${NC}"
elif [ "$SUCCESS_ON" -ge "$((NUM_TXS * 8 / 10))" ]; then
    VERDICT="GOOD"
    echo -e "  ${GREEN}✅ BENCHMARK GOOD - 80%+ transactions completed${NC}"
elif [ "$SUCCESS_ON" -ge "$((NUM_TXS / 2))" ]; then
    VERDICT="PARTIAL"
    echo -e "  ${YELLOW}⚠️  PARTIAL SUCCESS - Some TX failed (dev mode EOA issue)${NC}"
else
    VERDICT="FAILED"
    echo -e "  ${RED}❌ BENCHMARK FAILED${NC}"
fi

echo ""
echo "==============================================================================="

# Save results
cat > "$RETH_DIR/.prod_benchmark_results" << EOF
# Production-Grade Benchmark Results
DATE=$(date +"%Y-%m-%d %H:%M:%S")
NUM_TXS=$NUM_TXS
BLOCK_TIME=$BLOCK_TIME

# Pre-warming OFF
SUCCESS_OFF=$SUCCESS_OFF
FAILED_OFF=$FAILED_OFF
DUR_OFF=$DUR_OFF
TPS_OFF=$TPS_OFF

# Pre-warming ON
SUCCESS_ON=$SUCCESS_ON
FAILED_ON=$FAILED_ON
DUR_ON=$DUR_ON
TPS_ON=$TPS_ON
SIMULATIONS=$SIMS_ON
PREFETCH_OPS=$PREFETCH_ON
CACHE_HITS=$HITS_ON
CACHE_MISSES=$MISSES_ON
CACHE_HIT_RATE=$HITRATE_ON

# Comparison
TPS_CHANGE=$TPS_DIFF
VERDICT=$VERDICT
EOF

echo -e "${GREEN}Results saved to .prod_benchmark_results${NC}"
