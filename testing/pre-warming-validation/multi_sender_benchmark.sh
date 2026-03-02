#!/bin/bash
#===============================================================================
# MULTI-SENDER SUSTAINED BENCHMARK
#===============================================================================
# This benchmark:
# 1. Uses multiple pre-funded accounts as senders (avoids EOA issue)
# 2. Sends continuous transaction bursts across multiple blocks
# 3. Measures real block building performance
#
# Strategy: The dev account (0xf39...) funds other accounts first,
# then those accounts send transactions (they won't get deposit bytecode)
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

# Dev account (used only to fund other accounts)
DEV_ACCOUNT="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# Pre-funded accounts from the dev mnemonic (accounts 1-10)
# These are derived from "test test test test test test test test test test test junk"
SENDERS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"  # Account 1
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"  # Account 2
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"  # Account 3
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"  # Account 4
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"  # Account 5
)

# Receiver accounts (accounts 6-10)
RECEIVERS=(
    "0x976EA74026E726554dB657fA54763abd0C3a0aa9"  # Account 6
    "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955"  # Account 7
    "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f"  # Account 8
    "0xa0Ee7A142d267C1f36714E4a8F75612F20a79720"  # Account 9
    "0xBcd4042DE499D14e55001CcbB24a551F3b954096"  # Account 10
)

# Test parameters - can now be larger since we use multiple senders
BLOCK_TIME=2          # seconds between blocks
BURSTS=5              # number of transaction bursts
TXS_PER_BURST=5       # transactions per burst (one per sender)
BURST_INTERVAL=3      # seconds between bursts (spans multiple blocks)
TOTAL_TXS=$((BURSTS * TXS_PER_BURST))

cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
    rm -rf "$RETH_DIR/.multi-sender-bench-"* 2>/dev/null || true
}
trap cleanup EXIT

send_tx() {
    local FROM=$1
    local TO=$2
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$FROM\",\"to\":\"$TO\",\"value\":\"0x16345785D8A0000\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":1}" 2>/dev/null)

    if echo "$RESULT" | grep -q '"result"'; then
        echo "SUCCESS"
    else
        # Extract error message for debugging
        local ERROR=$(echo "$RESULT" | grep -o '"message":"[^"]*"' | cut -d'"' -f4)
        echo "FAILED:$ERROR"
    fi
}

get_block_number() {
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null)
    local HEX=$(echo "$RESULT" | grep -o '"result":"0x[^"]*"' | cut -d'"' -f4)
    printf "%d" "$HEX" 2>/dev/null || echo "0"
}

get_balance() {
    local ADDR=$1
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBalance\",\"params\":[\"$ADDR\",\"latest\"],\"id\":1}" 2>/dev/null)
    echo "$RESULT" | grep -o '"result":"0x[^"]*"' | cut -d'"' -f4
}

get_metrics() {
    local METRIC=$1
    curl -s http://localhost:9001/metrics | grep "^${METRIC} " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0"
}

fund_sender_accounts() {
    echo -e "${BLUE}Funding sender accounts from dev account...${NC}" >&2

    # Fund each sender with 10 ETH
    local FUND_AMOUNT="0x8AC7230489E80000"  # 10 ETH in hex

    for sender in "${SENDERS[@]}"; do
        local RESULT=$(send_tx "$DEV_ACCOUNT" "$sender")
        if [[ "$RESULT" == "SUCCESS" ]]; then
            echo "  ✅ Funded $sender" >&2
        else
            echo "  ❌ Failed to fund $sender: $RESULT" >&2
        fi
    done

    # Wait for funding transactions to be mined
    echo -e "${YELLOW}Waiting for funding transactions to be mined...${NC}" >&2
    sleep $((BLOCK_TIME * 2))

    # Verify balances
    echo -e "${BLUE}Verifying sender balances...${NC}" >&2
    local ALL_FUNDED=true
    for sender in "${SENDERS[@]}"; do
        local BALANCE=$(get_balance "$sender")
        if [ -n "$BALANCE" ] && [ "$BALANCE" != "0x0" ]; then
            echo "  ✅ $sender: $BALANCE" >&2
        else
            echo "  ❌ $sender: No balance" >&2
            ALL_FUNDED=false
        fi
    done

    if [ "$ALL_FUNDED" = true ]; then
        echo -e "${GREEN}All senders funded successfully!${NC}" >&2
        return 0
    else
        echo -e "${RED}Some senders not funded!${NC}" >&2
        return 1
    fi
}

run_multi_sender_test() {
    local PREWARM=$1
    local DATADIR="$RETH_DIR/.multi-sender-bench-$PREWARM-$(date +%s)"

    # Clean previous
    rm -rf "$RETH_DIR/.multi-sender-bench-"* 2>/dev/null || true

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

    # Fund sender accounts first
    fund_sender_accounts

    # Get initial state (after funding)
    local START_BLOCK=$(get_block_number)
    local START_TIME=$(python3 -c "import time; print(time.time())")
    local TOTAL_SUCCESS=0
    local TOTAL_FAILED=0

    echo "" >&2
    echo -e "${BLUE}Sending $BURSTS bursts of $TXS_PER_BURST transactions (using ${#SENDERS[@]} senders)...${NC}" >&2
    echo "  (Burst interval: ${BURST_INTERVAL}s, Block time: ${BLOCK_TIME}s)" >&2
    echo "" >&2

    # Send bursts across multiple blocks
    for burst in $(seq 1 $BURSTS); do
        local BURST_SUCCESS=0
        local BURST_FAILED=0

        echo -n "  Burst $burst/$BURSTS: " >&2

        # Each sender sends one transaction per burst
        for i in $(seq 0 $((${#SENDERS[@]} - 1))); do
            local SENDER=${SENDERS[$i]}
            local RECEIVER=${RECEIVERS[$((i % ${#RECEIVERS[@]}))]}
            local RESULT=$(send_tx "$SENDER" "$RECEIVER")

            if [[ "$RESULT" == "SUCCESS" ]]; then
                ((BURST_SUCCESS++))
                ((TOTAL_SUCCESS++))
            else
                ((BURST_FAILED++))
                ((TOTAL_FAILED++))
                # Print error for debugging
                echo -n "[${RESULT}]" >&2
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
echo "  MULTI-SENDER SUSTAINED BENCHMARK"
echo "==============================================================================="
echo ""
echo "  Strategy: Use multiple pre-funded accounts to avoid EOA issue"
echo ""
echo "  Configuration:"
echo "  ─────────────────"
echo "  Block Time:         ${BLOCK_TIME}s"
echo "  Senders:            ${#SENDERS[@]}"
echo "  Transaction Bursts: $BURSTS"
echo "  TXs per Burst:      $TXS_PER_BURST"
echo "  Burst Interval:     ${BURST_INTERVAL}s"
echo "  Total TXs:          $TOTAL_TXS"
echo ""

# Ensure clean start
pkill -9 op-reth 2>/dev/null || true
sleep 3

# Test WITHOUT pre-warming
echo "==============================================================================="
echo -e "${BLUE}Test 1: Pre-warming DISABLED${NC}"
echo "==============================================================================="
RESULT_OFF=$(run_multi_sender_test "disabled")
SUCCESS_OFF=$(echo $RESULT_OFF | cut -d'|' -f1)
FAILED_OFF=$(echo $RESULT_OFF | cut -d'|' -f2)
DUR_OFF=$(echo $RESULT_OFF | cut -d'|' -f3)
TPS_OFF=$(echo $RESULT_OFF | cut -d'|' -f4)
BLOCKS_OFF=$(echo $RESULT_OFF | cut -d'|' -f5)
TXPB_OFF=$(echo $RESULT_OFF | cut -d'|' -f6)
echo ""
echo -e "${GREEN}Complete: ${SUCCESS_OFF}/${TOTAL_TXS} TXs, ${BLOCKS_OFF} blocks, ${TPS_OFF} TPS${NC}"
echo ""

sleep 3

# Test WITH pre-warming
echo "==============================================================================="
echo -e "${BLUE}Test 2: Pre-warming ENABLED${NC}"
echo "==============================================================================="
RESULT_ON=$(run_multi_sender_test "enabled")
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
echo -e "${GREEN}Complete: ${SUCCESS_ON}/${TOTAL_TXS} TXs, ${BLOCKS_ON} blocks, ${TPS_ON} TPS${NC}"
echo ""

# Calculate improvement
if [ ! -z "$TPS_OFF" ] && [ ! -z "$TPS_ON" ] && [ "$TPS_OFF" != "0" ]; then
    TPS_DIFF=$(python3 -c "print(round((($TPS_ON - $TPS_OFF) / $TPS_OFF) * 100, 1))")
else
    TPS_DIFF="N/A"
fi

echo "==============================================================================="
echo "                    MULTI-SENDER BENCHMARK RESULTS"
echo "==============================================================================="
echo ""
echo "  📊 MULTI-BLOCK PERFORMANCE (${#SENDERS[@]} senders, ${BURSTS} bursts)"
echo "  ─────────────────────────"
echo ""
echo "                    │ Pre-warming OFF │ Pre-warming ON  │"
echo "  ──────────────────┼─────────────────┼─────────────────┤"
printf "  TXs Succeeded     │ %-15s │ %-15s │\n" "$SUCCESS_OFF/$TOTAL_TXS" "$SUCCESS_ON/$TOTAL_TXS"
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
if [ "$SUCCESS_OFF" -eq "$TOTAL_TXS" ] && [ "$SUCCESS_ON" -eq "$TOTAL_TXS" ]; then
    echo -e "  ${GREEN}✅ All transactions succeeded in both tests${NC}"
    echo -e "  ${GREEN}✅ Multi-sender approach avoids EOA issue${NC}"
    if (( $(echo "$TPS_DIFF > 0" | bc -l 2>/dev/null || echo "0") )); then
        echo -e "  ${GREEN}✅ Pre-warming shows +${TPS_DIFF}% TPS improvement${NC}"
    fi
elif [ "$SUCCESS_ON" -ge "$((TOTAL_TXS * 8 / 10))" ]; then
    echo -e "  ${YELLOW}⚠️  80%+ transactions succeeded${NC}"
else
    echo -e "  ${RED}❌ Low success rate - investigate errors${NC}"
fi

echo ""
echo "==============================================================================="

# Save results
cat > "$RETH_DIR/.multi_sender_benchmark_results" << EOF
# Multi-Sender Sustained Benchmark Results
DATE=$(date +"%Y-%m-%d %H:%M:%S")
BLOCK_TIME=$BLOCK_TIME
SENDERS=${#SENDERS[@]}
BURSTS=$BURSTS
TXS_PER_BURST=$TXS_PER_BURST
TOTAL_TXS=$TOTAL_TXS

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

echo -e "${GREEN}Results saved to .multi_sender_benchmark_results${NC}"

