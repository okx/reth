#!/bin/bash
#===============================================================================
#  REALISTIC PRE-WARMING BENCHMARK v2
#===============================================================================
#
#  This script provides realistic benchmarking comparing cache performance
#  between pre-warming DISABLED and ENABLED modes.
#
#  KEY IMPROVEMENT: Uses `payloads_cached_reads_hits/misses` metrics which are
#  ALWAYS tracked (regardless of pre-warming feature state), allowing true
#  baseline comparison.
#
#  USAGE:
#    ./realistic_benchmark_v2.sh [OPTIONS]
#
#  OPTIONS:
#    --txns N          Total transactions (default: 1000)
#    --reuse-pct N     Percentage of reused addresses (default: 70)
#    --pool-size N     Number of addresses in reuse pool (default: 20)
#    --skip-build      Skip cargo build
#
#===============================================================================

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Defaults
TOTAL_TXNS=1000
REUSE_PCT=70
POOL_SIZE=20
SKIP_BUILD=false
BLOCK_TIME=2

# Parse args
while [[ $# -gt 0 ]]; do
    case $1 in
        --txns) TOTAL_TXNS="$2"; shift 2 ;;
        --reuse-pct) REUSE_PCT="$2"; shift 2 ;;
        --pool-size) POOL_SIZE="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --block-time) BLOCK_TIME="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

RETH_DIR="/Users/lakshmikanth/Documents/optimisation/reth"
RESULTS_DIR="${RETH_DIR}/.realistic-benchmark-v2-$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"

NUM_CPUS=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)

echo ""
echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${BLUE}║          REALISTIC PRE-WARMING BENCHMARK v2                                  ║${NC}"
echo -e "${BOLD}${BLUE}║          (Always-On CachedReads Metrics)                                     ║${NC}"
echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo "  Date:              $(date '+%Y-%m-%d %H:%M:%S')"
echo "  Total Transactions: ${TOTAL_TXNS}"
echo "  Reuse Percentage:  ${REUSE_PCT}%"
echo "  Address Pool Size: ${POOL_SIZE}"
echo "  Results Dir:       ${RESULTS_DIR}"
echo ""

# Build if needed
if [ "$SKIP_BUILD" = false ]; then
    echo -e "${CYAN}Building op-reth with pre-warming...${NC}"
    cd "$RETH_DIR"
    cargo build --release --package op-reth --features pre-warming 2>&1 | tail -3
    echo -e "${GREEN}✓ Build complete${NC}"
fi

# Helper to get metric - now uses the ALWAYS-ON payloads_cached_reads_* metrics
get_metric() {
    local val=$(curl -s "http://localhost:9001/metrics" 2>/dev/null | grep "^$1 " | awk '{print $2}' | head -1)
    # Remove decimal part if present
    val=${val%.*}
    echo "${val:-0}"
}

# Start node
start_node() {
    local PREWARM=$1
    local DATA_DIR=$2

    pkill -9 op-reth 2>/dev/null || true
    sleep 2
    rm -rf "$DATA_DIR"

    if [ "$PREWARM" = "true" ]; then
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATA_DIR" \
            --dev --dev.block-time ${BLOCK_TIME}s \
            --http --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming true \
            --txpool.pre-warming-workers $NUM_CPUS \
            --txpool.pre-fetch-workers $NUM_CPUS \
            --log.stdout.filter error > "$RESULTS_DIR/node_${PREWARM}.log" 2>&1 &
    else
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATA_DIR" \
            --dev --dev.block-time ${BLOCK_TIME}s \
            --http --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming false \
            --log.stdout.filter error > "$RESULTS_DIR/node_${PREWARM}.log" 2>&1 &
    fi

    # Wait for node
    for i in {1..30}; do
        if curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | grep -q result; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# Send transactions with mixed addresses
send_mixed_transactions() {
    python3 << PYEOF
import subprocess
import json
import sys
import time
import random
from eth_account import Account

# Config
TOTAL = ${TOTAL_TXNS}
REUSE_PCT = ${REUSE_PCT}
POOL_SIZE = ${POOL_SIZE}

SENDER = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
PRIVATE_KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
CHAIN_ID = 1337

account = Account.from_key(PRIVATE_KEY)

# Generate address pool for reuse
ADDRESS_POOL = [Account.create().address for _ in range(POOL_SIZE)]

def get_recipient():
    """Return reused or unique address based on REUSE_PCT"""
    if random.randint(1, 100) <= REUSE_PCT:
        return random.choice(ADDRESS_POOL)
    else:
        return Account.create().address

def send_raw_tx(raw_tx):
    try:
        result = subprocess.run(
            ["curl", "-s", "-X", "POST", "http://localhost:8545",
             "-H", "Content-Type: application/json",
             "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_sendRawTransaction", "params": [raw_tx], "id": 1})],
            capture_output=True, text=True, timeout=5
        )
        resp = json.loads(result.stdout)
        return "result" in resp and not "error" in resp
    except:
        return False

def send_eth_transfer(to_addr, nonce):
    from web3 import Web3
    to_checksum = Web3.to_checksum_address(to_addr)
    tx = {
        "nonce": nonce,
        "gasPrice": 1000000000,
        "gas": 21000,
        "to": to_checksum,
        "value": 1000,
        "chainId": CHAIN_ID,
    }
    signed = account.sign_transaction(tx)
    raw_tx = signed.raw_transaction.hex()
    if not raw_tx.startswith("0x"):
        raw_tx = "0x" + raw_tx
    return send_raw_tx(raw_tx)

# Get initial nonce
nonce_result = subprocess.run(
    ["curl", "-s", "-X", "POST", "http://localhost:8545",
     "-H", "Content-Type: application/json",
     "-d", json.dumps({"jsonrpc": "2.0", "method": "eth_getTransactionCount", "params": [SENDER, "latest"], "id": 1})],
    capture_output=True, text=True, timeout=5
)
nonce = int(json.loads(nonce_result.stdout)["result"], 16)

# Track address usage for stats
reused_count = 0
unique_count = 0
success = 0
failed = 0
start_time = time.time()
BURST = 50

for i in range(TOTAL):
    to_addr = get_recipient()

    # Track if this is from pool
    if to_addr in ADDRESS_POOL:
        reused_count += 1
    else:
        unique_count += 1

    if send_eth_transfer(to_addr, nonce):
        success += 1
        nonce += 1
    else:
        failed += 1

    # Progress
    if (i + 1) % BURST == 0 or i == TOTAL - 1:
        pct = (i + 1) * 100 // TOTAL
        elapsed = time.time() - start_time
        tps = success / elapsed if elapsed > 0 else 0
        bar = '#' * (pct // 2) + ' ' * (50 - pct // 2)
        print(f"\r    [{bar}] {pct}% | {i+1}/{TOTAL} | {tps:.1f} TPS", end='', flush=True)

elapsed = time.time() - start_time
tps = success / elapsed if elapsed > 0 else 0

print(f"\n  ✓ Completed: {success}/{TOTAL} in {elapsed:.1f}s ({tps:.1f} TPS)")
print(f"    Reused addresses: {reused_count} ({reused_count*100//TOTAL}%)")
print(f"    Unique addresses: {unique_count} ({unique_count*100//TOTAL}%)")
print(f"    Failed: {failed}")
print(f"SUCCESS:{success}")
print(f"TPS:{tps:.2f}")
PYEOF
}

# Capture metrics - using ALWAYS-ON payloads_cached_reads_* metrics
capture_metrics() {
    local OUTPUT=$1
    local MODE=$2

    sleep 5  # Wait for processing

    # ALWAYS-ON metrics: reth_payloads_cached_reads_hits / reth_payloads_cached_reads_misses
    local HITS=$(get_metric "reth_payloads_cached_reads_hits")
    local MISSES=$(get_metric "reth_payloads_cached_reads_misses")

    # Pre-warming specific metrics (will be 0 when pre-warming is OFF)
    local SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
    local PREFETCH=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
    local PREFETCH_ACCTS=$(get_metric "reth_txpool_pre_warming_prefetch_accounts")

    local BUILD_EXEC_SUM=$(get_metric "reth_block_timing_build_exec_mempool_transactions_sum")
    local BUILD_EXEC_COUNT=$(get_metric "reth_block_timing_build_exec_mempool_transactions_count")
    local STATE_ROOT_SUM=$(get_metric "reth_block_timing_build_calc_state_root_sum")
    local STATE_ROOT_COUNT=$(get_metric "reth_block_timing_build_calc_state_root_count")

    HITS=${HITS:-0}
    MISSES=${MISSES:-0}

    local TOTAL_ACCESS=$((HITS + MISSES))
    local HIT_RATE=0
    if [ "$TOTAL_ACCESS" -gt 0 ]; then
        HIT_RATE=$(python3 -c "print(round($HITS * 100 / $TOTAL_ACCESS, 1))")
    fi

    local BLOCK_EXEC_MS=0
    local STATE_ROOT_MS=0
    if [ "${BUILD_EXEC_COUNT:-0}" != "0" ] && [ "$BUILD_EXEC_COUNT" != "" ]; then
        BLOCK_EXEC_MS=$(python3 -c "print(round(float('${BUILD_EXEC_SUM:-0}') / float('$BUILD_EXEC_COUNT') * 1000, 4))")
    fi
    if [ "${STATE_ROOT_COUNT:-0}" != "0" ] && [ "$STATE_ROOT_COUNT" != "" ]; then
        STATE_ROOT_MS=$(python3 -c "print(round(float('${STATE_ROOT_SUM:-0}') / float('$STATE_ROOT_COUNT') * 1000, 4))")
    fi

    # Save JSON
    python3 << PYEOF
import json
from datetime import datetime

data = {
    "timestamp": datetime.now().isoformat(),
    "mode": "$MODE",
    "cache_hits": int(${HITS:-0}),
    "cache_misses": int(${MISSES:-0}),
    "cache_hit_rate": float(${HIT_RATE:-0}),
    "simulations": int(${SIMS:-0}),
    "prefetch_ops": int(${PREFETCH:-0}),
    "prefetch_accounts": int(${PREFETCH_ACCTS:-0}),
    "block_execution_ms": float(${BLOCK_EXEC_MS:-0}),
    "state_root_ms": float(${STATE_ROOT_MS:-0})
}
with open("$OUTPUT", "w") as f:
    json.dump(data, f, indent=2)
PYEOF

    echo "  Cache Hit Rate: ${HIT_RATE}%"
    echo "  Cache Hits: ${HITS} | Misses: ${MISSES}"
    echo "  Block Exec: ${BLOCK_EXEC_MS} ms | State Root: ${STATE_ROOT_MS} ms"
}

#===============================================================================
# PHASE 1: Pre-warming OFF
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 1: Pre-warming DISABLED${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

echo -e "${CYAN}Starting node (pre-warming OFF)...${NC}"
if start_node "false" "${RESULTS_DIR}/data_off"; then
    echo -e "${GREEN}✓ Node ready${NC}"
else
    echo -e "${RED}✗ Node failed to start${NC}"
    exit 1
fi

echo ""
echo -e "${BOLD}Sending transactions...${NC}"
TPS_OFF=$(send_mixed_transactions 2>&1 | tee /dev/stderr | grep "^TPS:" | cut -d: -f2)

echo ""
echo -e "${CYAN}Capturing metrics...${NC}"
capture_metrics "${RESULTS_DIR}/results_off.json" "OFF"

pkill -9 op-reth 2>/dev/null || true
sleep 2

#===============================================================================
# PHASE 2: Pre-warming ON
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  PHASE 2: Pre-warming ENABLED${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

echo -e "${CYAN}Starting node (pre-warming ON)...${NC}"
if start_node "true" "${RESULTS_DIR}/data_on"; then
    echo -e "${GREEN}✓ Node ready${NC}"
else
    echo -e "${RED}✗ Node failed to start${NC}"
    exit 1
fi

echo ""
echo -e "${BOLD}Sending transactions...${NC}"
TPS_ON=$(send_mixed_transactions 2>&1 | tee /dev/stderr | grep "^TPS:" | cut -d: -f2)

echo ""
echo -e "${CYAN}Capturing metrics...${NC}"
capture_metrics "${RESULTS_DIR}/results_on.json" "ON"

pkill -9 op-reth 2>/dev/null || true

#===============================================================================
# PHASE 3: COMPARISON
#===============================================================================
echo ""
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  COMPARISON REPORT${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo ""

python3 << PYEOF
import json

with open("${RESULTS_DIR}/results_off.json") as f:
    off = json.load(f)
with open("${RESULTS_DIR}/results_on.json") as f:
    on = json.load(f)

# Cache Hit Rate
hit_off = off['cache_hit_rate']
hit_on = on['cache_hit_rate']
hit_change = hit_on - hit_off

# Block Execution
exec_off = off['block_execution_ms']
exec_on = on['block_execution_ms']
exec_change = ((exec_on - exec_off) / exec_off * 100) if exec_off > 0 else 0

# State Root
state_off = off['state_root_ms']
state_on = on['state_root_ms']
state_change = ((state_on - state_off) / state_off * 100) if state_off > 0 else 0

print("┌──────────────────────────────────────────────────────────────────────────────┐")
print("│  CACHE HIT RATE (payloads_cached_reads_*)                                   │")
print("├──────────────────────────────────────────────────────────────────────────────┤")
print(f"│  Pre-warming OFF:   {hit_off:>8.1f}%  (Hits: {off['cache_hits']}, Misses: {off['cache_misses']})                   │")
print(f"│  Pre-warming ON:    {hit_on:>8.1f}%  (Hits: {on['cache_hits']}, Misses: {on['cache_misses']})                   │")
print(f"│  IMPROVEMENT:       {hit_change:>+8.1f}%                                          │")
print("└──────────────────────────────────────────────────────────────────────────────┘")
print("")

print("┌──────────────────────────────────────────────────────────────────────────────┐")
print("│  BLOCK TIMING                                                               │")
print("├──────────────────────────────────────────────────────────────────────────────┤")
print(f"│  Block Execution (OFF):  {exec_off:>10.4f} ms                                │")
print(f"│  Block Execution (ON):   {exec_on:>10.4f} ms                                │")
print(f"│  Change:                 {exec_change:>+8.1f}%                                    │")
print("│                                                                              │")
print(f"│  State Root (OFF):       {state_off:>10.4f} ms                                │")
print(f"│  State Root (ON):        {state_on:>10.4f} ms                                │")
print(f"│  Change:                 {state_change:>+8.1f}%                                    │")
print("└──────────────────────────────────────────────────────────────────────────────┘")
print("")

if on['simulations'] > 0 or on['prefetch_ops'] > 0:
    print("┌──────────────────────────────────────────────────────────────────────────────┐")
    print("│  PRE-WARMING STATS (ON mode only)                                           │")
    print("├──────────────────────────────────────────────────────────────────────────────┤")
    print(f"│  Simulations:        {on['simulations']:>10}                                   │")
    print(f"│  Prefetch Ops:       {on['prefetch_ops']:>10}                                   │")
    print(f"│  Accounts Fetched:   {on['prefetch_accounts']:>10}                                   │")
    print("└──────────────────────────────────────────────────────────────────────────────┘")
    print("")

print("══════════════════════════════════════════════════════════════════════════════")
print("  SUMMARY")
print("══════════════════════════════════════════════════════════════════════════════")
print("")
print(f"  Baseline Cache Hit Rate (OFF): {hit_off:.1f}%")
print(f"  With Pre-warming (ON):         {hit_on:.1f}%")
print(f"  IMPROVEMENT:                   {hit_change:+.1f}% points")
print("")

if hit_change > 5:
    print("  ✓ Pre-warming IMPROVES cache hit rate over realistic baseline")
elif hit_change > 0:
    print("  ~ Pre-warming shows MARGINAL improvement")
else:
    print("  ✗ Pre-warming shows NO improvement - investigate")

if exec_change < -5:
    print(f"  ✓ Block execution {abs(exec_change):.1f}% FASTER")
if state_change < -5:
    print(f"  ✓ State root {abs(state_change):.1f}% FASTER")

print("")
print("══════════════════════════════════════════════════════════════════════════════")
PYEOF

echo ""
echo -e "${GREEN}Results saved to: ${RESULTS_DIR}${NC}"
echo ""

