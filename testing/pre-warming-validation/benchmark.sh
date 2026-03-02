#!/bin/zsh
#===============================================================================
#  PRE-WARMING COMPREHENSIVE BENCHMARK
#===============================================================================
#  This script performs complete benchmarking of the pre-warming feature:
#  - Compares pre-warming ON vs OFF
#  - Real-world L2 production load (XLayer/Optimism compatible)
#  - Captures all metrics (TPS, cache hits/misses, simulations)
#  - Generates detailed performance report
#
#  L2 Production Load Profile:
#  - Target: 100-500 TPS (Optimism/Base/XLayer typical range)
#  - Block Time: 1-2 seconds
#  - Transactions per block: 50-200
#===============================================================================

set -e

# Configuration - Real-world L2 Load Profile
SCRIPT_DIR="${0:A:h}"
RETH_DIR="${SCRIPT_DIR}/../.."

# Load Profile Presets
PROFILE=${1:-"standard"}  # Options: light, standard, heavy, stress

case $PROFILE in
    light)
        BLOCK_TIME=2
        BURSTS=5
        TXS_PER_BURST=20
        BURST_INTERVAL=2
        ;;
    standard)
        # ~100 TPS target (typical L2 load)
        BLOCK_TIME=1
        BURSTS=10
        TXS_PER_BURST=50
        BURST_INTERVAL=1
        ;;
    heavy)
        # ~200 TPS target (high L2 load)
        BLOCK_TIME=1
        BURSTS=15
        TXS_PER_BURST=100
        BURST_INTERVAL=1
        ;;
    stress)
        # ~500 TPS target (stress test / peak load)
        BLOCK_TIME=1
        BURSTS=20
        TXS_PER_BURST=200
        BURST_INTERVAL=1
        ;;
    ultra)
        # ~2000 TPS target (extreme load test)
        BLOCK_TIME=1
        BURSTS=25
        TXS_PER_BURST=500
        BURST_INTERVAL=0.5
        ;;
    *)
        echo "Unknown profile: $PROFILE"
        echo "Usage: $0 [light|standard|heavy|stress|ultra]"
        exit 1
        ;;
esac

TOTAL_TXS=$((BURSTS * TXS_PER_BURST))
TARGET_TPS=$(python3 -c "print(int($TXS_PER_BURST / $BURST_INTERVAL))" 2>/dev/null || echo "$TXS_PER_BURST")

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Private key for dev account
PRIVATE_KEY="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"

# Results storage (using individual variables)
OFF_SUCCESS=0
OFF_FAILED=0
OFF_DURATION=0
OFF_TPS=0

ON_SUCCESS=0
ON_FAILED=0
ON_DURATION=0
ON_TPS=0
ON_SIMULATIONS=0
ON_PREFETCH=0
ON_PREFETCH_ACCOUNTS=0
ON_PREFETCH_STORAGE=0
ON_CACHE_ENTRIES=0
ON_CACHE_KEYS=0
ON_HITS=0
ON_MISSES=0
ON_HIT_RATE=0

#===============================================================================
# Helper Functions
#===============================================================================

print_header() {
    echo ""
    echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}${BLUE}║${NC}  ${BOLD}$1${NC}"
    echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"
}

print_section() {
    echo ""
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BOLD}  $1${NC}"
    echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

cleanup() {
    pkill -9 op-reth 2>/dev/null || true
    sleep 2
}

get_metric() {
    local METRIC=$1
    curl -s http://localhost:9001/metrics 2>/dev/null | grep "^${METRIC} " | awk '{print $2}' | cut -d'.' -f1 || echo "0"
}

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

#===============================================================================
# Transaction Sender (Python)
#===============================================================================

send_transactions() {
    local COUNT=$1
    local START_NONCE=$2

    python3 << PYEOF
import requests
import sys
from eth_account import Account

pk = "0x${PRIVATE_KEY}"
account = Account.from_key(pk)
success = 0
failed = 0
errors = []

for i in range($COUNT):
    nonce = $START_NONCE + i
    tx = {
        'nonce': nonce,
        'to': '0x70997970C51812dc3A010C7d01b50e0d17dc79C8',
        'value': 10000000000000000,
        'gas': 21000,
        'gasPrice': 1000000000,
        'chainId': 1337,
    }

    try:
        signed = account.sign_transaction(tx)
        raw_tx = '0x' + signed.raw_transaction.hex()

        response = requests.post(
            'http://localhost:8545',
            json={
                'jsonrpc': '2.0',
                'method': 'eth_sendRawTransaction',
                'params': [raw_tx],
                'id': i + 1
            },
            timeout=10
        )

        result = response.json()
        if 'result' in result:
            success += 1
        else:
            failed += 1
            errors.append(result.get('error', {}).get('message', 'unknown'))
    except Exception as e:
        failed += 1
        errors.append(str(e))

print(f"SUCCESS:{success}")
print(f"FAILED:{failed}")
if errors:
    unique_errors = list(set(errors))[:3]
    print(f"ERRORS:{';'.join(unique_errors)}")
PYEOF
}

get_nonce() {
    local RESULT=$(curl -s http://localhost:8545 -X POST \
        -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_getTransactionCount","params":["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","pending"],"id":1}' 2>/dev/null)
    local HEX=$(echo "$RESULT" | grep -o '"result":"0x[^"]*"' | cut -d'"' -f4)
    printf "%d" "$HEX" 2>/dev/null || echo "0"
}

#===============================================================================
# Run Benchmark
#===============================================================================

run_benchmark() {
    local MODE=$1  # "enabled" or "disabled"
    local DATADIR="$RETH_DIR/.benchmark-$MODE-$(date +%s)"

    print_section "Running Benchmark: Pre-warming $MODE"

    # Start node
    echo -e "  ${BLUE}Starting op-reth node...${NC}"
    rm -rf "$DATADIR" 2>/dev/null

    if [ "$MODE" = "enabled" ]; then
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

    if ! wait_for_node; then
        echo -e "  ${RED}✗ Failed to start node${NC}"
        return 1
    fi
    echo -e "  ${GREEN}✓ Node started${NC}"

    # Capture baseline metrics
    local BASELINE_SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
    local BASELINE_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
    local BASELINE_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
    local BASELINE_PREFETCH=$(get_metric "reth_txpool_pre_warming_prefetch_operations")

    # Run transaction bursts
    echo ""
    echo -e "  ${BLUE}Sending $BURSTS bursts × $TXS_PER_BURST txs = $TOTAL_TXS total transactions${NC}"
    echo ""

    local TOTAL_SUCCESS=0
    local TOTAL_FAILED=0
    local START_TIME=$(python3 -c "import time; print(time.time())")

    for burst in $(seq 1 $BURSTS); do
        local NONCE=$(get_nonce)
        echo -ne "  Burst ${burst}/${BURSTS} (nonce=${NONCE}): "

        local RESULT=$(send_transactions $TXS_PER_BURST $NONCE 2>/dev/null)
        local BURST_SUCCESS=$(echo "$RESULT" | grep "SUCCESS:" | cut -d: -f2)
        local BURST_FAILED=$(echo "$RESULT" | grep "FAILED:" | cut -d: -f2)

        BURST_SUCCESS=${BURST_SUCCESS:-0}
        BURST_FAILED=${BURST_FAILED:-0}

        TOTAL_SUCCESS=$((TOTAL_SUCCESS + BURST_SUCCESS))
        TOTAL_FAILED=$((TOTAL_FAILED + BURST_FAILED))

        if [ "$BURST_FAILED" -eq 0 ]; then
            echo -e "${GREEN}${BURST_SUCCESS}/${TXS_PER_BURST} ✓${NC}"
        else
            echo -e "${YELLOW}${BURST_SUCCESS}/${TXS_PER_BURST} (${BURST_FAILED} failed)${NC}"
        fi

        if [ $burst -lt $BURSTS ]; then
            sleep $BURST_INTERVAL
        fi
    done

    # Wait for blocks to be mined
    echo ""
    echo -e "  ${BLUE}Waiting for blocks to finalize...${NC}"
    sleep $((BLOCK_TIME * 3))

    local END_TIME=$(python3 -c "import time; print(time.time())")
    local DURATION=$(python3 -c "print(round($END_TIME - $START_TIME, 2))")
    local TPS=$(python3 -c "print(round($TOTAL_SUCCESS / max($DURATION, 0.001), 2))")

    # Capture final metrics
    local FINAL_SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
    local FINAL_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
    local FINAL_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
    local FINAL_PREFETCH=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
    local FINAL_ACCOUNTS=$(get_metric "reth_txpool_pre_warming_prefetch_accounts")
    local FINAL_STORAGE=$(get_metric "reth_txpool_pre_warming_prefetch_storage_slots")
    local CACHE_ENTRIES=$(get_metric "reth_txpool_pre_warming_cache_entries")
    local CACHE_KEYS=$(get_metric "reth_txpool_pre_warming_cache_keys_total")

    # Calculate deltas
    local SIMS=$((FINAL_SIMS - BASELINE_SIMS))
    local HITS=$((FINAL_HITS - BASELINE_HITS))
    local MISSES=$((FINAL_MISSES - BASELINE_MISSES))
    local PREFETCH=$((FINAL_PREFETCH - BASELINE_PREFETCH))
    local TOTAL_ACCESS=$((HITS + MISSES))
    local HIT_RATE=0
    if [ $TOTAL_ACCESS -gt 0 ]; then
        HIT_RATE=$((HITS * 100 / TOTAL_ACCESS))
    fi

    # Store results
    if [ "$MODE" = "enabled" ]; then
        ON_SUCCESS=$TOTAL_SUCCESS
        ON_FAILED=$TOTAL_FAILED
        ON_DURATION=$DURATION
        ON_TPS=$TPS
        ON_SIMULATIONS=$SIMS
        ON_PREFETCH=$PREFETCH
        ON_PREFETCH_ACCOUNTS=$FINAL_ACCOUNTS
        ON_PREFETCH_STORAGE=$FINAL_STORAGE
        ON_CACHE_ENTRIES=$CACHE_ENTRIES
        ON_CACHE_KEYS=$CACHE_KEYS
        ON_HITS=$HITS
        ON_MISSES=$MISSES
        ON_HIT_RATE=$HIT_RATE
    else
        OFF_SUCCESS=$TOTAL_SUCCESS
        OFF_FAILED=$TOTAL_FAILED
        OFF_DURATION=$DURATION
        OFF_TPS=$TPS
    fi

    echo -e "  ${GREEN}✓ Benchmark complete: ${TOTAL_SUCCESS}/${TOTAL_TXS} txs, ${TPS} TPS${NC}"

    # Cleanup
    cleanup
    rm -rf "$DATADIR" 2>/dev/null
}

#===============================================================================
# Generate Report
#===============================================================================

generate_report() {
    local TPS_CHANGE="N/A"
    if [ "$OFF_TPS" != "0" ] && [ -n "$OFF_TPS" ]; then
        TPS_CHANGE=$(python3 -c "print(round((($ON_TPS - $OFF_TPS) / $OFF_TPS) * 100, 1))" 2>/dev/null || echo "N/A")
    fi

    echo ""
    echo ""
    echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}${BLUE}║${NC}                    ${BOLD}PRE-WARMING BENCHMARK REPORT${NC}                              ${BOLD}${BLUE}║${NC}"
    echo -e "${BOLD}${BLUE}╠══════════════════════════════════════════════════════════════════════════════╣${NC}"
    echo -e "${BOLD}${BLUE}║${NC}  Date: $(date '+%Y-%m-%d %H:%M:%S')                                               ${BOLD}${BLUE}║${NC}"
    echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"

    echo ""
    echo -e "${CYAN}┌──────────────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}TEST CONFIGURATION${NC}  (Profile: ${CYAN}${(U)PROFILE}${NC})                                      ${CYAN}│${NC}"
    echo -e "${CYAN}├──────────────────────────────────────────────────────────────────────────────┤${NC}"
    printf "${CYAN}│${NC}  %-20s ${BOLD}%-10s${NC}                                             ${CYAN}│${NC}\n" "Block Time:" "${BLOCK_TIME}s"
    printf "${CYAN}│${NC}  %-20s ${BOLD}%-10s${NC}                                             ${CYAN}│${NC}\n" "Total Bursts:" "${BURSTS}"
    printf "${CYAN}│${NC}  %-20s ${BOLD}%-10s${NC}                                             ${CYAN}│${NC}\n" "TXs per Burst:" "${TXS_PER_BURST}"
    printf "${CYAN}│${NC}  %-20s ${BOLD}%-10s${NC}                                             ${CYAN}│${NC}\n" "Burst Interval:" "${BURST_INTERVAL}s"
    printf "${CYAN}│${NC}  %-20s ${BOLD}%-10s${NC}                                             ${CYAN}│${NC}\n" "Total TXs:" "${TOTAL_TXS}"
    printf "${CYAN}│${NC}  %-20s ${BOLD}%-10s${NC}                                             ${CYAN}│${NC}\n" "Target TPS:" "~${TARGET_TPS}"
    echo -e "${CYAN}└──────────────────────────────────────────────────────────────────────────────┘${NC}"

    echo ""
    echo -e "${CYAN}┌──────────────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}PERFORMANCE COMPARISON${NC}                                                       ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────┬─────────────────────┬────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}  Metric                ${CYAN}│${NC}  Pre-warming OFF    ${CYAN}│${NC}  Pre-warming ON            ${CYAN}│${NC}"
    echo -e "${CYAN}├─────────────────────────┼─────────────────────┼────────────────────────────┤${NC}"
    printf "${CYAN}│${NC}  %-22s ${CYAN}│${NC}  %-18s ${CYAN}│${NC}  %-25s ${CYAN}│${NC}\n" "TXs Succeeded" "${OFF_SUCCESS}/${TOTAL_TXS}" "${ON_SUCCESS}/${TOTAL_TXS}"
    printf "${CYAN}│${NC}  %-22s ${CYAN}│${NC}  %-18s ${CYAN}│${NC}  %-25s ${CYAN}│${NC}\n" "TXs Failed" "${OFF_FAILED}" "${ON_FAILED}"
    printf "${CYAN}│${NC}  %-22s ${CYAN}│${NC}  %-18s ${CYAN}│${NC}  %-25s ${CYAN}│${NC}\n" "Duration" "${OFF_DURATION}s" "${ON_DURATION}s"
    printf "${CYAN}│${NC}  %-22s ${CYAN}│${NC}  ${BOLD}%-18s${NC} ${CYAN}│${NC}  ${BOLD}%-25s${NC} ${CYAN}│${NC}\n" "TPS" "${OFF_TPS}" "${ON_TPS}"
    echo -e "${CYAN}└─────────────────────────┴─────────────────────┴────────────────────────────┘${NC}"

    echo ""
    if [[ "$TPS_CHANGE" != "N/A" ]]; then
        if (( $(echo "$TPS_CHANGE > 0" | bc -l 2>/dev/null || echo 0) )); then
            echo -e "  📈 TPS Change: ${GREEN}${BOLD}+${TPS_CHANGE}%${NC}"
        elif (( $(echo "$TPS_CHANGE < 0" | bc -l 2>/dev/null || echo 0) )); then
            echo -e "  📉 TPS Change: ${YELLOW}${BOLD}${TPS_CHANGE}%${NC}"
        else
            echo -e "  📊 TPS Change: ${BOLD}${TPS_CHANGE}%${NC}"
        fi
    fi

    echo ""
    echo -e "${CYAN}┌──────────────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}PRE-WARMING METRICS${NC} (Pre-warming ON only)                                   ${CYAN}│${NC}"
    echo -e "${CYAN}├──────────────────────────────────────────────────────────────────────────────┤${NC}"
    echo -e "${CYAN}│${NC}                                                                              ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}Simulation Statistics${NC}                                                       ${CYAN}│${NC}"
    printf "${CYAN}│${NC}    ├─ Simulations Completed:    %-10s                                 ${CYAN}│${NC}\n" "${ON_SIMULATIONS}"
    printf "${CYAN}│${NC}    ├─ Cache Entries:            %-10s                                 ${CYAN}│${NC}\n" "${ON_CACHE_ENTRIES}"
    printf "${CYAN}│${NC}    └─ Total Keys Cached:        %-10s                                 ${CYAN}│${NC}\n" "${ON_CACHE_KEYS}"
    echo -e "${CYAN}│${NC}                                                                              ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}Prefetch Statistics${NC}                                                         ${CYAN}│${NC}"
    printf "${CYAN}│${NC}    ├─ Prefetch Operations:      %-10s                                 ${CYAN}│${NC}\n" "${ON_PREFETCH}"
    printf "${CYAN}│${NC}    ├─ Accounts Prefetched:      %-10s                                 ${CYAN}│${NC}\n" "${ON_PREFETCH_ACCOUNTS}"
    printf "${CYAN}│${NC}    └─ Storage Slots Prefetched: %-10s                                 ${CYAN}│${NC}\n" "${ON_PREFETCH_STORAGE}"
    echo -e "${CYAN}│${NC}                                                                              ${CYAN}│${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}Cache Performance${NC}                                                           ${CYAN}│${NC}"
    printf "${CYAN}│${NC}    ├─ Cache Hits:               %-10s                                 ${CYAN}│${NC}\n" "${ON_HITS}"
    printf "${CYAN}│${NC}    ├─ Cache Misses:             %-10s                                 ${CYAN}│${NC}\n" "${ON_MISSES}"
    printf "${CYAN}│${NC}    ├─ Total Accesses:           %-10s                                 ${CYAN}│${NC}\n" "$((ON_HITS + ON_MISSES))"
    echo -e "${CYAN}│${NC}    │                                                                        ${CYAN}│${NC}"

    if [ "$ON_HIT_RATE" -ge 50 ]; then
        printf "${CYAN}│${NC}    └─ ${BOLD}Hit Rate:                 ${GREEN}%-10s${NC}                                 ${CYAN}│${NC}\n" "${ON_HIT_RATE}%"
    elif [ "$ON_HIT_RATE" -ge 25 ]; then
        printf "${CYAN}│${NC}    └─ ${BOLD}Hit Rate:                 ${YELLOW}%-10s${NC}                                 ${CYAN}│${NC}\n" "${ON_HIT_RATE}%"
    else
        printf "${CYAN}│${NC}    └─ ${BOLD}Hit Rate:                 ${RED}%-10s${NC}                                 ${CYAN}│${NC}\n" "${ON_HIT_RATE}%"
    fi
    echo -e "${CYAN}│${NC}                                                                              ${CYAN}│${NC}"
    echo -e "${CYAN}└──────────────────────────────────────────────────────────────────────────────┘${NC}"

    echo ""
    echo -e "${CYAN}┌──────────────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${CYAN}│${NC}  ${BOLD}VERDICT${NC}                                                                     ${CYAN}│${NC}"
    echo -e "${CYAN}├──────────────────────────────────────────────────────────────────────────────┤${NC}"

    local ALL_PASS=true

    # Check transaction success
    if [ "$ON_SUCCESS" -eq "$TOTAL_TXS" ] && [ "$OFF_SUCCESS" -eq "$TOTAL_TXS" ]; then
        echo -e "${CYAN}│${NC}  ${GREEN}✅ All transactions succeeded in both modes${NC}                               ${CYAN}│${NC}"
    else
        echo -e "${CYAN}│${NC}  ${YELLOW}⚠️  Some transactions failed${NC}                                              ${CYAN}│${NC}"
        ALL_PASS=false
    fi

    # Check simulations
    if [ "$ON_SIMULATIONS" -gt 0 ]; then
        echo -e "${CYAN}│${NC}  ${GREEN}✅ Pre-warming simulations executed (${ON_SIMULATIONS} completed)${NC}                      ${CYAN}│${NC}"
    else
        echo -e "${CYAN}│${NC}  ${RED}❌ No simulations completed${NC}                                               ${CYAN}│${NC}"
        ALL_PASS=false
    fi

    # Check prefetch
    if [ "$ON_PREFETCH" -gt 0 ]; then
        echo -e "${CYAN}│${NC}  ${GREEN}✅ Prefetch operations executed (${ON_PREFETCH} ops)${NC}                              ${CYAN}│${NC}"
    else
        echo -e "${CYAN}│${NC}  ${RED}❌ No prefetch operations${NC}                                                 ${CYAN}│${NC}"
        ALL_PASS=false
    fi

    # Check cache utilization
    if [ "$((ON_HITS + ON_MISSES))" -gt 0 ]; then
        echo -e "${CYAN}│${NC}  ${GREEN}✅ Cache being utilized (${ON_HIT_RATE}% hit rate)${NC}                                      ${CYAN}│${NC}"
    else
        echo -e "${CYAN}│${NC}  ${YELLOW}⚠️  No cache accesses recorded${NC}                                            ${CYAN}│${NC}"
    fi

    echo -e "${CYAN}│${NC}                                                                              ${CYAN}│${NC}"

    if [ "$ALL_PASS" = true ]; then
        echo -e "${CYAN}│${NC}  ${GREEN}${BOLD}🎉 BENCHMARK PASSED - Pre-warming feature is working correctly!${NC}           ${CYAN}│${NC}"
    else
        echo -e "${CYAN}│${NC}  ${YELLOW}${BOLD}⚠️  BENCHMARK COMPLETED WITH WARNINGS - Review results above${NC}              ${CYAN}│${NC}"
    fi

    echo -e "${CYAN}└──────────────────────────────────────────────────────────────────────────────┘${NC}"
    echo ""
}

#===============================================================================
# Main
#===============================================================================

main() {
    print_header "PRE-WARMING COMPREHENSIVE BENCHMARK"

    echo ""
    echo -e "  ${BOLD}Load Profile: ${CYAN}${(U)PROFILE}${NC}"
    echo -e "  ${BOLD}Configuration:${NC}"
    echo -e "    Block Time:     ${BLOCK_TIME}s"
    echo -e "    Bursts:         ${BURSTS}"
    echo -e "    TXs per Burst:  ${TXS_PER_BURST}"
    echo -e "    Burst Interval: ${BURST_INTERVAL}s"
    echo -e "    Total TXs:      ${TOTAL_TXS}"
    echo -e "    Target TPS:     ~${TARGET_TPS}"
    echo ""
    echo -e "  ${YELLOW}L2 Reference: Optimism/Base/XLayer target 100-2000+ TPS at peak${NC}"
    echo ""

    # Check for eth-account
    if ! python3 -c "import eth_account" 2>/dev/null; then
        echo -e "  ${YELLOW}Installing eth-account...${NC}"
        pip3 install eth-account requests -q 2>/dev/null
    fi

    # Check binary exists
    if [ ! -f "$RETH_DIR/target/release/op-reth" ]; then
        echo -e "  ${RED}Error: op-reth binary not found at $RETH_DIR/target/release/op-reth${NC}"
        echo -e "  ${YELLOW}Please build with: cargo build --release --package op-reth --features pre-warming${NC}"
        exit 1
    fi

    # Cleanup before starting
    cleanup

    # Run benchmarks
    run_benchmark "disabled"
    sleep 3
    run_benchmark "enabled"

    # Generate report
    generate_report
}

# Trap cleanup on exit
trap cleanup EXIT

# Run main
main "$@"

