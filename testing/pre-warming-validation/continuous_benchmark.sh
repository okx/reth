#!/bin/bash
#===============================================================================
#  CONTINUOUS BENCHMARK - Long-running load test
#===============================================================================
#  Runs continuous load against a SINGLE running node for extended duration.
#  Captures metrics at regular intervals for consistent comparison.
#
#  Usage:
#    ./continuous_benchmark.sh --duration 3600 --interval 30 --prewarm-on
#    ./continuous_benchmark.sh --duration 3600 --interval 30 --prewarm-off
#
#  Options:
#    --duration <seconds>   Total test duration (default: 3600 = 1 hour)
#    --interval <seconds>   Interval between metric captures (default: 30)
#    --prewarm-on           Run with pre-warming enabled
#    --prewarm-off          Run with pre-warming disabled
#    --burst <count>        Transactions per burst (default: 100)
#    --parallel             Run both modes simultaneously (separate nodes)
#===============================================================================

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RETH_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Default parameters
DURATION=3600        # 1 hour
INTERVAL=30          # 30 seconds between metric captures
BURST_SIZE=100       # Transactions per burst
PREWARM_MODE=""      # Must specify --prewarm-on or --prewarm-off
PARALLEL=false
BLOCK_TIME=1

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --duration)
            DURATION="$2"
            shift 2
            ;;
        --interval)
            INTERVAL="$2"
            shift 2
            ;;
        --burst)
            BURST_SIZE="$2"
            shift 2
            ;;
        --prewarm-on)
            PREWARM_MODE="on"
            shift
            ;;
        --prewarm-off)
            PREWARM_MODE="off"
            shift
            ;;
        --parallel)
            PARALLEL=true
            shift
            ;;
        --block-time)
            BLOCK_TIME="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Timestamp
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOG_DIR="$RETH_DIR/.continuous-benchmark-${TIMESTAMP}"
mkdir -p "$LOG_DIR"

# Dev accounts (Hardhat/Anvil default)
PRIVATE_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

RECIPIENTS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
    "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
    "0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc"
)

# ERC20 Calldata (same as unified_benchmark.sh)
# Pre-built transfer calldata to recipient 0x70997970c51812dc3a010c7d01b50e0d17dc79c8 for 1 token
ERC20_TRANSFER="0xa9059cbb00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c80000000000000000000000000000000000000000000000000de0b6b3a7640000"
# Hardcoded contract address (first contract deployed in dev mode)
CONTRACT_ADDR="0x5FbDB2315678afecb367f032d93F642f64180aa3"

# Metrics file
METRICS_FILE="$LOG_DIR/metrics_${PREWARM_MODE}.csv"
SUMMARY_FILE="$LOG_DIR/summary_${PREWARM_MODE}.txt"
REPORT_FILE="$RETH_DIR/continuous_benchmark_report.md"

# Arrays to accumulate metrics for averaging
declare -a TPS_SAMPLES=()
declare -a HIT_RATE_SAMPLES=()
declare -a DELTA_TPS_ON=()
declare -a DELTA_TPS_OFF=()
declare -a DELTA_HIT_RATES=()

#-------------------------------------------------------------------------------
# Functions
#-------------------------------------------------------------------------------

log() {
    echo -e "${CYAN}[$(date '+%H:%M:%S')]${NC} $1"
}

error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Calculate statistics from array
calc_stats() {
    local -n arr=$1
    local count=${#arr[@]}

    if [ "$count" -eq 0 ]; then
        echo "0|0|0|0|0"
        return
    fi

    local sum=0
    local min=${arr[0]}
    local max=${arr[0]}

    for val in "${arr[@]}"; do
        sum=$(echo "$sum + $val" | bc)
        if (( $(echo "$val < $min" | bc -l) )); then
            min=$val
        fi
        if (( $(echo "$val > $max" | bc -l) )); then
            max=$val
        fi
    done

    local avg=$(echo "scale=2; $sum / $count" | bc)

    # Calculate standard deviation
    local sq_diff_sum=0
    for val in "${arr[@]}"; do
        local diff=$(echo "$val - $avg" | bc)
        sq_diff_sum=$(echo "$sq_diff_sum + ($diff * $diff)" | bc)
    done
    local variance=$(echo "scale=4; $sq_diff_sum / $count" | bc)
    local stddev=$(echo "scale=2; sqrt($variance)" | bc 2>/dev/null || echo "0")

    echo "$avg|$min|$max|$stddev|$count"
}

# Calculate percentile from sorted array
calc_percentile() {
    local -n arr=$1
    local percentile=$2
    local count=${#arr[@]}

    if [ "$count" -eq 0 ]; then
        echo "0"
        return
    fi

    # Sort array
    IFS=$'\n' sorted=($(sort -n <<<"${arr[*]}")); unset IFS

    local index=$(echo "scale=0; ($count - 1) * $percentile / 100" | bc)
    echo "${sorted[$index]}"
}

get_nonce() {
    local port=${1:-8545}
    local result=$(curl -s http://localhost:$port -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getTransactionCount\",\"params\":[\"$SENDER\",\"pending\"],\"id\":1}" 2>/dev/null)
    echo "$result" | grep -o '"result":"[^"]*"' | cut -d'"' -f4
}

get_nonce_decimal() {
    local port=${1:-8545}
    local hex=$(get_nonce $port)
    echo $((16#${hex#0x}))
}

# Simple send burst - uses hardcoded ERC20 transfer calldata (same as unified_benchmark)
send_burst() {
    local port=$1
    local count=$2

    local nonce_dec=$(get_nonce_decimal $port)
    local success=0
    local failed=0

    for ((i=0; i<count; i++)); do
        local current_nonce=$(printf "0x%x" $((nonce_dec + i)))

        # Send ERC20 transfer to hardcoded contract
        local result=$(curl -s http://localhost:$port -X POST -H "Content-Type: application/json" \
            -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$CONTRACT_ADDR\",\"data\":\"$ERC20_TRANSFER\",\"gas\":\"0x30000\",\"gasPrice\":\"0x3B9ACA00\",\"nonce\":\"$current_nonce\"}],\"id\":1}" 2>/dev/null)

        if echo "$result" | grep -q '"result":"0x'; then
            ((success++))
        else
            ((failed++))
        fi
    done

    echo "$success|$failed"
}

get_metrics() {
    curl -s http://localhost:9001/metrics 2>/dev/null
}

capture_metrics() {
    local elapsed=$1
    local tx_sent=$2
    local tx_success=$3

    local metrics=$(get_metrics)

    local simulations=$(echo "$metrics" | grep "^reth_txpool_pre_warming_simulations_completed " | awk '{print $2}' || echo "0")
    local cache_hits=$(echo "$metrics" | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}' || echo "0")
    local cache_misses=$(echo "$metrics" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}' || echo "0")
    local prefetch_ops=$(echo "$metrics" | grep "^reth_txpool_pre_warming_prefetch_operations " | awk '{print $2}' || echo "0")

    local total_access=$((cache_hits + cache_misses))
    local hit_rate=0
    if [ "$total_access" -gt 0 ]; then
        hit_rate=$(echo "scale=2; $cache_hits * 100 / $total_access" | bc)
    fi

    local tps=0
    if [ "$elapsed" -gt 0 ]; then
        tps=$(echo "scale=2; $tx_success / $elapsed" | bc)
    fi

    # Write to CSV
    echo "$elapsed,$tx_sent,$tx_success,$tps,$simulations,$cache_hits,$cache_misses,$hit_rate,$prefetch_ops" >> "$METRICS_FILE"

    # Return metrics for display
    echo "$tps|$hit_rate|$cache_hits|$cache_misses|$simulations"
}

send_burst() {
    local count=$1
    local nonce_hex=$(get_nonce)
    local nonce=$((16#${nonce_hex#0x}))
    local success=0
    local failed=0

    for ((i=0; i<count; i++)); do
        local recipient=${RECIPIENTS[$((i % ${#RECIPIENTS[@]}))]}
        local current_nonce=$(printf "0x%x" $((nonce + i)))
        local value="0x2386F26FC10000"  # 0.01 ETH

        # Create and sign transaction using cast if available, otherwise use eth_sendTransaction
        local result=$(curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
            -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$recipient\",\"value\":\"$value\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\",\"nonce\":\"$current_nonce\"}],\"id\":1}" 2>/dev/null)

        if echo "$result" | grep -q "result"; then
            ((success++))
        else
            ((failed++))
        fi
    done

    echo "$success|$failed"
}

start_node() {
    local prewarm=$1
    local datadir="$LOG_DIR/data-$prewarm"
    local chain_log="$LOG_DIR/chain_${prewarm}.log"

    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    if [ "$prewarm" = "on" ]; then
        local workers=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo "8")
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$datadir" --dev --dev.block-time "${BLOCK_TIME}s" \
            --http --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming true \
            --txpool.pre-warming-workers "$workers" \
            --txpool.pre-fetch-workers "$workers" \
            --log.stdout.filter error > "$chain_log" 2>&1 &
    else
        "$RETH_DIR/target/release/op-reth" node \
            --datadir "$datadir" --dev --dev.block-time "${BLOCK_TIME}s" \
            --http --http.api eth,debug,net,web3,txpool \
            --metrics 0.0.0.0:9001 \
            --txpool.pre-warming false \
            --log.stdout.filter error > "$chain_log" 2>&1 &
    fi

    echo $!
}

wait_for_node() {
    local max_wait=30
    local count=0
    while [ $count -lt $max_wait ]; do
        if curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
            -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null | grep -q "result"; then
            return 0
        fi
        sleep 1
        ((count++))
    done
    return 1
}

run_continuous_test() {
    local mode=$1

    log "Starting continuous benchmark - Pre-warming: ${BOLD}$mode${NC}"
    log "Duration: ${DURATION}s | Interval: ${INTERVAL}s | Burst: ${BURST_SIZE} txs"

    # Start node
    log "Starting node..."
    local node_pid=$(start_node "$mode")

    if ! wait_for_node; then
        error "Node failed to start"
        exit 1
    fi

    log "Node started (PID: $node_pid)"

    # Initialize CSV
    echo "elapsed_sec,tx_sent,tx_success,tps,simulations,cache_hits,cache_misses,hit_rate,prefetch_ops" > "$METRICS_FILE"

    # Initialize counters
    local total_sent=0
    local total_success=0
    local start_time=$(date +%s)
    local end_time=$((start_time + DURATION))
    local last_capture=$start_time

    echo ""
    echo -e "${BOLD}┌─────────┬──────────┬──────────┬──────────┬──────────┬──────────┐${NC}"
    echo -e "${BOLD}│ Elapsed │ TX Sent  │ TPS      │ Hit Rate │ Hits     │ Misses   │${NC}"
    echo -e "${BOLD}├─────────┼──────────┼──────────┼──────────┼──────────┼──────────┤${NC}"

    while [ $(date +%s) -lt $end_time ]; do
        # Send burst
        local result=$(send_burst $BURST_SIZE)
        local burst_success=$(echo "$result" | cut -d'|' -f1)
        local burst_failed=$(echo "$result" | cut -d'|' -f2)

        total_sent=$((total_sent + BURST_SIZE))
        total_success=$((total_success + burst_success))

        local current_time=$(date +%s)
        local elapsed=$((current_time - start_time))

        # Capture metrics at interval
        if [ $((current_time - last_capture)) -ge $INTERVAL ]; then
            local metrics=$(capture_metrics $elapsed $total_sent $total_success)
            local tps=$(echo "$metrics" | cut -d'|' -f1)
            local hit_rate=$(echo "$metrics" | cut -d'|' -f2)
            local hits=$(echo "$metrics" | cut -d'|' -f3)
            local misses=$(echo "$metrics" | cut -d'|' -f4)

            printf "│ %7ds │ %8d │ %8s │ %8s%% │ %8s │ %8s │\n" \
                "$elapsed" "$total_sent" "$tps" "$hit_rate" "$hits" "$misses"

            last_capture=$current_time
        fi

        # Small delay between bursts
        sleep 0.5
    done

    echo -e "${BOLD}└─────────┴──────────┴──────────┴──────────┴──────────┴──────────┘${NC}"
    echo ""

    # Final metrics capture
    local final_elapsed=$(($(date +%s) - start_time))
    local final_metrics=$(capture_metrics $final_elapsed $total_sent $total_success)
    local final_tps=$(echo "$final_metrics" | cut -d'|' -f1)
    local final_hit_rate=$(echo "$final_metrics" | cut -d'|' -f2)

    # Write summary
    {
        echo "Continuous Benchmark Summary - Pre-warming: $mode"
        echo "================================================"
        echo "Duration: ${final_elapsed}s"
        echo "Total TX Sent: $total_sent"
        echo "Total TX Success: $total_success"
        echo "Average TPS: $final_tps"
        echo "Final Cache Hit Rate: ${final_hit_rate}%"
        echo ""
        echo "Metrics file: $METRICS_FILE"
    } > "$SUMMARY_FILE"

    log "Test complete. Summary saved to: $SUMMARY_FILE"

    # Cleanup
    kill $node_pid 2>/dev/null || true

    echo "$final_tps|$final_hit_rate|$total_sent|$total_success"
}

run_parallel_test() {
    log "Running PARALLEL continuous benchmark (both modes simultaneously)"
    log "Duration: ${DURATION}s | Interval: ${INTERVAL}s | Burst: ${BURST_SIZE} txs"

    local LOG_DIR_ON="$LOG_DIR/prewarm_on"
    local LOG_DIR_OFF="$LOG_DIR/prewarm_off"
    mkdir -p "$LOG_DIR_ON" "$LOG_DIR_OFF"

    # Start two nodes on different ports
    pkill -9 op-reth 2>/dev/null || true
    sleep 2

    local workers=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo "8")

    log "Using $workers workers for pre-warming and prefetch"

    # Node 1: Pre-warming ON (ports 8545, 9001, discovery 30303, authrpc 8551)
    "$RETH_DIR/target/release/op-reth" node \
        --datadir "$LOG_DIR_ON/data" --dev --dev.block-time "${BLOCK_TIME}s" \
        --http --http.port 8545 --http.api eth,debug,net,web3,txpool \
        --port 30303 \
        --authrpc.port 8551 \
        --ipcpath "$LOG_DIR_ON/reth.ipc" \
        --metrics 0.0.0.0:9001 \
        --txpool.pre-warming true \
        --txpool.pre-warming-workers "$workers" \
        --txpool.pre-fetch-workers "$workers" \
        --log.stdout.filter error > "$LOG_DIR_ON/chain.log" 2>&1 &
    local pid_on=$!

    # Node 2: Pre-warming OFF (ports 8546, 9002, discovery 30304, authrpc 8552)
    "$RETH_DIR/target/release/op-reth" node \
        --datadir "$LOG_DIR_OFF/data" --dev --dev.block-time "${BLOCK_TIME}s" \
        --http --http.port 8546 --http.api eth,debug,net,web3,txpool \
        --port 30304 \
        --authrpc.port 8552 \
        --ipcpath "$LOG_DIR_OFF/reth.ipc" \
        --metrics 0.0.0.0:9002 \
        --txpool.pre-warming false \
        --log.stdout.filter error > "$LOG_DIR_OFF/chain.log" 2>&1 &
    local pid_off=$!

    log "Started Node ON (PID: $pid_on, port 8545) and Node OFF (PID: $pid_off, port 8546)"

    # Wait for both nodes to be ready
    sleep 10

    log "Using hardcoded ERC20 contract address: $CONTRACT_ADDR"

    # Initialize CSV files
    local METRICS_ON="$LOG_DIR_ON/metrics.csv"
    local METRICS_OFF="$LOG_DIR_OFF/metrics.csv"
    echo "elapsed_sec,tx_sent,tx_success,tps,interval_tps,cache_hits,cache_misses,hit_rate,storage_hits" > "$METRICS_ON"
    echo "elapsed_sec,tx_sent,tx_success,tps,interval_tps,cache_hits,cache_misses,hit_rate,storage_hits" > "$METRICS_OFF"

    local total_sent_on=0 total_success_on=0
    local total_sent_off=0 total_success_off=0
    local start_time=$(date +%s)
    local end_time=$((start_time + DURATION))
    local last_capture=$start_time

    # Arrays to accumulate interval metrics for averaging
    local -a interval_tps_on=()
    local -a interval_tps_off=()
    local -a interval_hit_rates=()
    local -a interval_tps_diff=()
    local prev_tx_on=0 prev_tx_off=0
    local prev_time=$start_time

    echo ""
    echo -e "${BOLD}┌─────────┬────────────────────────────┬────────────────────────────┐${NC}"
    echo -e "${BOLD}│ Elapsed │      PRE-WARM ON           │      PRE-WARM OFF          │${NC}"
    echo -e "${BOLD}│         │ TPS    | Hit% | Hits       │ TPS    | Hit% | Misses     │${NC}"
    echo -e "${BOLD}├─────────┼────────────────────────────┼────────────────────────────┤${NC}"

    while [ $(date +%s) -lt $end_time ]; do
        # Send ERC20 transfer bursts to both nodes (using hardcoded contract)
        local result_on=$(send_burst 8545 $BURST_SIZE)
        local result_off=$(send_burst 8546 $BURST_SIZE)

        local success_on=$(echo "$result_on" | cut -d'|' -f1)
        local success_off=$(echo "$result_off" | cut -d'|' -f1)

        total_sent_on=$((total_sent_on + BURST_SIZE))
        total_sent_off=$((total_sent_off + BURST_SIZE))
        total_success_on=$((total_success_on + success_on))
        total_success_off=$((total_success_off + success_off))

        local current_time=$(date +%s)
        local elapsed=$((current_time - start_time))

        # Capture metrics at interval
        if [ $((current_time - last_capture)) -ge $INTERVAL ]; then
            local interval_duration=$((current_time - prev_time))

            # Metrics for ON node - use sync caching metrics for actual EVM cache hits
            local metrics_on=$(curl -s http://localhost:9001/metrics 2>/dev/null)
            local account_hits_on=$(echo "$metrics_on" | grep "^reth_sync_caching_account_cache_hits " | awk '{print $2}' | tr -d '\n' || echo "0")
            local storage_hits_on=$(echo "$metrics_on" | grep "^reth_sync_caching_storage_cache_hits " | awk '{print $2}' | tr -d '\n' || echo "0")
            local hits_on=$((${account_hits_on%.*} + ${storage_hits_on%.*}))
            local misses_on=$(echo "$metrics_on" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}' | tr -d '\n' || echo "0")
            misses_on=${misses_on%.*}
            local total_on=$((hits_on + misses_on))
            local hit_rate_on=0
            [ "$total_on" -gt 0 ] && hit_rate_on=$(echo "scale=1; $hits_on * 100 / $total_on" | bc)

            # Metrics for OFF node
            # Metrics for OFF node - also use sync caching metrics
            local metrics_off=$(curl -s http://localhost:9002/metrics 2>/dev/null)
            local account_hits_off=$(echo "$metrics_off" | grep "^reth_sync_caching_account_cache_hits " | awk '{print $2}' | tr -d '\n' || echo "0")
            local storage_hits_off=$(echo "$metrics_off" | grep "^reth_sync_caching_storage_cache_hits " | awk '{print $2}' | tr -d '\n' || echo "0")
            local hits_off=$((${account_hits_off%.*} + ${storage_hits_off%.*}))
            local misses_off=$(echo "$metrics_off" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}' | tr -d '\n' || echo "0")
            misses_off=${misses_off%.*}

            # Calculate INTERVAL TPS (transactions in this interval / interval duration)
            local interval_tx_on=$((total_success_on - prev_tx_on))
            local interval_tx_off=$((total_success_off - prev_tx_off))
            local tps_on_interval=0
            local tps_off_interval=0
            if [ "$interval_duration" -gt 0 ]; then
                tps_on_interval=$(echo "scale=2; $interval_tx_on / $interval_duration" | bc)
                tps_off_interval=$(echo "scale=2; $interval_tx_off / $interval_duration" | bc)
            fi

            # Also calculate cumulative TPS for display
            local tps_on=$(echo "scale=2; $total_success_on / $elapsed" | bc)
            local tps_off=$(echo "scale=2; $total_success_off / $elapsed" | bc)

            # Accumulate interval metrics for later averaging
            interval_tps_on+=("$tps_on_interval")
            interval_tps_off+=("$tps_off_interval")
            interval_hit_rates+=("$hit_rate_on")

            # Calculate interval TPS difference
            if [ "$tps_off_interval" != "0" ] && [ -n "$tps_off_interval" ]; then
                local diff=$(echo "scale=2; ($tps_on_interval - $tps_off_interval) * 100 / $tps_off_interval" | bc 2>/dev/null || echo "0")
                interval_tps_diff+=("$diff")
            fi

            printf "│ %7ds │ %6s | %4s%% | %8s │ %6s | N/A  | %8s │\n" \
                "$elapsed" "$tps_on_interval" "$hit_rate_on" "$hits_on" "$tps_off_interval" "$hits_off"

            # Save to CSV (both cumulative and interval)
            echo "$elapsed,$total_sent_on,$total_success_on,$tps_on,$tps_on_interval,$hits_on,$misses_on,$hit_rate_on,0" >> "$METRICS_ON"
            echo "$elapsed,$total_sent_off,$total_success_off,$tps_off,$tps_off_interval,0,0,0,0" >> "$METRICS_OFF"

            # Update previous values for next interval
            prev_tx_on=$total_success_on
            prev_tx_off=$total_success_off
            prev_time=$current_time
            last_capture=$current_time
        fi

        sleep 0.5
    done

    echo -e "${BOLD}└─────────┴────────────────────────────┴────────────────────────────┘${NC}"
    echo ""

    # Final calculations
    local final_elapsed=$(($(date +%s) - start_time))
    local final_tps_on=$(echo "scale=2; $total_success_on / $final_elapsed" | bc)
    local final_tps_off=$(echo "scale=2; $total_success_off / $final_elapsed" | bc)
    local tps_diff=$(echo "scale=2; ($final_tps_on - $final_tps_off) * 100 / $final_tps_off" | bc 2>/dev/null || echo "0")

    # Get final hit rate
    local final_metrics_on=$(curl -s http://localhost:9001/metrics 2>/dev/null)
    local final_hits=$(echo "$final_metrics_on" | grep "^reth_txpool_pre_warming_cache_hits " | awk '{print $2}' | tr -d '\n' || echo "0")
    local final_misses=$(echo "$final_metrics_on" | grep "^reth_txpool_pre_warming_cache_misses " | awk '{print $2}' | tr -d '\n' || echo "0")
    local final_total=$((final_hits + final_misses))
    local final_hit_rate=0
    [ "$final_total" -gt 0 ] && final_hit_rate=$(echo "scale=1; $final_hits * 100 / $final_total" | bc)

    # Calculate averaged statistics from interval samples
    local num_samples=${#interval_tps_on[@]}
    log "Calculating statistics from $num_samples interval samples..."

    # Calculate average, min, max, stddev for TPS ON
    local sum_tps_on=0 min_tps_on=999999 max_tps_on=0
    for val in "${interval_tps_on[@]}"; do
        sum_tps_on=$(echo "$sum_tps_on + $val" | bc)
        (( $(echo "$val < $min_tps_on" | bc -l) )) && min_tps_on=$val
        (( $(echo "$val > $max_tps_on" | bc -l) )) && max_tps_on=$val
    done
    local avg_tps_on=$(echo "scale=2; $sum_tps_on / $num_samples" | bc 2>/dev/null || echo "0")

    # Calculate average, min, max for TPS OFF
    local sum_tps_off=0 min_tps_off=999999 max_tps_off=0
    for val in "${interval_tps_off[@]}"; do
        sum_tps_off=$(echo "$sum_tps_off + $val" | bc)
        (( $(echo "$val < $min_tps_off" | bc -l) )) && min_tps_off=$val
        (( $(echo "$val > $max_tps_off" | bc -l) )) && max_tps_off=$val
    done
    local avg_tps_off=$(echo "scale=2; $sum_tps_off / $num_samples" | bc 2>/dev/null || echo "0")

    # Calculate average hit rate
    local sum_hit_rate=0
    for val in "${interval_hit_rates[@]}"; do
        sum_hit_rate=$(echo "$sum_hit_rate + $val" | bc)
    done
    local avg_hit_rate=$(echo "scale=1; $sum_hit_rate / $num_samples" | bc 2>/dev/null || echo "0")

    # Calculate average TPS difference
    local sum_diff=0
    local num_diffs=${#interval_tps_diff[@]}
    for val in "${interval_tps_diff[@]}"; do
        sum_diff=$(echo "$sum_diff + $val" | bc)
    done
    local avg_diff=$(echo "scale=2; $sum_diff / $num_diffs" | bc 2>/dev/null || echo "0")

    # Calculate P50, P95, P99 for TPS ON (using sorted array)
    IFS=$'\n' sorted_tps_on=($(sort -n <<<"${interval_tps_on[*]}")); unset IFS
    local p50_idx=$(( (num_samples - 1) * 50 / 100 ))
    local p95_idx=$(( (num_samples - 1) * 95 / 100 ))
    local p99_idx=$(( (num_samples - 1) * 99 / 100 ))
    local p50_tps_on=${sorted_tps_on[$p50_idx]:-0}
    local p95_tps_on=${sorted_tps_on[$p95_idx]:-0}
    local p99_tps_on=${sorted_tps_on[$p99_idx]:-0}

    # Generate report
    {
        echo "# Continuous Benchmark Report"
        echo ""
        echo "**Generated:** $(date '+%Y-%m-%d %H:%M:%S')"
        echo "**Duration:** ${final_elapsed} seconds"
        echo "**Burst Size:** ${BURST_SIZE} transactions"
        echo "**Block Time:** ${BLOCK_TIME}s"
        echo "**Measurement Intervals:** ${num_samples}"
        echo "**Interval Duration:** ${INTERVAL}s"
        echo ""
        echo "## Cumulative Results"
        echo ""
        echo "| Metric | Pre-warm ON | Pre-warm OFF | Difference |"
        echo "|--------|-------------|--------------|------------|"
        echo "| **Total TPS** | ${final_tps_on} | ${final_tps_off} | ${tps_diff}% |"
        echo "| **TX Sent** | ${total_sent_on} | ${total_sent_off} | - |"
        echo "| **Cache Hits** | ${final_hits} | N/A | - |"
        echo "| **Cache Misses** | ${final_misses} | N/A | - |"
        echo "| **Hit Rate** | ${final_hit_rate}% | N/A | - |"
        echo ""
        echo "## Interval-Averaged Statistics (More Accurate)"
        echo ""
        echo "These statistics are calculated from ${num_samples} interval measurements, providing more reliable averages."
        echo ""
        echo "### TPS Statistics"
        echo ""
        echo "| Metric | Pre-warm ON | Pre-warm OFF |"
        echo "|--------|-------------|--------------|"
        echo "| **Average TPS** | ${avg_tps_on} | ${avg_tps_off} |"
        echo "| **Min TPS** | ${min_tps_on} | ${min_tps_off} |"
        echo "| **Max TPS** | ${max_tps_on} | ${max_tps_off} |"
        echo "| **P50 TPS** | ${p50_tps_on} | - |"
        echo "| **P95 TPS** | ${p95_tps_on} | - |"
        echo "| **P99 TPS** | ${p99_tps_on} | - |"
        echo ""
        echo "### Average Improvement"
        echo ""
        echo "| Metric | Value |"
        echo "|--------|-------|"
        echo "| **Avg TPS Improvement** | ${avg_diff}% |"
        echo "| **Avg Cache Hit Rate** | ${avg_hit_rate}% |"
        echo ""
        echo "## Interpretation"
        echo ""
        if (( $(echo "$avg_diff > 0" | bc -l) )); then
            echo "Pre-warming **improved** average interval TPS by ${avg_diff}%"
        else
            echo "Pre-warming showed ${avg_diff}% average TPS difference"
        fi
        echo ""
        echo "Cache hit rate of ${avg_hit_rate}% (averaged across intervals) indicates effective pre-warming."
        echo ""
        echo "## Data Files"
        echo ""
        echo "- Pre-warm ON metrics: \`$METRICS_ON\`"
        echo "- Pre-warm OFF metrics: \`$METRICS_OFF\`"
    } > "$REPORT_FILE"

    log "Report saved to: $REPORT_FILE"

    # Cleanup
    kill $pid_on $pid_off 2>/dev/null || true

    echo ""
    echo -e "${GREEN}${BOLD}═══════════════════════════════════════════════════════════════${NC}"
    echo -e "${GREEN}${BOLD}  CONTINUOUS BENCHMARK COMPLETE${NC}"
    echo -e "${GREEN}${BOLD}═══════════════════════════════════════════════════════════════${NC}"
    echo ""
    echo -e "  ${BOLD}Cumulative Results:${NC}"
    echo -e "  ├─ Duration:          ${final_elapsed}s"
    echo -e "  ├─ Total TPS (ON):    ${final_tps_on}"
    echo -e "  ├─ Total TPS (OFF):   ${final_tps_off}"
    echo -e "  └─ Total Improvement: ${tps_diff}%"
    echo ""
    echo -e "  ${BOLD}Interval-Averaged Results (${num_samples} samples):${NC}"
    echo -e "  ├─ Avg TPS (ON):      ${BOLD}${avg_tps_on}${NC}"
    echo -e "  ├─ Avg TPS (OFF):     ${avg_tps_off}"
    echo -e "  ├─ Avg Improvement:   ${BOLD}${avg_diff}%${NC}"
    echo -e "  ├─ P50 TPS (ON):      ${p50_tps_on}"
    echo -e "  ├─ P95 TPS (ON):      ${p95_tps_on}"
    echo -e "  └─ Avg Hit Rate:      ${BOLD}${avg_hit_rate}%${NC}"
    echo ""
    echo -e "  Report: ${REPORT_FILE}"
    echo ""
}

#-------------------------------------------------------------------------------
# Main
#-------------------------------------------------------------------------------

echo ""
echo -e "${BOLD}${CYAN}╔══════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${CYAN}║       CONTINUOUS BENCHMARK - Long-running Load Test          ║${NC}"
echo -e "${BOLD}${CYAN}╚══════════════════════════════════════════════════════════════╝${NC}"
echo ""

if [ "$PARALLEL" = true ]; then
    run_parallel_test
elif [ -n "$PREWARM_MODE" ]; then
    run_continuous_test "$PREWARM_MODE"
else
    echo "Usage: $0 [--prewarm-on | --prewarm-off | --parallel]"
    echo ""
    echo "Options:"
    echo "  --prewarm-on     Run with pre-warming enabled"
    echo "  --prewarm-off    Run with pre-warming disabled"
    echo "  --parallel       Run both modes simultaneously (recommended)"
    echo "  --duration N     Test duration in seconds (default: 3600)"
    echo "  --interval N     Metric capture interval (default: 30)"
    echo "  --burst N        Transactions per burst (default: 100)"
    echo "  --block-time N   Block time in seconds (default: 1)"
    echo ""
    echo "Examples:"
    echo "  $0 --parallel --duration 3600 --burst 200"
    echo "  $0 --prewarm-on --duration 1800"
    exit 1
fi

