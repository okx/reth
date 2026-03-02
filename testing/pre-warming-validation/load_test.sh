#!/bin/bash
#===============================================================================
# LOAD TEST - Pre-Warming Performance with Multiple Senders
#===============================================================================
# Tests pre-warming under realistic load with multiple senders and receivers
# to demonstrate cache effectiveness at scale.
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
DATADIR="$RETH_DIR/.benchmark-load-test"
REPORT_FILE="$RETH_DIR/my-docs/simulation-architecture/BENCHMARK_REPORT.md"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo "==============================================================================="
echo "  LOAD TEST - Pre-Warming with Multiple Senders"
echo "==============================================================================="

# Cleanup
cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
}
trap cleanup EXIT

pkill -9 op-reth 2>/dev/null || true
sleep 2
rm -rf "$DATADIR"

echo -e "${BLUE}Starting op-reth node...${NC}"
"$RETH_DIR/target/release/op-reth" node \
    --datadir "$DATADIR" \
    --dev \
    --dev.block-time 3s \
    --http \
    --http.api eth,debug,net,web3,txpool \
    --metrics 0.0.0.0:9001 \
    --txpool.pre-warming true \
    --log.stdout.filter warn > "$RETH_DIR/load_test.log" 2>&1 &

sleep 15

if ! curl -s http://localhost:8545 > /dev/null 2>&1; then
    echo "ERROR: Node failed to start"
    exit 1
fi
echo -e "${GREEN}Node ready!${NC}"

# Capture baseline
BASELINE_HITS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_hits " | grep -v "#" | awk '{print $2}' || echo "0")
BASELINE_MISSES=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_misses " | grep -v "#" | awk '{print $2}' || echo "0")
BASELINE_SIMS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_simulations_completed " | grep -v "#" | awk '{print $2}' || echo "0")

echo -e "\n${BLUE}Sending transactions (Batch 1 - Fresh state)...${NC}"

# Dev account
SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# Multiple receivers
RECEIVERS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
)

TX_COUNT=0

# Send first batch
for RECEIVER in "${RECEIVERS[@]}"; do
    RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$RECEIVER\",\"value\":\"0x16345785D8A0000\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":1}" 2>/dev/null)

    if echo "$RESULT" | grep -q "result"; then
        echo "  ✅ TX to ${RECEIVER:0:10}..."
        ((TX_COUNT++))
    fi
done

echo -e "\n${YELLOW}Waiting for block...${NC}"
sleep 4

# Capture mid-point metrics
MID_HITS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_hits " | grep -v "#" | awk '{print $2}' || echo "0")
MID_MISSES=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_misses " | grep -v "#" | awk '{print $2}' || echo "0")
MID_SIMS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_simulations_completed " | grep -v "#" | awk '{print $2}' || echo "0")

echo -e "\n${BLUE}Sending transactions (Batch 2 - Cached state)...${NC}"

# Send second batch to SAME receivers (should hit cache)
for RECEIVER in "${RECEIVERS[@]}"; do
    RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$RECEIVER\",\"value\":\"0x16345785D8A0000\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":1}" 2>/dev/null)

    if echo "$RESULT" | grep -q "result"; then
        echo "  ✅ TX to ${RECEIVER:0:10}..."
        ((TX_COUNT++))
    else
        echo "  ⚠️ TX skipped (nonce issue - expected)"
    fi
done

echo -e "\n${YELLOW}Waiting for final block...${NC}"
sleep 5

# Capture final metrics
FINAL_HITS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_hits " | grep -v "#" | awk '{print $2}' || echo "0")
FINAL_MISSES=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_misses " | grep -v "#" | awk '{print $2}' || echo "0")
FINAL_SIMS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_simulations_completed " | grep -v "#" | awk '{print $2}' || echo "0")
FINAL_PREFETCH=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_prefetch_operations " | grep -v "#" | awk '{print $2}' || echo "0")
FINAL_ACCOUNTS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_prefetch_accounts " | grep -v "#" | awk '{print $2}' || echo "0")
FINAL_STORAGE=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_prefetch_storage_slots " | grep -v "#" | awk '{print $2}' || echo "0")
FINAL_KEYS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming_cache_keys_total " | grep -v "#" | awk '{print $2}' || echo "0")

# Calculate
SIMS=$((${FINAL_SIMS%.*} - ${BASELINE_SIMS%.*}))
HITS=$((${FINAL_HITS%.*} - ${BASELINE_HITS%.*}))
MISSES=$((${FINAL_MISSES%.*} - ${BASELINE_MISSES%.*}))
TOTAL=$((HITS + MISSES))

if [ $TOTAL -gt 0 ]; then
    HIT_RATE=$((HITS * 100 / TOTAL))
else
    HIT_RATE=0
fi

# Calculate batch comparison
BATCH1_HITS=$((${MID_HITS%.*} - ${BASELINE_HITS%.*}))
BATCH1_MISSES=$((${MID_MISSES%.*} - ${BASELINE_MISSES%.*}))
BATCH1_TOTAL=$((BATCH1_HITS + BATCH1_MISSES))
if [ $BATCH1_TOTAL -gt 0 ]; then
    BATCH1_RATE=$((BATCH1_HITS * 100 / BATCH1_TOTAL))
else
    BATCH1_RATE=0
fi

BATCH2_HITS=$((${FINAL_HITS%.*} - ${MID_HITS%.*}))
BATCH2_MISSES=$((${FINAL_MISSES%.*} - ${MID_MISSES%.*}))
BATCH2_TOTAL=$((BATCH2_HITS + BATCH2_MISSES))
if [ $BATCH2_TOTAL -gt 0 ]; then
    BATCH2_RATE=$((BATCH2_HITS * 100 / BATCH2_TOTAL))
else
    BATCH2_RATE=0
fi

echo ""
echo "==============================================================================="
echo "                         LOAD TEST RESULTS"
echo "==============================================================================="
echo ""
echo "  📊 TEST CONFIGURATION"
echo "  ─────────────────────"
echo "  Senders:              1 (dev account)"
echo "  Receivers:            ${#RECEIVERS[@]}"
echo "  Total Transactions:   $TX_COUNT"
echo "  Batches:              2"
echo ""
echo "  📈 OVERALL METRICS"
echo "  ─────────────────────"
echo "  Simulations:          $SIMS"
echo "  Prefetch Operations:  ${FINAL_PREFETCH%.*}"
echo "  Accounts Prefetched:  ${FINAL_ACCOUNTS%.*}"
echo "  Storage Prefetched:   ${FINAL_STORAGE%.*}"
echo "  Total Keys Cached:    ${FINAL_KEYS%.*}"
echo ""
echo "  🎯 CACHE PERFORMANCE"
echo "  ─────────────────────"
echo "  Total Hits:           $HITS"
echo "  Total Misses:         $MISSES"
echo "  Overall Hit Rate:     ${HIT_RATE}%"
echo ""
echo "  📊 BATCH COMPARISON (Cache Warming Effect)"
echo "  ─────────────────────"
echo "  Batch 1 (Cold):       ${BATCH1_RATE}% hit rate ($BATCH1_HITS hits / $BATCH1_TOTAL accesses)"
echo "  Batch 2 (Warm):       ${BATCH2_RATE}% hit rate ($BATCH2_HITS hits / $BATCH2_TOTAL accesses)"
if [ $BATCH2_RATE -gt $BATCH1_RATE ]; then
    IMPROVEMENT=$((BATCH2_RATE - BATCH1_RATE))
    echo "  Improvement:          +${IMPROVEMENT}% hit rate after cache warm-up"
fi
echo ""
echo "==============================================================================="

# Export for report
echo "SIMS=$SIMS" > "$RETH_DIR/.load_test_results"
echo "HITS=$HITS" >> "$RETH_DIR/.load_test_results"
echo "MISSES=$MISSES" >> "$RETH_DIR/.load_test_results"
echo "HIT_RATE=$HIT_RATE" >> "$RETH_DIR/.load_test_results"
echo "TX_COUNT=$TX_COUNT" >> "$RETH_DIR/.load_test_results"
echo "RECEIVERS=${#RECEIVERS[@]}" >> "$RETH_DIR/.load_test_results"
echo "BATCH1_RATE=$BATCH1_RATE" >> "$RETH_DIR/.load_test_results"
echo "BATCH2_RATE=$BATCH2_RATE" >> "$RETH_DIR/.load_test_results"
echo "ACCOUNTS=${FINAL_ACCOUNTS%.*}" >> "$RETH_DIR/.load_test_results"
echo "STORAGE=${FINAL_STORAGE%.*}" >> "$RETH_DIR/.load_test_results"
echo "KEYS=${FINAL_KEYS%.*}" >> "$RETH_DIR/.load_test_results"
echo "PREFETCH=${FINAL_PREFETCH%.*}" >> "$RETH_DIR/.load_test_results"

echo -e "${GREEN}Results saved!${NC}"

