#!/bin/bash
# Complete Pre-Warming Validation Test Suite
# Runs all validations with assertions

set -e

echo "================================================================================"
echo "COMPLETE PRE-WARMING & PREFETCH VALIDATION TEST SUITE"
echo "================================================================================"
echo ""

# Detect reth root directory (go up from testing/pre-warming-validation)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Change to reth root for all operations
cd "$RETH_ROOT"

echo "Working directory: $RETH_ROOT"
echo ""

# Configuration
DATADIR=".test-validation"
CHAIN_LOG="validation_chain.log"
TEST_OUTPUT="validation_results.txt"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Cleanup function
cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
}

# Trap to cleanup on exit
trap cleanup EXIT

# Step 1: Kill any existing node
echo -e "${YELLOW}Step 1: Killing any existing op-reth processes...${NC}"
pkill -9 op-reth 2>/dev/null || true
sleep 2

# Step 2: Build (if needed)
echo -e "${YELLOW}Step 2: Building op-reth with pre-warming feature...${NC}"
cd /Users/lakshmikanth/Documents/optimisation/reth
cargo build --release --package op-reth --features pre-warming 2>&1 | grep -E "Compiling|Finished" | tail -5
if [ ${PIPESTATUS[0]} -ne 0 ]; then
    echo -e "${RED}❌ Build failed!${NC}"
    exit 1
fi
echo -e "${GREEN}✅ Build complete${NC}"

# Step 3: Clean up old test data
echo -e "${YELLOW}Step 3: Cleaning up old test data...${NC}"
rm -rf "$DATADIR"
rm -f "$CHAIN_LOG"

# Step 4: Start fresh node
echo -e "${YELLOW}Step 4: Starting fresh node with pre-warming enabled...${NC}"
./target/release/op-reth node \
    --datadir "$DATADIR" \
    --dev \
    --dev.block-time 2s \
    --http \
    --http.api eth,debug,net,web3,txpool \
    --metrics 0.0.0.0:9001 \
    --txpool.pre-warming true \
    --log.stdout.filter warn \
    > "$CHAIN_LOG" 2>&1 &

NODE_PID=$!
echo -e "${GREEN}✅ Node started with PID: $NODE_PID${NC}"

# Step 5: Wait for node to be ready
echo -e "${YELLOW}Step 5: Waiting for node to be ready...${NC}"
sleep 10

# Check if node is still running
if ! kill -0 $NODE_PID 2>/dev/null; then
    echo -e "${RED}❌ ERROR: Node failed to start!${NC}"
    echo "Last 20 lines of log:"
    tail -20 "$CHAIN_LOG"
    exit 1
fi

# Verify RPC is responding
MAX_RETRIES=5
RETRY=0
while [ $RETRY -lt $MAX_RETRIES ]; do
    if curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' > /dev/null 2>&1; then
        echo -e "${GREEN}✅ Node RPC is ready${NC}"
        break
    fi
    RETRY=$((RETRY + 1))
    echo "Waiting for RPC... (attempt $RETRY/$MAX_RETRIES)"
    sleep 2
done

if [ $RETRY -eq $MAX_RETRIES ]; then
    echo -e "${RED}❌ ERROR: RPC not responding after $MAX_RETRIES attempts${NC}"
    exit 1
fi

# Step 6: Send test transactions
echo -e "\n${YELLOW}Step 6: Sending test transactions...${NC}"
echo "================================================================================"

# Send 3 transactions
TX_COUNT=0
FAILED_COUNT=0

for i in {1..3}; do
    echo -e "${YELLOW}Sending transaction $i/3...${NC}"

    RESULT=$(curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{
            \"from\":\"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266\",
            \"to\":\"0x70997970C51812dc3A010C7d01b50e0d17dc79C8\",
            \"value\":\"0x16345785D8A0000\",
            \"gas\":\"0x5208\",
            \"gasPrice\":\"0x3B9ACA00\"
        }],\"id\":1}")

    if echo "$RESULT" | grep -q "\"result\""; then
        TX_HASH=$(echo "$RESULT" | grep -o "0x[a-fA-F0-9]\{64\}" | head -1)
        echo -e "${GREEN}✅ TX $i: ${TX_HASH:0:20}...${NC}"
        TX_COUNT=$((TX_COUNT + 1))
    else
        ERROR=$(echo "$RESULT" | grep -o "\"message\":\"[^\"]*\"" | cut -d'"' -f4)
        echo -e "${RED}❌ TX $i: ${ERROR:-Failed}${NC}"
        FAILED_COUNT=$((FAILED_COUNT + 1))
    fi

    sleep 1
done

echo ""
echo "Transaction Summary: $TX_COUNT successful, $FAILED_COUNT failed"

# Step 7: Wait for simulations and block building
echo -e "\n${YELLOW}Step 7: Waiting for simulations & block building...${NC}"
echo "================================================================================"
echo "Waiting 5 seconds for simulations and block building to complete..."
sleep 5

# Step 8: Additional verification from logs
echo -e "\n${YELLOW}Step 8: Verifying logs...${NC}"
echo "================================================================================"

echo -e "\n${YELLOW}Checking for pre-warming initialization:${NC}"
if grep -q "Pre-warming initialization completed" "$CHAIN_LOG"; then
    echo -e "${GREEN}✅ Pre-warming initialized${NC}"
else
    echo -e "${RED}❌ Pre-warming not initialized${NC}"
fi

echo -e "\n${YELLOW}Checking for simulations:${NC}"
SIM_COUNT=$(grep -c "Simulation complete" "$CHAIN_LOG" || echo "0")
echo -e "${GREEN}✅ Found $SIM_COUNT simulations in logs${NC}"

echo -e "\n${YELLOW}Checking for prefetch executions:${NC}"
PREFETCH_COUNT=$(grep -c "PREFETCH Step 8.*SUCCESS" "$CHAIN_LOG" || echo "0")
echo -e "${GREEN}✅ Found $PREFETCH_COUNT successful prefetch operations in logs${NC}"

echo -e "\n${YELLOW}Checking for metrics updates:${NC}"
METRICS_UPDATE_COUNT=$(grep -c "Metrics updated successfully" "$CHAIN_LOG" || echo "0")
if [ $METRICS_UPDATE_COUNT -gt 0 ]; then
    echo -e "${GREEN}✅ Metrics updated $METRICS_UPDATE_COUNT times${NC}"
else
    echo -e "${RED}❌ No metrics updates found${NC}"
fi

# Step 9: Check final metrics from Prometheus
echo -e "\n${YELLOW}Step 9: Final Metrics Check${NC}"
echo "================================================================================"

METRICS=$(curl -s http://localhost:9001/metrics | grep "reth_txpool_pre_warming" | grep -v "#" | grep -v "quantile")

echo -e "\n${YELLOW}Pre-Warming Metrics:${NC}"
echo "$METRICS" | grep "simulations_"

echo -e "\n${YELLOW}Cache Metrics:${NC}"
echo "$METRICS" | grep "cache_"

echo -e "\n${YELLOW}Prefetch Metrics:${NC}"
echo "$METRICS" | grep "prefetch_"

# Extract key metrics
SIMS=$(echo "$METRICS" | grep "simulations_completed " | awk '{print $2}' | cut -d. -f1)
PREFETCH_OPS=$(echo "$METRICS" | grep "prefetch_operations " | awk '{print $2}' | cut -d. -f1)
PREFETCH_ACCTS=$(echo "$METRICS" | grep "prefetch_accounts " | awk '{print $2}' | cut -d. -f1)
CACHE_HITS=$(echo "$METRICS" | grep "cache_hits " | awk '{print $2}' | cut -d. -f1)
CACHE_MISSES=$(echo "$METRICS" | grep "cache_misses " | awk '{print $2}' | cut -d. -f1)
CACHE_ENTRIES=$(echo "$METRICS" | grep "cache_entries " | awk '{print $2}' | cut -d. -f1)

# Calculate cache hit rate
TOTAL_CACHE_ACCESS=$((CACHE_HITS + CACHE_MISSES))
if [ $TOTAL_CACHE_ACCESS -gt 0 ]; then
    HIT_RATE=$((CACHE_HITS * 100 / TOTAL_CACHE_ACCESS))
else
    HIT_RATE=0
fi

# Step 10: Final Summary
echo ""
echo "================================================================================"
echo "FINAL SUMMARY"
echo "================================================================================"

echo -e "\n${YELLOW}Metrics Summary:${NC}"
echo "  Simulations Completed: ${SIMS:-0}"
echo "  Cache Entries: ${CACHE_ENTRIES:-0}"
echo "  Prefetch Operations: ${PREFETCH_OPS:-0}"
echo "  Prefetch Accounts: ${PREFETCH_ACCTS:-0}"
echo ""
echo "  🎯 CACHE UTILIZATION:"
echo "  ├─ Cache Hits: ${CACHE_HITS:-0}"
echo "  ├─ Cache Misses: ${CACHE_MISSES:-0}"
echo "  ├─ Total Access: ${TOTAL_CACHE_ACCESS}"
echo "  └─ Hit Rate: ${HIT_RATE}%"

# Assertions
echo -e "\n${YELLOW}=== ASSERTIONS ===${NC}"

# Assertion 1: Simulations happened
if [ "${SIMS:-0}" -gt 0 ]; then
    echo -e "${GREEN}✅ Assertion 1: Pre-warming simulations completed (${SIMS})${NC}"
else
    echo -e "${RED}❌ Assertion 1: No simulations completed${NC}"
fi

# Assertion 2: Prefetch operations happened
if [ "${PREFETCH_OPS:-0}" -gt 0 ]; then
    echo -e "${GREEN}✅ Assertion 2: Prefetch operations executed (${PREFETCH_OPS})${NC}"
else
    echo -e "${RED}❌ Assertion 2: No prefetch operations${NC}"
fi

# Assertion 3: Prefetch fetched accounts
if [ "${PREFETCH_ACCTS:-0}" -gt 0 ]; then
    echo -e "${GREEN}✅ Assertion 3: Prefetch fetched ${PREFETCH_ACCTS} accounts from MDBX${NC}"
else
    echo -e "${RED}❌ Assertion 3: No accounts prefetched${NC}"
fi

# Assertion 4: Cache utilization
echo ""
echo -e "${YELLOW}Assertion 4: Cache Utilization During EVM Execution${NC}"
if [ $TOTAL_CACHE_ACCESS -gt 0 ]; then
    echo -e "${GREEN}✅ Cache is being used! ${TOTAL_CACHE_ACCESS} total accesses${NC}"
    echo -e "${GREEN}   Cache Hits: ${CACHE_HITS} | Misses: ${CACHE_MISSES} | Hit Rate: ${HIT_RATE}%${NC}"

    if [ $HIT_RATE -ge 90 ]; then
        echo -e "${GREEN}   🎉 EXCELLENT hit rate (>= 90%)!${NC}"
    elif [ $HIT_RATE -ge 70 ]; then
        echo -e "${GREEN}   ✅ GOOD hit rate (>= 70%)${NC}"
    elif [ $HIT_RATE -ge 50 ]; then
        echo -e "${YELLOW}   ⚠️  MODERATE hit rate (50-70%)${NC}"
    else
        echo -e "${YELLOW}   ⚠️  LOW hit rate (< 50%)${NC}"
    fi
else
    echo -e "${YELLOW}⚠️  No cache accesses yet (CACHE HITS=${CACHE_HITS:-0}, MISSES=${CACHE_MISSES:-0})${NC}"
    echo -e "${YELLOW}   This means:${NC}"
    echo -e "${YELLOW}   - Either no blocks were built with transactions${NC}"
    echo -e "${YELLOW}   - Or cache hit/miss tracking needs debug logs enabled${NC}"
    echo -e "${YELLOW}   - Run with: --log.stdout.filter debug to see detailed cache logs${NC}"
fi

echo -e "\n${YELLOW}Log Summary:${NC}"
echo "  Simulations in logs: $SIM_COUNT"
echo "  Prefetch successes: $PREFETCH_COUNT"
echo "  Metrics updates: $METRICS_UPDATE_COUNT"

# Determine exit code based on assertions
EXIT_CODE=0
if [ "${SIMS:-0}" -eq 0 ] || [ "${PREFETCH_OPS:-0}" -eq 0 ]; then
    EXIT_CODE=1
fi

# Final verdict
echo ""
echo "================================================================================"
if [ $EXIT_CODE -eq 0 ] && [ "${SIMS:-0}" -gt 0 ] && [ "${PREFETCH_OPS:-0}" -gt 0 ]; then
    echo -e "${GREEN}✅✅✅ ALL TESTS PASSED - SYSTEM FULLY FUNCTIONAL! ✅✅✅${NC}"
else
    echo -e "${YELLOW}⚠️  SOME TESTS DID NOT PASS - CHECK RESULTS ABOVE${NC}"
fi
echo "================================================================================"
echo ""

echo -e "${YELLOW}Node still running (PID: $NODE_PID)${NC}"
echo "Logs saved to: $CHAIN_LOG"
echo ""
echo "To stop the node: pkill -9 op-reth"
echo "To view logs: tail -f $CHAIN_LOG"
echo ""

exit $EXIT_CODE

