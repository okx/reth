#!/bin/bash
#===============================================================================
#  UNIFIED PRE-WARMING BENCHMARK SUITE
#===============================================================================
#  Comprehensive benchmark for pre-warming feature comparing:
#  1. ETH Transfers (simple value transfers)
#  2. ERC20 Operations (transfer, transferFrom, approve)
#
#  Generates a unified report showing TPS and Cache performance
#
#  Usage:
#    ./unified_benchmark.sh                      # Quick test (~10K txs, ~6 min)
#    ./unified_benchmark.sh --rebuild            # Rebuild binary first
#    ./unified_benchmark.sh --max-workers        # Use all CPUs as workers
#    ./unified_benchmark.sh --full-load          # Medium load (~28K txs, ~20 min)
#    ./unified_benchmark.sh --high-load          # High load (~100K txs, ~60-90 min)
#    ./unified_benchmark.sh --rebuild --max-workers --high-load  # All options
#===============================================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
TIMESTAMP=$(date +%s)
REPORT_FILE="$RETH_DIR/benchmark_report.md"
LOG_DIR="$RETH_DIR/.unified-benchmark-${TIMESTAMP}"
ERROR_LOG="$RETH_DIR/benchmark_errors.log"

# Clear error log at start
echo "=== Benchmark Error Log - $(date '+%Y-%m-%d %H:%M:%S') ===" > "$ERROR_LOG"

# Parse arguments
REBUILD=false
MAX_WORKERS=false
if [ "$1" = "--rebuild" ] || [ "$2" = "--rebuild" ]; then
    REBUILD=true
fi
if [ "$1" = "--max-workers" ] || [ "$2" = "--max-workers" ]; then
    MAX_WORKERS=true
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
MAGENTA='\033[0;35m'
BOLD='\033[1m'
NC='\033[0m'

# Dev account
SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

# Test recipients
RECIPIENTS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
)

# ERC20 Calldata
ERC20_TRANSFER="0xa9059cbb00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c80000000000000000000000000000000000000000000000000de0b6b3a7640000"
ERC20_TRANSFER_FROM="0x23b872dd000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb9226600000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c80000000000000000000000000000000000000000000000000de0b6b3a7640000"
CONTRACT_ADDR="0x5FbDB2315678afecb367f032d93F642f64180aa3"

# Results - using simple variables
ETH_NORMAL_OFF_TPS="0"
ETH_NORMAL_ON_TPS="0"
ETH_NORMAL_ON_HIT="0"
ETH_PEAK_OFF_TPS="0"
ETH_PEAK_ON_TPS="0"
ETH_PEAK_ON_HIT="0"
ERC20_TRANSFER_NORMAL_OFF_TPS="0"
ERC20_TRANSFER_NORMAL_ON_TPS="0"
ERC20_TRANSFER_NORMAL_ON_HIT="0"
ERC20_TRANSFER_NORMAL_ON_STORAGE="0"
ERC20_TRANSFER_PEAK_OFF_TPS="0"
ERC20_TRANSFER_PEAK_ON_TPS="0"
ERC20_TRANSFER_PEAK_ON_HIT="0"
ERC20_TRANSFER_PEAK_ON_STORAGE="0"
ERC20_TRANSFERFROM_NORMAL_OFF_TPS="0"
ERC20_TRANSFERFROM_NORMAL_ON_TPS="0"
ERC20_TRANSFERFROM_NORMAL_ON_HIT="0"
ERC20_TRANSFERFROM_PEAK_OFF_TPS="0"
ERC20_TRANSFERFROM_PEAK_ON_TPS="0"
ERC20_TRANSFERFROM_PEAK_ON_HIT="0"

mkdir -p "$LOG_DIR"

cleanup() {
    pkill -9 op-reth 2>/dev/null || true
}
trap cleanup EXIT

wait_for_node() {
    local MAX_WAIT=30
    local COUNT=0
    while [ $COUNT -lt $MAX_WAIT ]; do
        if curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | grep -q "result"; then
            return 0
        fi
        sleep 1
        COUNT=$((COUNT + 1))
    done
    return 1
}

get_metric() {
    curl -s http://localhost:9001/metrics 2>/dev/null | grep "^$1 " | awk '{print $2}' | cut -d'.' -f1 || echo "0"
}

start_node() {
    local PREWARM=$1
    local DATADIR="$LOG_DIR/data-$(date +%s%N)"

    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    if [ "$PREWARM" = "enabled" ]; then
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATADIR" --dev --dev.block-time 1s \
            --http --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming true \
            --txpool.pre-warming-workers "$PREWARM_WORKERS" \
            --log.stdout.filter error > /dev/null 2>&1 &
    else
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$DATADIR" --dev --dev.block-time 1s \
            --http --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 --txpool.pre-warming false \
            --log.stdout.filter error > /dev/null 2>&1 &
    fi

    wait_for_node
}

send_transactions() {
    local TX_TYPE=$1
    local COUNT=$2
    local ERROR_LOG_FILE="$3"

    # Use sequential sending to avoid nonce collisions
    # Parallel sending causes "already known" and "replacement underpriced" errors
    python3 << PYEOF
import subprocess
import json
import time
import sys

TX_TYPE = "$TX_TYPE"
COUNT = $COUNT
SENDER = "$SENDER"
CONTRACT_ADDR = "$CONTRACT_ADDR"
ERC20_TRANSFER = "$ERC20_TRANSFER"
ERC20_TRANSFER_FROM = "$ERC20_TRANSFER_FROM"
ERROR_LOG = "$ERROR_LOG"

RECIPIENTS = [
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906",
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65",
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc",
    "0x976EA74026E726554dB657fA54763abd0C3a0aa9",
    "0x14dC79964da2C08b23698B3D3cc7Ca32193d9955",
    "0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f",
]

error_counts = {}

def log_error(msg):
    with open(ERROR_LOG, "a") as f:
        f.write(f"{msg}\n")

def send_single_tx(i):
    recipient = RECIPIENTS[i % len(RECIPIENTS)]

    if TX_TYPE == "eth":
        payload = {
            "jsonrpc": "2.0",
            "method": "eth_sendTransaction",
            "params": [{
                "from": SENDER,
                "to": recipient,
                "value": "0x16345785D8A0000",
                "gas": "0x5208",
                "gasPrice": "0x3B9ACA00"
            }],
            "id": i
        }
    elif TX_TYPE == "erc20_transfer":
        payload = {
            "jsonrpc": "2.0",
            "method": "eth_sendTransaction",
            "params": [{
                "from": SENDER,
                "to": CONTRACT_ADDR,
                "data": ERC20_TRANSFER,
                "gas": "0x30D40",
                "gasPrice": "0x3B9ACA00"
            }],
            "id": i
        }
    elif TX_TYPE == "erc20_transferFrom":
        payload = {
            "jsonrpc": "2.0",
            "method": "eth_sendTransaction",
            "params": [{
                "from": SENDER,
                "to": CONTRACT_ADDR,
                "data": ERC20_TRANSFER_FROM,
                "gas": "0x30D40",
                "gasPrice": "0x3B9ACA00"
            }],
            "id": i
        }

    try:
        result = subprocess.run(
            ['curl', '-s', 'http://localhost:8545', '-X', 'POST',
             '-H', 'Content-Type: application/json',
             '-d', json.dumps(payload)],
            capture_output=True, text=True, timeout=5
        )
        response = result.stdout
        if 'result' in response:
            return (1, None)
        elif 'error' in response:
            try:
                err_data = json.loads(response)
                err_msg = err_data.get('error', {}).get('message', 'Unknown error')
                return (0, err_msg)
            except:
                return (0, f"Parse error: {response[:100]}")
        else:
            return (0, f"No result: {response[:100]}")
    except subprocess.TimeoutExpired:
        return (0, "Timeout")
    except Exception as e:
        return (0, str(e))

# Sequential sending to avoid nonce collisions
success = 0
failed = 0

for i in range(COUNT):
    result, error = send_single_tx(i)
    if result == 1:
        success += 1
    else:
        failed += 1
        if error:
            error_counts[error] = error_counts.get(error, 0) + 1

# Log error summary
if error_counts:
    log_error(f"\n[{TX_TYPE}] Error Summary ({failed} failed out of {COUNT}):")
    for err, count in sorted(error_counts.items(), key=lambda x: -x[1])[:5]:
        log_error(f"  - {err}: {count} times")
        print(f"  [ERROR] {err}: {count}x", file=sys.stderr)

print(success)
PYEOF
}

run_test() {
    local NAME=$1
    local TX_TYPE=$2
    local COUNT=$3
    local PREWARM=$4

    echo -e "    ${CYAN}Running:${NC} $NAME ($COUNT txs, pre-warming=$PREWARM)..."

    # Log test start
    echo "" >> "$ERROR_LOG"
    echo "=== Test: $NAME ($COUNT txs, pre-warming=$PREWARM) ===" >> "$ERROR_LOG"
    echo "Started: $(date '+%Y-%m-%d %H:%M:%S')" >> "$ERROR_LOG"

    start_node "$PREWARM"
    sleep 3

    local BASE_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
    local BASE_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")

    local START=$(python3 -c "import time; print(time.time())")
    local SUCCESS=$(send_transactions "$TX_TYPE" "$COUNT" "$ERROR_LOG")
    local END=$(python3 -c "import time; print(time.time())")

    # Handle empty or invalid SUCCESS
    SUCCESS="${SUCCESS:-0}"
    if [ "$SUCCESS" = "0" ]; then
        echo -e "    ${RED}✗${NC} Failed: No transactions succeeded"
        echo "FAILED: No transactions succeeded" >> "$ERROR_LOG"
        echo "0 0 0"
        pkill -9 op-reth 2>/dev/null || true
        sleep 2
        return
    fi

    sleep 3

    local FINAL_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
    local FINAL_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
    local STORAGE=$(get_metric "reth_txpool_pre_warming_prefetch_storage_slots")

    local DURATION=$(python3 -c "print(round($END - $START, 2))")

    # Avoid division by zero
    local TPS="0"
    if python3 -c "exit(0 if float('$DURATION') > 0 else 1)" 2>/dev/null; then
        TPS=$(python3 -c "print(round(float('$SUCCESS') / float('$DURATION'), 2))")
    fi

    local DELTA_HITS=$((FINAL_HITS - BASE_HITS))
    local DELTA_MISSES=$((FINAL_MISSES - BASE_MISSES))
    local TOTAL=$((DELTA_HITS + DELTA_MISSES))
    local HIT_RATE=0
    if [ $TOTAL -gt 0 ]; then
        HIT_RATE=$((DELTA_HITS * 100 / TOTAL))
    fi

    echo -e "    ${GREEN}✓${NC} Complete: ${SUCCESS}/${COUNT} txs, ${BOLD}${TPS} TPS${NC}, Hit Rate: ${HIT_RATE}%"

    # Log success to error log
    echo "SUCCESS: $SUCCESS/$COUNT txs, TPS: $TPS, Hit Rate: $HIT_RATE%" >> "$ERROR_LOG"

    # Return values
    echo "$TPS $HIT_RATE $STORAGE"

    pkill -9 op-reth 2>/dev/null || true
    sleep 2
}

# Configuration - Load Testing
# Default: Quick test mode (completes in ~5-10 minutes)
NORMAL_LOAD=500         # 500 transactions per test
PEAK_LOAD=2000          # 2K transactions per test
BURST_SIZE=100          # Send 100 txs per burst
BURST_DELAY=0.1         # 100ms between bursts

# For higher load test, use --full-load flag (~20-30 minutes)
FULL_LOAD=false
HIGH_LOAD=false

if [ "$1" = "--full-load" ] || [ "$2" = "--full-load" ] || [ "$3" = "--full-load" ]; then
    FULL_LOAD=true
    NORMAL_LOAD=2000    # 2K for normal
    PEAK_LOAD=5000      # 5K for peak (total ~28K across all tests)
fi

# For 100K+ transactions, use --high-load flag (~60-90 minutes)
if [ "$1" = "--high-load" ] || [ "$2" = "--high-load" ] || [ "$3" = "--high-load" ] || [ "$4" = "--high-load" ]; then
    HIGH_LOAD=true
    NORMAL_LOAD=10000   # 10K for normal
    PEAK_LOAD=15000     # 15K for peak (total ~100K across all tests)
fi

# Get number of CPU cores for optimal worker count
NUM_CPUS=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo "8")

# Use max workers if flag set, otherwise half of CPUs (min 4)
if [ "$MAX_WORKERS" = true ]; then
    PREWARM_WORKERS=$NUM_CPUS
else
    PREWARM_WORKERS=$(( (NUM_CPUS / 2) > 4 ? (NUM_CPUS / 2) : 4 ))
fi

clear
echo ""
echo -e "${BOLD}${MAGENTA}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${MAGENTA}║       UNIFIED PRE-WARMING BENCHMARK SUITE                                    ║${NC}"
echo -e "${BOLD}${MAGENTA}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo -e "  Date: $(date '+%Y-%m-%d %H:%M:%S')"
if [ "$HIGH_LOAD" = true ]; then
    echo -e "  ${BOLD}${RED}MODE: HIGH LOAD (~100K transactions) - Est. 60-90 minutes${NC}"
elif [ "$FULL_LOAD" = true ]; then
    echo -e "  ${BOLD}MODE: FULL LOAD (~28K transactions)${NC}"
fi
echo -e "  Normal Load: ${NORMAL_LOAD} txs | Peak Load: ${PEAK_LOAD} txs"
echo -e "  CPUs: ${NUM_CPUS} | Pre-warming Workers: ${PREWARM_WORKERS}"
TOTAL_TXS=$((NORMAL_LOAD * 4 + PEAK_LOAD * 4))  # 4 tests each
echo -e "  Total Transactions: ~${TOTAL_TXS} (across all tests)"
echo ""

#-------------------------------------------------------------------------------
# REBUILD (if requested)
#-------------------------------------------------------------------------------
if [ "$REBUILD" = true ]; then
    echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
    echo -e "${BOLD}  CLEANUP & REBUILD${NC}"
    echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
    echo ""

    cd "$RETH_DIR"

    # Clean up old data directories from previous benchmark runs
    echo -e "  ${CYAN}Cleaning old data directories...${NC}"
    rm -rf "$RETH_DIR"/.unified-benchmark-* 2>/dev/null
    rm -rf "$RETH_DIR"/.erc20-benchmark-* 2>/dev/null
    rm -rf "$RETH_DIR"/.erc20-test-* 2>/dev/null
    rm -rf "$RETH_DIR"/.op-reth-* 2>/dev/null
    rm -rf "$RETH_DIR"/.test-* 2>/dev/null
    rm -rf "$RETH_DIR"/.debug-* 2>/dev/null
    echo -e "  ${GREEN}✓${NC} Old data directories cleaned"

    # Rebuild binary
    echo -e "  ${CYAN}Rebuilding op-reth with pre-warming feature...${NC}"
    if cargo build --release --package op-reth --features pre-warming 2>&1 | tail -5; then
        echo -e "  ${GREEN}✓${NC} Build successful"
    else
        echo -e "  ${RED}✗${NC} Build failed!"
        exit 1
    fi
    echo ""
fi

#-------------------------------------------------------------------------------
# SECTION 1: ETH TRANSFERS
#-------------------------------------------------------------------------------
echo -e "\n${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  SECTION 1: ETH TRANSFERS${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"

echo -e "\n  ${CYAN}Normal Load (${NORMAL_LOAD} txs):${NC}"
RESULT=$(run_test "ETH Normal" "eth" $NORMAL_LOAD "disabled" 2>/dev/null | tail -1)
ETH_NORMAL_OFF_TPS=$(echo $RESULT | awk '{print $1}')

RESULT=$(run_test "ETH Normal" "eth" $NORMAL_LOAD "enabled" 2>/dev/null | tail -1)
ETH_NORMAL_ON_TPS=$(echo $RESULT | awk '{print $1}')
ETH_NORMAL_ON_HIT=$(echo $RESULT | awk '{print $2}')

echo -e "\n  ${CYAN}Peak Load (${PEAK_LOAD} txs):${NC}"
RESULT=$(run_test "ETH Peak" "eth" $PEAK_LOAD "disabled" 2>/dev/null | tail -1)
ETH_PEAK_OFF_TPS=$(echo $RESULT | awk '{print $1}')

RESULT=$(run_test "ETH Peak" "eth" $PEAK_LOAD "enabled" 2>/dev/null | tail -1)
ETH_PEAK_ON_TPS=$(echo $RESULT | awk '{print $1}')
ETH_PEAK_ON_HIT=$(echo $RESULT | awk '{print $2}')

#-------------------------------------------------------------------------------
# SECTION 2: ERC20 OPERATIONS
#-------------------------------------------------------------------------------
echo -e "\n${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  SECTION 2: ERC20 OPERATIONS${NC}"
echo -e "${BOLD}${BLUE}══════════════════════════════════════════════════════════════════════════════${NC}"

echo -e "\n  ${CYAN}ERC20 transfer() - Normal Load:${NC}"
RESULT=$(run_test "ERC20 transfer Normal" "erc20_transfer" $NORMAL_LOAD "disabled" 2>/dev/null | tail -1)
ERC20_TRANSFER_NORMAL_OFF_TPS=$(echo $RESULT | awk '{print $1}')

RESULT=$(run_test "ERC20 transfer Normal" "erc20_transfer" $NORMAL_LOAD "enabled" 2>/dev/null | tail -1)
ERC20_TRANSFER_NORMAL_ON_TPS=$(echo $RESULT | awk '{print $1}')
ERC20_TRANSFER_NORMAL_ON_HIT=$(echo $RESULT | awk '{print $2}')
ERC20_TRANSFER_NORMAL_ON_STORAGE=$(echo $RESULT | awk '{print $3}')

echo -e "\n  ${CYAN}ERC20 transfer() - Peak Load:${NC}"
RESULT=$(run_test "ERC20 transfer Peak" "erc20_transfer" $PEAK_LOAD "disabled" 2>/dev/null | tail -1)
ERC20_TRANSFER_PEAK_OFF_TPS=$(echo $RESULT | awk '{print $1}')

RESULT=$(run_test "ERC20 transfer Peak" "erc20_transfer" $PEAK_LOAD "enabled" 2>/dev/null | tail -1)
ERC20_TRANSFER_PEAK_ON_TPS=$(echo $RESULT | awk '{print $1}')
ERC20_TRANSFER_PEAK_ON_HIT=$(echo $RESULT | awk '{print $2}')
ERC20_TRANSFER_PEAK_ON_STORAGE=$(echo $RESULT | awk '{print $3}')

echo -e "\n  ${CYAN}ERC20 transferFrom() - Normal Load:${NC}"
RESULT=$(run_test "ERC20 transferFrom Normal" "erc20_transferFrom" $NORMAL_LOAD "disabled" 2>/dev/null | tail -1)
ERC20_TRANSFERFROM_NORMAL_OFF_TPS=$(echo $RESULT | awk '{print $1}')

RESULT=$(run_test "ERC20 transferFrom Normal" "erc20_transferFrom" $NORMAL_LOAD "enabled" 2>/dev/null | tail -1)
ERC20_TRANSFERFROM_NORMAL_ON_TPS=$(echo $RESULT | awk '{print $1}')
ERC20_TRANSFERFROM_NORMAL_ON_HIT=$(echo $RESULT | awk '{print $2}')

echo -e "\n  ${CYAN}ERC20 transferFrom() - Peak Load:${NC}"
RESULT=$(run_test "ERC20 transferFrom Peak" "erc20_transferFrom" $PEAK_LOAD "disabled" 2>/dev/null | tail -1)
ERC20_TRANSFERFROM_PEAK_OFF_TPS=$(echo $RESULT | awk '{print $1}')

RESULT=$(run_test "ERC20 transferFrom Peak" "erc20_transferFrom" $PEAK_LOAD "enabled" 2>/dev/null | tail -1)
ERC20_TRANSFERFROM_PEAK_ON_TPS=$(echo $RESULT | awk '{print $1}')
ERC20_TRANSFERFROM_PEAK_ON_HIT=$(echo $RESULT | awk '{print $2}')

#-------------------------------------------------------------------------------
# Calculate improvements
#-------------------------------------------------------------------------------
calc_change() {
    python3 -c "
off = float('$1' or 0)
on = float('$2' or 0)
if off > 0:
    pct = ((on - off) / off) * 100
    if pct > 0:
        print(f'+{pct:.1f}%')
    else:
        print(f'{pct:.1f}%')
elif on > 0:
    print('+∞')  # Improved from 0 baseline
else:
    print('-')   # Both are 0
"
}

ETH_NORMAL_CHANGE=$(calc_change "$ETH_NORMAL_OFF_TPS" "$ETH_NORMAL_ON_TPS")
ETH_PEAK_CHANGE=$(calc_change "$ETH_PEAK_OFF_TPS" "$ETH_PEAK_ON_TPS")
ERC20_TRANSFER_NORMAL_CHANGE=$(calc_change "$ERC20_TRANSFER_NORMAL_OFF_TPS" "$ERC20_TRANSFER_NORMAL_ON_TPS")
ERC20_TRANSFER_PEAK_CHANGE=$(calc_change "$ERC20_TRANSFER_PEAK_OFF_TPS" "$ERC20_TRANSFER_PEAK_ON_TPS")
ERC20_TRANSFERFROM_NORMAL_CHANGE=$(calc_change "$ERC20_TRANSFERFROM_NORMAL_OFF_TPS" "$ERC20_TRANSFERFROM_NORMAL_ON_TPS")
ERC20_TRANSFERFROM_PEAK_CHANGE=$(calc_change "$ERC20_TRANSFERFROM_PEAK_OFF_TPS" "$ERC20_TRANSFERFROM_PEAK_ON_TPS")

#-------------------------------------------------------------------------------
# GENERATE MARKDOWN REPORT
#-------------------------------------------------------------------------------

# Determine winners (highlight best TPS)
ETH_WINNER=""
if python3 -c "exit(0 if float('$ETH_PEAK_ON_TPS' or 0) > float('$ETH_PEAK_OFF_TPS' or 0) else 1)" 2>/dev/null; then
    ETH_WINNER=""
fi

ERC20_TRANSFER_WINNER=""
if python3 -c "exit(0 if float('$ERC20_TRANSFER_PEAK_ON_TPS' or 0) > float('$ERC20_TRANSFER_PEAK_OFF_TPS' or 0) else 1)" 2>/dev/null; then
    ERC20_TRANSFER_WINNER=""
fi

ERC20_TRANSFERFROM_WINNER=""
if python3 -c "exit(0 if float('$ERC20_TRANSFERFROM_PEAK_ON_TPS' or 0) > float('$ERC20_TRANSFERFROM_PEAK_OFF_TPS' or 0) else 1)" 2>/dev/null; then
    ERC20_TRANSFERFROM_WINNER=""
fi

# Generate markdown using Python to avoid shell escaping issues
TOTAL_TXS_ACTUAL=$((NORMAL_LOAD * 4 + PEAK_LOAD * 4))
python3 << PYEOF
report = f"""# Pre-Warming Benchmark Report

**Generated:** $(date '+%Y-%m-%d %H:%M:%S')

---

## Test Configuration

| Parameter | Value |
|-----------|-------|
| Normal Load | ${NORMAL_LOAD} transactions |
| Peak Load | ${PEAK_LOAD} transactions |
| Pre-warming Workers | ${PREWARM_WORKERS} |
| Total Transactions | ~${TOTAL_TXS_ACTUAL} |
| Block Time | 1 second |

---

## Section 1: ETH Transfers

Simple ETH value transfers between accounts.

### Results

| Load | Pre-warming | TPS | Cache Hit Rate | TPS Change |
|------|-------------|-----|----------------|------------|
| Normal (${NORMAL_LOAD} txs) | OFF | ${ETH_NORMAL_OFF_TPS} | - | baseline |
| Normal (${NORMAL_LOAD} txs) | ON | ${ETH_NORMAL_ON_TPS} | ${ETH_NORMAL_ON_HIT}% | **${ETH_NORMAL_CHANGE}** |
| Peak (${PEAK_LOAD} txs) | OFF | ${ETH_PEAK_OFF_TPS} | - | baseline |
| Peak (${PEAK_LOAD} txs) | ON | ${ETH_PEAK_ON_TPS} | ${ETH_PEAK_ON_HIT}% | **${ETH_PEAK_CHANGE}** |

---

## Section 2: ERC20 Operations

ERC20 token operations with storage slot pre-warming.

### ERC20 transfer()

| Load | Pre-warming | TPS | Cache Hit Rate | Storage Slots | TPS Change |
|------|-------------|-----|----------------|---------------|------------|
| Normal (${NORMAL_LOAD} txs) | OFF | ${ERC20_TRANSFER_NORMAL_OFF_TPS} | - | - | baseline |
| Normal (${NORMAL_LOAD} txs) | ON | ${ERC20_TRANSFER_NORMAL_ON_TPS} | ${ERC20_TRANSFER_NORMAL_ON_HIT}% | ${ERC20_TRANSFER_NORMAL_ON_STORAGE} | **${ERC20_TRANSFER_NORMAL_CHANGE}** |
| Peak (${PEAK_LOAD} txs) | OFF | ${ERC20_TRANSFER_PEAK_OFF_TPS} | - | - | baseline |
| Peak (${PEAK_LOAD} txs) | ON | ${ERC20_TRANSFER_PEAK_ON_TPS} | ${ERC20_TRANSFER_PEAK_ON_HIT}% | ${ERC20_TRANSFER_PEAK_ON_STORAGE} | **${ERC20_TRANSFER_PEAK_CHANGE}** |

### ERC20 transferFrom()

| Load | Pre-warming | TPS | Cache Hit Rate | TPS Change |
|------|-------------|-----|----------------|------------|
| Normal (${NORMAL_LOAD} txs) | OFF | ${ERC20_TRANSFERFROM_NORMAL_OFF_TPS} | - | baseline |
| Normal (${NORMAL_LOAD} txs) | ON | ${ERC20_TRANSFERFROM_NORMAL_ON_TPS} | ${ERC20_TRANSFERFROM_NORMAL_ON_HIT}% | **${ERC20_TRANSFERFROM_NORMAL_CHANGE}** |
| Peak (${PEAK_LOAD} txs) | OFF | ${ERC20_TRANSFERFROM_PEAK_OFF_TPS} | - | baseline |
| Peak (${PEAK_LOAD} txs) | ON | ${ERC20_TRANSFERFROM_PEAK_ON_TPS} | ${ERC20_TRANSFERFROM_PEAK_ON_HIT}% | **${ERC20_TRANSFERFROM_PEAK_CHANGE}** |

---

## Summary

### Key Findings

| Test Type | Best TPS (Pre-warming ON) | Best Cache Hit Rate | TPS Improvement |
|-----------|---------------------------|---------------------|-----------------|
| ETH Transfers | ${ETH_PEAK_ON_TPS} TPS | ${ETH_PEAK_ON_HIT}% | **${ETH_PEAK_CHANGE}** |
| ERC20 transfer() | ${ERC20_TRANSFER_PEAK_ON_TPS} TPS | ${ERC20_TRANSFER_PEAK_ON_HIT}% | **${ERC20_TRANSFER_PEAK_CHANGE}** |
| ERC20 transferFrom() | ${ERC20_TRANSFERFROM_PEAK_ON_TPS} TPS | ${ERC20_TRANSFERFROM_PEAK_ON_HIT}% | **${ERC20_TRANSFERFROM_PEAK_CHANGE}** |

### Pre-warming Detection

The simulator detects and pre-warms storage slots for:

- **transfer(address,uint256)**: \`balances[sender]\`, \`balances[to]\`
- **transferFrom(address,address,uint256)**: \`balances[from]\`, \`balances[to]\`, \`allowances[from][sender]\`
- **approve(address,uint256)**: \`allowances[sender][spender]\`

---

*Report generated by unified_benchmark.sh*
"""
with open("$REPORT_FILE", "w") as f:
    f.write(report)
PYEOF

#-------------------------------------------------------------------------------
# PRINT SUMMARY TO CONSOLE
#-------------------------------------------------------------------------------
echo ""
echo -e "${BOLD}${MAGENTA}══════════════════════════════════════════════════════════════════════════════${NC}"
echo -e "${BOLD}  BENCHMARK RESULTS${NC}"
echo -e "${BOLD}${MAGENTA}══════════════════════════════════════════════════════════════════════════════${NC}"

echo ""
echo -e "${BOLD}  ETH TRANSFERS${NC}"
echo -e "  ┌─────────────┬──────────────┬──────────────┬───────────┬──────────┐"
echo -e "  │ Load        │ Pre-warm OFF │ Pre-warm ON  │ Hit Rate  │ Change   │"
echo -e "  ├─────────────┼──────────────┼──────────────┼───────────┼──────────┤"
echo -e "  │ Normal      │ ${ETH_NORMAL_OFF_TPS} TPS     │ ${ETH_NORMAL_ON_TPS} TPS     │ ${ETH_NORMAL_ON_HIT}%       │ ${ETH_NORMAL_CHANGE}    │"
echo -e "  │ Peak        │ ${ETH_PEAK_OFF_TPS} TPS     │ ${ETH_PEAK_ON_TPS} TPS     │ ${ETH_PEAK_ON_HIT}%       │ ${ETH_PEAK_CHANGE}    │"
echo -e "  └─────────────┴──────────────┴──────────────┴───────────┴──────────┘"

echo ""
echo -e "${BOLD}  ERC20 OPERATIONS${NC}"
echo -e "  ┌─────────────────────┬──────────────┬──────────────┬───────────┬──────────┐"
echo -e "  │ Operation           │ Pre-warm OFF │ Pre-warm ON  │ Hit Rate  │ Change   │"
echo -e "  ├─────────────────────┼──────────────┼──────────────┼───────────┼──────────┤"
echo -e "  │ transfer() Normal   │ ${ERC20_TRANSFER_NORMAL_OFF_TPS} TPS     │ ${ERC20_TRANSFER_NORMAL_ON_TPS} TPS     │ ${ERC20_TRANSFER_NORMAL_ON_HIT}%       │ ${ERC20_TRANSFER_NORMAL_CHANGE}    │"
echo -e "  │ transfer() Peak     │ ${ERC20_TRANSFER_PEAK_OFF_TPS} TPS     │ ${ERC20_TRANSFER_PEAK_ON_TPS} TPS     │ ${ERC20_TRANSFER_PEAK_ON_HIT}%       │ ${ERC20_TRANSFER_PEAK_CHANGE}    │"
echo -e "  │ transferFrom() Norm │ ${ERC20_TRANSFERFROM_NORMAL_OFF_TPS} TPS     │ ${ERC20_TRANSFERFROM_NORMAL_ON_TPS} TPS     │ ${ERC20_TRANSFERFROM_NORMAL_ON_HIT}%       │ ${ERC20_TRANSFERFROM_NORMAL_CHANGE}    │"
echo -e "  │ transferFrom() Peak │ ${ERC20_TRANSFERFROM_PEAK_OFF_TPS} TPS     │ ${ERC20_TRANSFERFROM_PEAK_ON_TPS} TPS     │ ${ERC20_TRANSFERFROM_PEAK_ON_HIT}%       │ ${ERC20_TRANSFERFROM_PEAK_CHANGE}    │"
echo -e "  └─────────────────────┴──────────────┴──────────────┴───────────┴──────────┘"

echo ""
echo -e "${GREEN}✅ Markdown report saved to: ${REPORT_FILE}${NC}"
echo -e "${YELLOW}📋 Error log saved to: ${ERROR_LOG}${NC}"
echo -e "   Copy the content for your Lark doc!"
echo ""

# Show error summary if any errors occurred
ERROR_COUNT=$(grep -c "\[ERROR\]" "$ERROR_LOG" 2>/dev/null || echo "0")
if [ "$ERROR_COUNT" -gt 0 ]; then
    echo -e "${YELLOW}⚠️  $ERROR_COUNT error(s) logged. Check: $ERROR_LOG${NC}"
    echo ""
fi

# Cleanup
rm -rf "$LOG_DIR"

echo -e "${BOLD}${GREEN}Benchmark complete!${NC}"
