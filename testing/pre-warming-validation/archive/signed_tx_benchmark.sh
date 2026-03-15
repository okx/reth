#!/bin/bash
#===============================================================================
# SIGNED RAW TX BENCHMARK - Bypasses EOA issue completely
#===============================================================================
# Uses eth_sendRawTransaction with pre-signed transactions
# This completely avoids the "sender is not an EOA" issue
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

# Known dev private key (safe to use in dev mode only!)
# This is the first account from "test test test test test test test test test test test junk"
PRIVATE_KEY="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

# Test configuration
BLOCK_TIME=2
NUM_BURSTS=5
TXS_PER_BURST=3
BURST_INTERVAL=2

cleanup() {
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
    rm -rf "$RETH_DIR/.signed-tx-bench-"* 2>/dev/null || true
}
trap cleanup EXIT

get_metrics() {
    local METRIC=$1
    curl -s http://localhost:9001/metrics 2>/dev/null | grep "^${METRIC} " | grep -v "#" | awk '{print $2}' | cut -d'.' -f1 || echo "0"
}

get_nonce() {
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","pending"],"id":1}' 2>/dev/null)
    local HEX=$(echo "$RESULT" | grep -o '"result":"0x[^"]*"' | cut -d'"' -f4)
    printf "%d" "$HEX" 2>/dev/null || echo "0"
}

# Use Python to sign and send transactions
send_signed_txs() {
    local COUNT=$1
    local START_NONCE=$2

    python3 << PYTHON_SCRIPT
import json
import requests
from eth_account import Account
from eth_account.signers.local import LocalAccount

# Private key (dev account)
private_key = "0x$PRIVATE_KEY"
account: LocalAccount = Account.from_key(private_key)

receivers = [
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906",
]

success = 0
failed = 0

for i in range($COUNT):
    nonce = $START_NONCE + i
    receiver = receivers[i % len(receivers)]

    tx = {
        'nonce': nonce,
        'to': receiver,
        'value': 100000000000000000,  # 0.1 ETH
        'gas': 21000,
        'gasPrice': 1000000000,  # 1 gwei
        'chainId': 1337,  # Op dev chain
    }

    try:
        signed = account.sign_transaction(tx)
        raw_tx = signed.raw_transaction.hex()

        response = requests.post(
            'http://localhost:8545',
            json={
                'jsonrpc': '2.0',
                'method': 'eth_sendRawTransaction',
                'params': ['0x' + raw_tx if not raw_tx.startswith('0x') else raw_tx],
                'id': i + 1
            },
            timeout=5
        )

        result = response.json()
        if 'result' in result:
            success += 1
        else:
            failed += 1
            print(f"TX {i+1} failed: {result.get('error', {}).get('message', 'unknown')}")
    except Exception as e:
        failed += 1
        print(f"TX {i+1} exception: {e}")

print(f"SUCCESS:{success}")
print(f"FAILED:{failed}")
PYTHON_SCRIPT
}

run_benchmark() {
    local PREWARM=$1
    local DATADIR="$RETH_DIR/.signed-tx-bench-$PREWARM-$(date +%s)"

    rm -rf "$RETH_DIR/.signed-tx-bench-"* 2>/dev/null || true

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
        echo "0|0|0|0|0|0|0|0"
        return
    fi

    echo -e "${GREEN}Node ready!${NC}" >&2

    local START_TIME=$(python3 -c "import time; print(time.time())")
    local TOTAL_SUCCESS=0
    local TOTAL_FAILED=0

    echo -e "${BLUE}Sending $NUM_BURSTS bursts of $TXS_PER_BURST signed transactions...${NC}" >&2

    for burst in $(seq 1 $NUM_BURSTS); do
        local NONCE=$(get_nonce)
        echo -n "  Burst $burst/$NUM_BURSTS (nonce=$NONCE): " >&2

        local RESULT=$(send_signed_txs $TXS_PER_BURST $NONCE 2>/dev/null)
        local BURST_SUCCESS=$(echo "$RESULT" | grep "SUCCESS:" | cut -d: -f2)
        local BURST_FAILED=$(echo "$RESULT" | grep "FAILED:" | cut -d: -f2)

        TOTAL_SUCCESS=$((TOTAL_SUCCESS + BURST_SUCCESS))
        TOTAL_FAILED=$((TOTAL_FAILED + BURST_FAILED))

        echo "$BURST_SUCCESS/$TXS_PER_BURST success" >&2

        if [ $burst -lt $NUM_BURSTS ]; then
            sleep $BURST_INTERVAL
        fi
    done

    sleep $((BLOCK_TIME * 2))

    local END_TIME=$(python3 -c "import time; print(time.time())")
    local DURATION=$(python3 -c "print(round($END_TIME - $START_TIME, 2))")
    local TPS=$(python3 -c "print(round($TOTAL_SUCCESS / max($DURATION, 0.001), 2))")

    local SIMS=0
    local PREFETCH=0
    local HITS=0
    local MISSES=0
    local HIT_RATE=0

    if [ "$PREWARM" = "enabled" ]; then
        SIMS=$(get_metrics "reth_txpool_pre_warming_simulations_completed")
        PREFETCH=$(get_metrics "reth_txpool_pre_warming_prefetch_operations")
        HITS=$(get_metrics "reth_txpool_pre_warming_cache_hits")
        MISSES=$(get_metrics "reth_txpool_pre_warming_cache_misses")
        local TOTAL=$((HITS + MISSES))
        if [ $TOTAL -gt 0 ]; then
            HIT_RATE=$((HITS * 100 / TOTAL))
        fi
    fi

    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    echo "$TOTAL_SUCCESS|$TOTAL_FAILED|$DURATION|$TPS|$SIMS|$PREFETCH|$HITS|$MISSES|$HIT_RATE"
}

echo "==============================================================================="
echo "  SIGNED RAW TX BENCHMARK - Bypasses EOA Issue"
echo "==============================================================================="
echo ""
echo "  This test uses eth_sendRawTransaction with pre-signed transactions"
echo "  to completely avoid the 'sender is not an EOA' issue."
echo ""
echo "  Configuration:"
echo "    Block Time:     ${BLOCK_TIME}s"
echo "    Bursts:         $NUM_BURSTS"
echo "    TXs per Burst:  $TXS_PER_BURST"
echo "    Total TXs:      $((NUM_BURSTS * TXS_PER_BURST))"
echo ""

# Check for eth_account module
if ! python3 -c "import eth_account" 2>/dev/null; then
    echo -e "${YELLOW}Installing eth-account...${NC}"
    pip3 install eth-account requests -q
fi

pkill -9 op-reth 2>/dev/null || true
sleep 3

# Test WITHOUT pre-warming
echo "==============================================================================="
echo -e "${BLUE}Test 1: Pre-warming DISABLED${NC}"
echo "==============================================================================="
RESULT_OFF=$(run_benchmark "disabled")
SUCCESS_OFF=$(echo $RESULT_OFF | cut -d'|' -f1)
FAILED_OFF=$(echo $RESULT_OFF | cut -d'|' -f2)
DUR_OFF=$(echo $RESULT_OFF | cut -d'|' -f3)
TPS_OFF=$(echo $RESULT_OFF | cut -d'|' -f4)
echo ""
echo -e "${GREEN}Complete: ${SUCCESS_OFF}/$((NUM_BURSTS * TXS_PER_BURST)) TXs, ${TPS_OFF} TPS${NC}"
echo ""

sleep 3

# Test WITH pre-warming
echo "==============================================================================="
echo -e "${BLUE}Test 2: Pre-warming ENABLED${NC}"
echo "==============================================================================="
RESULT_ON=$(run_benchmark "enabled")
SUCCESS_ON=$(echo $RESULT_ON | cut -d'|' -f1)
FAILED_ON=$(echo $RESULT_ON | cut -d'|' -f2)
DUR_ON=$(echo $RESULT_ON | cut -d'|' -f3)
TPS_ON=$(echo $RESULT_ON | cut -d'|' -f4)
SIMS_ON=$(echo $RESULT_ON | cut -d'|' -f5)
PREFETCH_ON=$(echo $RESULT_ON | cut -d'|' -f6)
HITS_ON=$(echo $RESULT_ON | cut -d'|' -f7)
MISSES_ON=$(echo $RESULT_ON | cut -d'|' -f8)
HITRATE_ON=$(echo $RESULT_ON | cut -d'|' -f9)
echo ""
echo -e "${GREEN}Complete: ${SUCCESS_ON}/$((NUM_BURSTS * TXS_PER_BURST)) TXs, ${TPS_ON} TPS${NC}"
echo ""

# Calculate improvement
TPS_DIFF="N/A"
if [ ! -z "$TPS_OFF" ] && [ ! -z "$TPS_ON" ] && [ "$TPS_OFF" != "0" ]; then
    TPS_DIFF=$(python3 -c "print(round((($TPS_ON - $TPS_OFF) / $TPS_OFF) * 100, 1))" 2>/dev/null || echo "N/A")
fi

TOTAL_TXS=$((NUM_BURSTS * TXS_PER_BURST))

echo "==============================================================================="
echo "                    SIGNED TX BENCHMARK RESULTS"
echo "==============================================================================="
echo ""
echo "  📊 PERFORMANCE COMPARISON"
echo "  ─────────────────────────"
echo ""
printf "                    │ Pre-warming OFF │ Pre-warming ON  │\n"
printf "  ──────────────────┼─────────────────┼─────────────────┤\n"
printf "  TXs Succeeded     │ %-15s │ %-15s │\n" "$SUCCESS_OFF/$TOTAL_TXS" "$SUCCESS_ON/$TOTAL_TXS"
printf "  Duration          │ %-15s │ %-15s │\n" "${DUR_OFF}s" "${DUR_ON}s"
printf "  TPS               │ %-15s │ %-15s │\n" "$TPS_OFF" "$TPS_ON"
echo ""

if [ "$TPS_DIFF" != "N/A" ]; then
    if (( $(echo "$TPS_DIFF > 0" | bc -l 2>/dev/null || echo 0) )); then
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

echo "  📋 VERDICT"
echo "  ─────────────────────────"
if [ "$SUCCESS_OFF" -eq "$TOTAL_TXS" ] && [ "$SUCCESS_ON" -eq "$TOTAL_TXS" ]; then
    echo -e "  ${GREEN}✅ All transactions succeeded in both tests!${NC}"
    echo -e "  ${GREEN}✅ EOA issue completely bypassed with signed TXs${NC}"
else
    echo -e "  ${YELLOW}⚠️  Some transactions failed${NC}"
fi
echo ""
echo "==============================================================================="

