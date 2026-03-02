#!/bin/bash
#===============================================================================
# TPS BENCHMARK - Compare Performance With/Without Pre-Warming
#===============================================================================
# Measures Transactions Per Second (TPS) to show performance impact
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
RECEIVERS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
)

NUM_TX=10

cleanup() {
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
    rm -rf "$RETH_DIR/.tps-test-"* 2>/dev/null || true
}
trap cleanup EXIT

# Ensure clean start
pkill -9 op-reth 2>/dev/null || true
sleep 2
rm -rf "$RETH_DIR/.tps-test-"* 2>/dev/null || true

run_tps_test() {
    local PREWARM=$1
    # Use timestamp to ensure fresh datadir each run (avoids EOA issue)
    local DATADIR="$RETH_DIR/.tps-test-$PREWARM-$(date +%s)"

    # Always clean previous test dirs
    rm -rf "$RETH_DIR/.tps-test-"* 2>/dev/null || true

    if [ "$PREWARM" = "enabled" ]; then
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATADIR" \
            --dev \
            --dev.block-time 1s \
            --http \
            --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming true \
            --log.stdout.filter error > /dev/null 2>&1 &
    else
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATADIR" \
            --dev \
            --dev.block-time 1s \
            --http \
            --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming false \
            --log.stdout.filter error > /dev/null 2>&1 &
    fi

    sleep 12

    if ! curl -s http://localhost:8545 > /dev/null 2>&1; then
        echo "0"
        return
    fi

    # Send transactions and measure time
    local START_TIME=$(python3 -c "import time; print(time.time())")
    local SUCCESS=0

    for i in $(seq 1 $NUM_TX); do
        local RECEIVER=${RECEIVERS[$((i % ${#RECEIVERS[@]}))]}
        local RESULT=$(curl -s http://localhost:8545 -X POST \
            -H "Content-Type: application/json" \
            -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$RECEIVER\",\"value\":\"0x16345785D8A0000\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":1}" 2>/dev/null)

        if echo "$RESULT" | grep -q "result"; then
            ((SUCCESS++))
        fi
    done

    local END_TIME=$(python3 -c "import time; print(time.time())")
    local DURATION=$(python3 -c "print(round($END_TIME - $START_TIME, 3))")

    # Wait for block to be mined
    sleep 6

    # Get block info
    local BLOCK_NUM=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | \
        grep -o '"result":"0x[^"]*"' | cut -d'"' -f4)

    local BLOCK_DEC=$((BLOCK_NUM))

    # Calculate TPS
    local TPS=$(python3 -c "print(round($SUCCESS / $DURATION, 2))")

    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    echo "$SUCCESS|$DURATION|$TPS|$BLOCK_DEC"
}

echo "==============================================================================="
echo "  TPS BENCHMARK - Pre-Warming Performance Comparison"
echo "==============================================================================="
echo ""

# Test WITHOUT pre-warming
echo -e "${BLUE}Test 1: Pre-warming DISABLED${NC}"
echo "Starting node without pre-warming..."
RESULT_OFF=$(run_tps_test "disabled")
TX_OFF=$(echo $RESULT_OFF | cut -d'|' -f1)
DUR_OFF=$(echo $RESULT_OFF | cut -d'|' -f2)
TPS_OFF=$(echo $RESULT_OFF | cut -d'|' -f3)
BLOCKS_OFF=$(echo $RESULT_OFF | cut -d'|' -f4)
echo -e "  Transactions: $TX_OFF"
echo -e "  Duration: ${DUR_OFF}s"
echo -e "  TPS: ${TPS_OFF}"
echo ""

# Test WITH pre-warming
echo -e "${BLUE}Test 2: Pre-warming ENABLED${NC}"
echo "Starting node with pre-warming..."
RESULT_ON=$(run_tps_test "enabled")
TX_ON=$(echo $RESULT_ON | cut -d'|' -f1)
DUR_ON=$(echo $RESULT_ON | cut -d'|' -f2)
TPS_ON=$(echo $RESULT_ON | cut -d'|' -f3)
BLOCKS_ON=$(echo $RESULT_ON | cut -d'|' -f4)
echo -e "  Transactions: $TX_ON"
echo -e "  Duration: ${DUR_ON}s"
echo -e "  TPS: ${TPS_ON}"
echo ""

# Calculate difference
if [ ! -z "$TPS_OFF" ] && [ ! -z "$TPS_ON" ] && [ "$TPS_OFF" != "0" ]; then
    DIFF=$(python3 -c "print(round((($TPS_ON - $TPS_OFF) / $TPS_OFF) * 100, 1))")
else
    DIFF="N/A"
fi

echo "==============================================================================="
echo "                         TPS COMPARISON RESULTS"
echo "==============================================================================="
echo ""
echo "  ┌─────────────────────────────────────────────────────────────┐"
echo "  │                    TPS BENCHMARK RESULTS                    │"
echo "  ├─────────────────────────────────────────────────────────────┤"
echo "  │  Pre-warming OFF:  ${TPS_OFF} TPS                            "
echo "  │  Pre-warming ON:   ${TPS_ON} TPS                             "
echo "  │  Difference:       ${DIFF}%                                  "
echo "  └─────────────────────────────────────────────────────────────┘"
echo ""
echo "  📊 DETAILED COMPARISON"
echo "  ─────────────────────"
echo "                    | OFF      | ON       |"
echo "  ──────────────────|──────────|──────────|"
echo "  Transactions      | $TX_OFF        | $TX_ON        |"
echo "  Duration (s)      | $DUR_OFF     | $DUR_ON     |"
echo "  TPS               | $TPS_OFF      | $TPS_ON      |"
echo "  Blocks Mined      | $BLOCKS_OFF        | $BLOCKS_ON        |"
echo ""
echo "==============================================================================="

# Save results for report
cat > "$RETH_DIR/.tps_results" << EOF
TPS_OFF=$TPS_OFF
TPS_ON=$TPS_ON
DIFF=$DIFF
TX_OFF=$TX_OFF
TX_ON=$TX_ON
DUR_OFF=$DUR_OFF
DUR_ON=$DUR_ON
BLOCKS_OFF=$BLOCKS_OFF
BLOCKS_ON=$BLOCKS_ON
EOF

echo -e "${GREEN}Results saved to .tps_results${NC}"

