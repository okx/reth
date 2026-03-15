#!/bin/bash
#===============================================================================
#  DEVNET PRE-WARMING CONTINUOUS MONITOR
#===============================================================================
#  Continuously monitors pre-warming metrics and generates periodic reports
#  with statistical analysis (mean, median, min, max, percentiles)
#
#  Usage:
#    ./monitor_devnet.sh                           # Default: localhost:9001, 30s interval
#    ./monitor_devnet.sh --url http://devnet:9001  # Custom metrics URL
#    ./monitor_devnet.sh --interval 60             # 60 second sampling interval
#    ./monitor_devnet.sh --report-interval 3600    # Generate report every hour
#    ./monitor_devnet.sh --duration 7200           # Run for 2 hours then exit
#
#===============================================================================

set -e

# Defaults
METRICS_URL="http://localhost:9001/metrics"
RPC_URL="http://localhost:8545"
SAMPLE_INTERVAL=30          # Sample every 30 seconds
REPORT_INTERVAL=300         # Generate report every 5 minutes
DURATION=0                  # 0 = run forever
OUTPUT_DIR="./devnet_monitoring_$(date +%Y%m%d_%H%M%S)"

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        --url)
            METRICS_URL="$2"
            shift 2
            ;;
        --rpc)
            RPC_URL="$2"
            shift 2
            ;;
        --interval)
            SAMPLE_INTERVAL="$2"
            shift 2
            ;;
        --report-interval)
            REPORT_INTERVAL="$2"
            shift 2
            ;;
        --duration)
            DURATION="$2"
            shift 2
            ;;
        --output)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo ""
            echo "Options:"
            echo "  --url URL              Metrics endpoint (default: http://localhost:9001/metrics)"
            echo "  --rpc URL              RPC endpoint (default: http://localhost:8545)"
            echo "  --interval SECS        Sampling interval in seconds (default: 30)"
            echo "  --report-interval SECS Report generation interval (default: 300)"
            echo "  --duration SECS        Total run duration, 0=forever (default: 0)"
            echo "  --output DIR           Output directory for reports"
            echo "  -h, --help             Show this help"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Files
RAW_DATA_FILE="$OUTPUT_DIR/raw_metrics.csv"
REPORT_FILE="$OUTPUT_DIR/latest_report.txt"
HISTORY_FILE="$OUTPUT_DIR/report_history.log"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

log() {
    echo -e "${CYAN}[$(date '+%H:%M:%S')]${NC} $1"
}

error() {
    echo -e "${RED}[ERROR]${NC} $1" >&2
}

# Initialize CSV with headers
# Note: Using reth_txpool_pre_warming_cache_hits/misses (CachedReads), NOT reth_sync_caching_* (ExecutionCache)
init_csv() {
    echo "timestamp,epoch,block_number,sim_triggered,sim_completed,sim_failed,sim_dropped,cache_hits,cache_misses,cache_entries,cache_keys,cache_evictions,prefetch_ops,prefetch_accounts,prefetch_storage,prefetch_contracts" > "$RAW_DATA_FILE"
}

# Fetch single metric value
get_metric() {
    local metric_name=$1
    local metrics=$2
    echo "$metrics" | grep "^${metric_name} " | awk '{print $2}' | cut -d'.' -f1 | head -1 || echo "0"
}

# Get current block number
get_block_number() {
    local result=$(curl -s --max-time 5 "$RPC_URL" -X POST -H "Content-Type: application/json" \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' 2>/dev/null)
    local hex=$(echo "$result" | grep -o '"result":"[^"]*"' | cut -d'"' -f4)
    if [ -n "$hex" ]; then
        echo $((16#${hex#0x}))
    else
        echo "0"
    fi
}

# Sample current metrics
sample_metrics() {
    local metrics=$(curl -s --max-time 10 "$METRICS_URL" 2>/dev/null)

    if [ -z "$metrics" ]; then
        error "Failed to fetch metrics from $METRICS_URL"
        return 1
    fi

    local timestamp=$(date '+%Y-%m-%d %H:%M:%S')
    local epoch=$(date +%s)
    local block=$(get_block_number)

    # Simulation metrics
    local sim_triggered=$(get_metric "reth_txpool_pre_warming_simulations_triggered" "$metrics")
    local sim_completed=$(get_metric "reth_txpool_pre_warming_simulations_completed" "$metrics")
    local sim_failed=$(get_metric "reth_txpool_pre_warming_simulations_failed" "$metrics")
    local sim_dropped=$(get_metric "reth_txpool_pre_warming_simulations_dropped" "$metrics")

    # Cache metrics
    local cache_hits=$(get_metric "reth_txpool_pre_warming_cache_hits" "$metrics")
    local cache_misses=$(get_metric "reth_txpool_pre_warming_cache_misses" "$metrics")
    local cache_entries=$(get_metric "reth_txpool_pre_warming_cache_entries" "$metrics")
    local cache_keys=$(get_metric "reth_txpool_pre_warming_cache_keys_total" "$metrics")
    local cache_evictions=$(get_metric "reth_txpool_pre_warming_cache_evictions" "$metrics")

    # Prefetch metrics
    local prefetch_ops=$(get_metric "reth_txpool_pre_warming_prefetch_operations" "$metrics")
    local prefetch_accounts=$(get_metric "reth_txpool_pre_warming_prefetch_accounts" "$metrics")
    local prefetch_storage=$(get_metric "reth_txpool_pre_warming_prefetch_storage_slots" "$metrics")
    local prefetch_contracts=$(get_metric "reth_txpool_pre_warming_prefetch_contracts" "$metrics")

    # CachedReads cache metrics - what pre-warming/prefetch populates and payload builder uses
    # NOTE: Using reth_txpool_pre_warming_cache_hits/misses, NOT reth_sync_caching_* (ExecutionCache is separate)
    # The cache_hits and cache_misses above already capture the correct metrics

    # Handle empty values
    sim_triggered=${sim_triggered:-0}
    sim_completed=${sim_completed:-0}
    sim_failed=${sim_failed:-0}
    sim_dropped=${sim_dropped:-0}
    cache_hits=${cache_hits:-0}
    cache_misses=${cache_misses:-0}
    cache_entries=${cache_entries:-0}
    cache_keys=${cache_keys:-0}
    cache_evictions=${cache_evictions:-0}
    prefetch_ops=${prefetch_ops:-0}
    prefetch_accounts=${prefetch_accounts:-0}
    prefetch_storage=${prefetch_storage:-0}
    prefetch_contracts=${prefetch_contracts:-0}

    # Write to CSV
    echo "$timestamp,$epoch,$block,$sim_triggered,$sim_completed,$sim_failed,$sim_dropped,$cache_hits,$cache_misses,$cache_entries,$cache_keys,$cache_evictions,$prefetch_ops,$prefetch_accounts,$prefetch_storage,$prefetch_contracts" >> "$RAW_DATA_FILE"

    # Return key values for live display
    echo "$sim_completed|$sim_failed|$cache_hits|$cache_misses|$prefetch_ops|$block"
}

# Calculate statistics from CSV data
calculate_stats() {
    local period_start=$1  # Epoch timestamp
    local period_end=$2

    # Use Python for statistical calculations
    python3 << PYEOF
import csv
import sys
from collections import defaultdict

period_start = $period_start
period_end = $period_end
csv_file = "$RAW_DATA_FILE"

# Read data within period
data = defaultdict(list)
first_row = None
last_row = None
row_count = 0

try:
    with open(csv_file, 'r') as f:
        reader = csv.DictReader(f)
        for row in reader:
            epoch = int(row['epoch'])
            if period_start <= epoch <= period_end:
                row_count += 1
                if first_row is None:
                    first_row = row
                last_row = row

                # Collect numeric values
                for key in row:
                    if key not in ['timestamp', 'epoch']:
                        try:
                            data[key].append(float(row[key]))
                        except:
                            pass
except Exception as e:
    print(f"Error reading CSV: {e}", file=sys.stderr)
    sys.exit(1)

if row_count < 2:
    print("INSUFFICIENT_DATA")
    sys.exit(0)

def calc_stats(values):
    if not values:
        return {'min': 0, 'max': 0, 'mean': 0, 'median': 0, 'p95': 0, 'p99': 0}

    sorted_v = sorted(values)
    n = len(sorted_v)

    return {
        'min': sorted_v[0],
        'max': sorted_v[-1],
        'mean': sum(values) / n,
        'median': sorted_v[n // 2],
        'p95': sorted_v[int(n * 0.95)] if n > 1 else sorted_v[-1],
        'p99': sorted_v[int(n * 0.99)] if n > 1 else sorted_v[-1]
    }

def calc_delta(first, last, key):
    try:
        return float(last[key]) - float(first[key])
    except:
        return 0

# Calculate deltas (changes over period)
period_duration = period_end - period_start
if period_duration <= 0:
    period_duration = 1

# Deltas
delta_sim_completed = calc_delta(first_row, last_row, 'sim_completed')
delta_sim_failed = calc_delta(first_row, last_row, 'sim_failed')
delta_cache_hits = calc_delta(first_row, last_row, 'cache_hits')
delta_cache_misses = calc_delta(first_row, last_row, 'cache_misses')
delta_prefetch_ops = calc_delta(first_row, last_row, 'prefetch_ops')
delta_prefetch_accounts = calc_delta(first_row, last_row, 'prefetch_accounts')
delta_blocks = calc_delta(first_row, last_row, 'block_number')

# Rates (per second)
sim_rate = delta_sim_completed / period_duration if period_duration > 0 else 0
prefetch_rate = delta_prefetch_ops / period_duration if period_duration > 0 else 0
block_rate = delta_blocks / period_duration if period_duration > 0 else 0

# Hit rates - using CachedReads metrics (reth_txpool_pre_warming_cache_hits/misses)
total_cache_access = delta_cache_hits + delta_cache_misses
cache_hit_rate = (delta_cache_hits / total_cache_access * 100) if total_cache_access > 0 else 0


# Simulation success rate
total_sim = delta_sim_completed + delta_sim_failed
sim_success_rate = (delta_sim_completed / total_sim * 100) if total_sim > 0 else 100

# Output results as key=value pairs
print(f"SAMPLES={row_count}")
print(f"PERIOD_SECS={period_duration}")
print(f"BLOCKS_PROCESSED={int(delta_blocks)}")
print(f"BLOCK_RATE={block_rate:.2f}")
print(f"SIMULATIONS_COMPLETED={int(delta_sim_completed)}")
print(f"SIMULATIONS_FAILED={int(delta_sim_failed)}")
print(f"SIMULATION_RATE={sim_rate:.2f}")
print(f"SIMULATION_SUCCESS_RATE={sim_success_rate:.1f}")
print(f"CACHE_HITS={int(delta_cache_hits)}")
print(f"CACHE_MISSES={int(delta_cache_misses)}")
print(f"CACHE_HIT_RATE={cache_hit_rate:.1f}")
print(f"PREFETCH_OPS={int(delta_prefetch_ops)}")
print(f"PREFETCH_ACCOUNTS={int(delta_prefetch_accounts)}")
print(f"PREFETCH_RATE={prefetch_rate:.2f}")
print(f"CACHE_ENTRIES_AVG={calc_stats(data['cache_entries'])['mean']:.0f}")
print(f"CACHE_ENTRIES_MAX={calc_stats(data['cache_entries'])['max']:.0f}")

PYEOF
}

# Generate human-readable report
generate_report() {
    local period_start=$1
    local period_end=$2
    local report_name=$3

    local stats=$(calculate_stats "$period_start" "$period_end")

    if [ "$stats" = "INSUFFICIENT_DATA" ]; then
        log "Not enough data to generate report yet"
        return
    fi

    # Parse stats into variables
    eval $(echo "$stats" | grep "=")

    local period_mins=$(echo "scale=1; $PERIOD_SECS / 60" | bc)
    local report_time=$(date '+%Y-%m-%d %H:%M:%S')

    # Generate report
    cat > "$REPORT_FILE" << EOF
================================================================================
                    PRE-WARMING PERFORMANCE REPORT
================================================================================
Generated: $report_time
Period: ${period_mins} minutes ($SAMPLES samples)
Metrics URL: $METRICS_URL
--------------------------------------------------------------------------------

📊 THROUGHPUT
--------------------------------------------------------------------------------
  Blocks Processed:     $BLOCKS_PROCESSED
  Block Rate:           ${BLOCK_RATE}/sec

🔄 SIMULATIONS
--------------------------------------------------------------------------------
  Completed:            $SIMULATIONS_COMPLETED
  Failed:               $SIMULATIONS_FAILED
  Success Rate:         ${SIMULATION_SUCCESS_RATE}%
  Simulation Rate:      ${SIMULATION_RATE}/sec

💾 PRE-WARMING CACHE
--------------------------------------------------------------------------------
  Cache Hits:           $CACHE_HITS
  Cache Misses:         $CACHE_MISSES
  Hit Rate:             ${CACHE_HIT_RATE}%
  Avg Entries:          $CACHE_ENTRIES_AVG
  Max Entries:          $CACHE_ENTRIES_MAX

📥 PREFETCH OPERATIONS
--------------------------------------------------------------------------------
  Total Operations:     $PREFETCH_OPS
  Accounts Prefetched:  $PREFETCH_ACCOUNTS
  Prefetch Rate:        ${PREFETCH_RATE}/sec

⚡ EVM EXECUTION CACHE (Actual Performance)
--------------------------------------------------------------------------------
  EVM Cache Hits:       $EVM_HITS
  EVM Cache Misses:     $EVM_MISSES
  EVM Hit Rate:         ${EVM_HIT_RATE}%

📈 HEALTH INDICATORS
--------------------------------------------------------------------------------
EOF

    # Add health indicators
    local status_sim="✅ GOOD"
    local status_cache="✅ GOOD"
    local status_evm="✅ GOOD"

    if (( $(echo "$SIMULATION_SUCCESS_RATE < 90" | bc -l) )); then
        status_sim="⚠️  WARNING"
    fi
    if (( $(echo "$SIMULATION_SUCCESS_RATE < 75" | bc -l) )); then
        status_sim="❌ CRITICAL"
    fi

    if (( $(echo "$CACHE_HIT_RATE < 50" | bc -l) )); then
        status_cache="⚠️  WARNING"
    fi
    if (( $(echo "$CACHE_HIT_RATE < 20" | bc -l) )); then
        status_cache="❌ LOW"
    fi

    if (( $(echo "$EVM_HIT_RATE < 50" | bc -l) )); then
        status_evm="⚠️  WARNING"
    fi
    if (( $(echo "$EVM_HIT_RATE < 20" | bc -l) )); then
        status_evm="❌ LOW"
    fi

    cat >> "$REPORT_FILE" << EOF
  Simulation Health:    $status_sim (${SIMULATION_SUCCESS_RATE}% success)
  Cache Efficiency:     $status_cache (${CACHE_HIT_RATE}% hit rate)
  EVM Cache Efficiency: $status_evm (${EVM_HIT_RATE}% hit rate)

================================================================================
EOF

    # Also append to history
    echo "[$report_time] Blocks: $BLOCKS_PROCESSED | SimRate: ${SIMULATION_RATE}/s | CacheHit: ${CACHE_HIT_RATE}% | EVMHit: ${EVM_HIT_RATE}%" >> "$HISTORY_FILE"

    # Print to console
    echo ""
    cat "$REPORT_FILE"

    log "Report saved to: $REPORT_FILE"
}

# Main monitoring loop
main() {
    echo ""
    echo -e "${BOLD}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${BOLD}║       DEVNET PRE-WARMING CONTINUOUS MONITOR                  ║${NC}"
    echo -e "${BOLD}╚══════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    log "Metrics URL: $METRICS_URL"
    log "Sample Interval: ${SAMPLE_INTERVAL}s"
    log "Report Interval: ${REPORT_INTERVAL}s"
    log "Output Directory: $OUTPUT_DIR"
    echo ""

    # Initialize
    init_csv

    local start_time=$(date +%s)
    local last_report_time=$start_time
    local sample_count=0

    # Header for live output
    echo -e "${BOLD}┌─────────────────────────────────────────────────────────────────────┐${NC}"
    echo -e "${BOLD}│ Time     │ Block   │ Sims    │ Failed │ Hits    │ Misses │ Prefetch │${NC}"
    echo -e "${BOLD}├─────────────────────────────────────────────────────────────────────┤${NC}"

    while true; do
        local current_time=$(date +%s)

        # Check duration limit
        if [ "$DURATION" -gt 0 ]; then
            local elapsed=$((current_time - start_time))
            if [ "$elapsed" -ge "$DURATION" ]; then
                log "Duration limit reached. Generating final report..."
                generate_report "$start_time" "$current_time" "final"
                break
            fi
        fi

        # Sample metrics
        local result=$(sample_metrics)
        if [ $? -eq 0 ] && [ -n "$result" ]; then
            sample_count=$((sample_count + 1))

            # Parse result
            IFS='|' read -r sims failed hits misses prefetch block <<< "$result"

            # Live display
            printf "│ %8s │ %7s │ %7s │ %6s │ %7s │ %6s │ %8s │\n" \
                "$(date '+%H:%M:%S')" "$block" "$sims" "$failed" "$hits" "$misses" "$prefetch"
        fi

        # Generate periodic report
        if [ $((current_time - last_report_time)) -ge "$REPORT_INTERVAL" ]; then
            echo -e "${BOLD}└─────────────────────────────────────────────────────────────────────┘${NC}"
            generate_report "$last_report_time" "$current_time" "periodic"
            last_report_time=$current_time

            # Restart live header
            echo ""
            echo -e "${BOLD}┌─────────────────────────────────────────────────────────────────────┐${NC}"
            echo -e "${BOLD}│ Time     │ Block   │ Sims    │ Failed │ Hits    │ Misses │ Prefetch │${NC}"
            echo -e "${BOLD}├─────────────────────────────────────────────────────────────────────┤${NC}"
        fi

        sleep "$SAMPLE_INTERVAL"
    done

    echo -e "${BOLD}└─────────────────────────────────────────────────────────────────────┘${NC}"
    log "Monitoring complete. Data saved to: $OUTPUT_DIR"
}

# Handle Ctrl+C gracefully
trap 'echo ""; log "Interrupted. Generating final report..."; generate_report "$last_report_time" "$(date +%s)" "interrupted"; exit 0' INT TERM

# Run
main

