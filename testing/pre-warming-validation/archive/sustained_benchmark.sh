#!/bin/bash
#===============================================================================
# SUSTAINED TPS BENCHMARK - Real Block Building Performance
#===============================================================================
# This benchmark:
# 1. Starts node ONCE
# 2. Sends continuous transaction bursts across multiple blocks
# 3. Measures actual block execution performance
# 4. Does NOT restart the node between measurements
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

# Test parameters - designed to span multiple blocks
BLOCK_TIME=2          # seconds between blocks
BURSTS=3              # number of transaction bursts to send (reduced to avoid EOA issue)
TXS_PER_BURST=3       # transactions per burst
BURST_INTERVAL=1      # seconds between bursts (shorter to avoid EOA issue)

cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
    rm -rf "$RETH_DIR/.sustained-bench-"* 2>/dev/null || true
}
trap cleanup EXIT

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

get_block_number() {
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null)
    local HEX=$(echo "$RESULT" | grep -o '"result":"0x[^"]*"' | cut -d'"' -f4)
    printf "%d" "$HEX" 2>/dev/null || echo "0"
}

get_metrics() {
    local METRIC=$1
    curl -s http://localhost:9001/metrics | grep "^${METRIC} " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0"
}

run_sustained_test() {
    local PREWARM=$1
    local DATADIR="$RETH_DIR/.sustained-bench-$PREWARM-$(date +%s)"

    # Clean previous
    rm -rf "$RETH_DIR/.sustained-bench-"* 2>/dev/null || true

    echo -e "${BLUE}Starting node with pre-warming=$PREWARM...${NC}" >&2

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

    sleep 15

    if ! curl -s http://localhost:8545 > /dev/null 2>&1; then
        echo -e "ERROR: Node failed to start" >&2
        echo "0|0|0|0|0|0|0|0|0|0|0"
        return
    fi

    echo -e "${GREEN}Node ready!${NC}" >&2

    # Get initial state
    local START_BLOCK=$(get_block_number)
    local START_TIME=$(python3 -c "import time; print(time.time())")
    local TOTAL_SUCCESS=0
    local TOTAL_FAILED=0

    echo "" >&2
    echo -e "${BLUE}Sending $BURSTS bursts of $TXS_PER_BURST transactions...${NC}" >&2
    echo "  (Burst interval: ${BURST_INTERVAL}s, Block time: ${BLOCK_TIME}s)" >&2
    echo "" >&2

    # Send bursts across multiple blocks
    for burst in $(seq 1 $BURSTS); do
        local BURST_SUCCESS=0
        local BURST_FAILED=0

        echo -n "  Burst $burst/$BURSTS: " >&2

        for i in $(seq 1 $TXS_PER_BURST); do
            local RECEIVER=${RECEIVERS[$(( (burst * TXS_PER_BURST + i) % ${#RECEIVERS[@]} ))]}
            local RESULT=$(send_tx "$RECEIVER")
            if [ "$RESULT" = "SUCCESS" ]; then
                ((BURST_SUCCESS++))
                ((TOTAL_SUCCESS++))
            else
                ((BURST_FAILED++))
                ((TOTAL_FAILED++))
            fi
        done

        local CURRENT_BLOCK=$(get_block_number)
        echo "$BURST_SUCCESS/$TXS_PER_BURST success (block #$CURRENT_BLOCK)" >&2

        # Wait between bursts to span multiple blocks
        if [ $burst -lt $BURSTS ]; then
            sleep $BURST_INTERVAL
        fi
    done

    # Wait for final block to be mined
    echo "" >&2
    echo -e "${YELLOW}Waiting for final blocks to be mined...${NC}" >&2
    sleep $((BLOCK_TIME * 2))

    local END_TIME=$(python3 -c "import time; print(time.time())")
    local END_BLOCK=$(get_block_number)
    local DURATION=$(python3 -c "print(round($END_TIME - $START_TIME, 2))")
    local BLOCKS_MINED=$((END_BLOCK - START_BLOCK))
    local TPS=$(python3 -c "print(round($TOTAL_SUCCESS / max($DURATION, 0.001), 2))")
    local TX_PER_BLOCK=$(python3 -c "print(round($TOTAL_SUCCESS / max($BLOCKS_MINED, 1), 2))")

    # Get cache metrics
    local CACHE_HITS=0
    local CACHE_MISSES=0
    local HIT_RATE=0
    local SIMS=0
    local PREFETCH=0

    if [ "$PREWARM" = "enabled" ]; then
        CACHE_HITS=$(get_metrics "reth_txpool_pre_warming_cache_hits")
        CACHE_MISSES=$(get_metrics "reth_txpool_pre_warming_cache_misses")
        SIMS=$(get_metrics "reth_txpool_pre_warming_simulations_completed")
        PREFETCH=$(get_metrics "reth_txpool_pre_warming_prefetch_operations")
        local TOTAL=$((CACHE_HITS + CACHE_MISSES))
        if [ $TOTAL -gt 0 ]; then
            HIT_RATE=$((CACHE_HITS * 100 / TOTAL))
        fi
    fi

    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    # Return results
    echo "$TOTAL_SUCCESS|$TOTAL_FAILED|$DURATION|$TPS|$BLOCKS_MINED|$TX_PER_BLOCK|$CACHE_HITS|$CACHE_MISSES|$HIT_RATE|$SIMS|$PREFETCH"
}

echo "==============================================================================="
echo "  SUSTAINED TPS BENCHMARK - Multi-Block Performance"
echo "==============================================================================="
echo ""
echo "  Configuration:"
echo "  ─────────────────"
echo "  Block Time:         ${BLOCK_TIME}s"
echo "  Transaction Bursts: $BURSTS"
echo "  TXs per Burst:      $TXS_PER_BURST"
echo "  Burst Interval:     ${BURST_INTERVAL}s (spans multiple blocks)"
echo "  Total TXs:          $((BURSTS * TXS_PER_BURST))"
echo ""

# Ensure clean start
pkill -9 op-reth 2>/dev/null || true
sleep 3

# Test WITHOUT pre-warming
echo "==============================================================================="
echo -e "${BLUE}Test 1: Pre-warming DISABLED${NC}"
echo "==============================================================================="
RESULT_OFF=$(run_sustained_test "disabled")
SUCCESS_OFF=$(echo $RESULT_OFF | cut -d'|' -f1)
FAILED_OFF=$(echo $RESULT_OFF | cut -d'|' -f2)
DUR_OFF=$(echo $RESULT_OFF | cut -d'|' -f3)
TPS_OFF=$(echo $RESULT_OFF | cut -d'|' -f4)
BLOCKS_OFF=$(echo $RESULT_OFF | cut -d'|' -f5)
TXPB_OFF=$(echo $RESULT_OFF | cut -d'|' -f6)
echo ""
echo -e "${GREEN}Complete: ${SUCCESS_OFF}/$((BURSTS * TXS_PER_BURST)) TXs, ${BLOCKS_OFF} blocks, ${TPS_OFF} TPS${NC}"
echo ""

sleep 3

# Test WITH pre-warming
echo "==============================================================================="
echo -e "${BLUE}Test 2: Pre-warming ENABLED${NC}"
echo "==============================================================================="
RESULT_ON=$(run_sustained_test "enabled")
SUCCESS_ON=$(echo $RESULT_ON | cut -d'|' -f1)
FAILED_ON=$(echo $RESULT_ON | cut -d'|' -f2)
DUR_ON=$(echo $RESULT_ON | cut -d'|' -f3)
TPS_ON=$(echo $RESULT_ON | cut -d'|' -f4)
BLOCKS_ON=$(echo $RESULT_ON | cut -d'|' -f5)
TXPB_ON=$(echo $RESULT_ON | cut -d'|' -f6)
HITS_ON=$(echo $RESULT_ON | cut -d'|' -f7)
MISSES_ON=$(echo $RESULT_ON | cut -d'|' -f8)
HITRATE_ON=$(echo $RESULT_ON | cut -d'|' -f9)
SIMS_ON=$(echo $RESULT_ON | cut -d'|' -f10)
PREFETCH_ON=$(echo $RESULT_ON | cut -d'|' -f11)
echo ""
echo -e "${GREEN}Complete: ${SUCCESS_ON}/$((BURSTS * TXS_PER_BURST)) TXs, ${BLOCKS_ON} blocks, ${TPS_ON} TPS${NC}"
echo ""

# Calculate improvement
if [ ! -z "$TPS_OFF" ] && [ ! -z "$TPS_ON" ] && [ "$TPS_OFF" != "0" ]; then
    TPS_DIFF=$(python3 -c "print(round((($TPS_ON - $TPS_OFF) / $TPS_OFF) * 100, 1))")
else
    TPS_DIFF="N/A"
fi

echo "==============================================================================="
echo "                    SUSTAINED BENCHMARK RESULTS"
echo "==============================================================================="
echo ""
echo "  📊 MULTI-BLOCK PERFORMANCE"
echo "  ─────────────────────────"
echo ""
echo "                    │ Pre-warming OFF │ Pre-warming ON  │"
echo "  ──────────────────┼─────────────────┼─────────────────┤"
printf "  TXs Succeeded     │ %-15s │ %-15s │\n" "$SUCCESS_OFF/$((BURSTS * TXS_PER_BURST))" "$SUCCESS_ON/$((BURSTS * TXS_PER_BURST))"
printf "  Blocks Mined      │ %-15s │ %-15s │\n" "$BLOCKS_OFF" "$BLOCKS_ON"
printf "  TX/Block          │ %-15s │ %-15s │\n" "$TXPB_OFF" "$TXPB_ON"
printf "  Duration          │ %-15s │ %-15s │\n" "${DUR_OFF}s" "${DUR_ON}s"
printf "  TPS               │ %-15s │ %-15s │\n" "$TPS_OFF" "$TPS_ON"
echo ""

if [ "$TPS_DIFF" != "N/A" ]; then
    if (( $(echo "$TPS_DIFF > 0" | bc -l) )); then
        echo -e "  📈 TPS Change:      ${GREEN}+${TPS_DIFF}%${NC}"
    else
        echo -e "  📉 TPS Change:      ${YELLOW}${TPS_DIFF}%${NC}"
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

# Verdict
echo "  📋 VERDICT"
echo "  ─────────────────────────"
if [ "$SUCCESS_OFF" -eq "$((BURSTS * TXS_PER_BURST))" ] && [ "$SUCCESS_ON" -eq "$((BURSTS * TXS_PER_BURST))" ]; then
    echo -e "  ${GREEN}✅ All transactions succeeded in both tests${NC}"
    if (( $(echo "$TPS_DIFF > 0" | bc -l 2>/dev/null || echo "0") )); then
        echo -e "  ${GREEN}✅ Pre-warming shows +${TPS_DIFF}% TPS improvement${NC}"
    fi
elif [ "$SUCCESS_ON" -ge "$((BURSTS * TXS_PER_BURST * 8 / 10))" ]; then
    echo -e "  ${YELLOW}⚠️  80%+ transactions succeeded${NC}"
else
    echo -e "  ${RED}❌ Low success rate - check for EOA issue${NC}"
fi

echo ""
echo "==============================================================================="

# Save results
cat > "$RETH_DIR/.sustained_benchmark_results" << EOF
# Sustained Multi-Block Benchmark Results
DATE=$(date +"%Y-%m-%d %H:%M:%S")
BLOCK_TIME=$BLOCK_TIME
BURSTS=$BURSTS
TXS_PER_BURST=$TXS_PER_BURST
TOTAL_TXS=$((BURSTS * TXS_PER_BURST))

# Pre-warming OFF
SUCCESS_OFF=$SUCCESS_OFF
FAILED_OFF=$FAILED_OFF
DUR_OFF=$DUR_OFF
TPS_OFF=$TPS_OFF
BLOCKS_OFF=$BLOCKS_OFF
TXPB_OFF=$TXPB_OFF

# Pre-warming ON
SUCCESS_ON=$SUCCESS_ON
FAILED_ON=$FAILED_ON
DUR_ON=$DUR_ON
TPS_ON=$TPS_ON
BLOCKS_ON=$BLOCKS_ON
TXPB_ON=$TXPB_ON
SIMULATIONS=$SIMS_ON
PREFETCH_OPS=$PREFETCH_ON
CACHE_HITS=$HITS_ON
CACHE_MISSES=$MISSES_ON
CACHE_HIT_RATE=$HITRATE_ON

# Comparison
TPS_CHANGE=$TPS_DIFF
EOF

echo -e "${GREEN}Results saved to .sustained_benchmark_results${NC}"

