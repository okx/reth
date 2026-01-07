#!/bin/bash
# Run proof generation benchmarks and save results
#
# Usage:
#   ./run_proof_benchmarks.sh              # Run all benchmarks
#   ./run_proof_benchmarks.sh --quick      # Run with fewer samples (faster)
#   ./run_proof_benchmarks.sh --baseline   # Save as baseline for comparison

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$SCRIPT_DIR"  # Script is already in project root

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Proof Generation Benchmark Runner${NC}"
echo -e "${GREEN}========================================${NC}"
echo

# Check if cargo is available
if ! command -v cargo &> /dev/null; then
    echo -e "${RED}Error: cargo not found${NC}"
    exit 1
fi

cd "$PROJECT_ROOT"

# Parse arguments
QUICK_MODE=""
BASELINE_MODE=""
COMPARE_MODE=""

for arg in "$@"; do
    case $arg in
        --quick)
            QUICK_MODE="--quick"
            echo -e "${YELLOW}Quick mode enabled (fewer samples)${NC}"
            shift
            ;;
        --baseline)
            BASELINE_MODE="--save-baseline"
            echo -e "${YELLOW}Saving results as baseline${NC}"
            shift
            ;;
        --compare)
            COMPARE_MODE="--baseline"
            echo -e "${YELLOW}Comparing against baseline${NC}"
            shift
            ;;
        *)
            ;;
    esac
done

echo -e "${GREEN}Building benchmark...${NC}"
cargo build --release --package reth-trie-parallel --benches

echo
echo -e "${GREEN}Running benchmarks...${NC}"
echo -e "${YELLOW}This will measure:${NC}"
echo "  - Single storage proof generation time"
echo "  - Account multiproof generation (parallel)"
echo "  - MDBX trie node read latency"
echo "  - Detailed timing breakdowns"
echo

# Run the benchmark
if [ -n "$BASELINE_MODE" ]; then
    cargo bench --package reth-trie-parallel --bench proof_generation $QUICK_MODE -- --save-baseline proof_baseline
elif [ -n "$COMPARE_MODE" ]; then
    cargo bench --package reth-trie-parallel --bench proof_generation $QUICK_MODE -- --baseline proof_baseline
else
    cargo bench --package reth-trie-parallel --bench proof_generation $QUICK_MODE
fi

echo
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Benchmark Results${NC}"
echo -e "${GREEN}========================================${NC}"
echo
echo "Results saved to: target/criterion/"
echo
echo "To view detailed HTML reports:"
echo "  open target/criterion/report/index.html"
echo
echo "To compare against baseline:"
echo "  ./run_proof_benchmarks.sh --compare"
echo

# Extract and display key metrics if criterion output exists
if [ -f "target/criterion/single_storage_proof/slots_10/mdbx_direct/new/estimates.json" ]; then
    echo -e "${GREEN}Quick Summary (Single Proof with 10 slots):${NC}"
    
    # Use jq if available, otherwise skip
    if command -v jq &> /dev/null; then
        MEAN=$(jq -r '.mean.point_estimate' target/criterion/single_storage_proof/slots_10/mdbx_direct/new/estimates.json)
        # Convert to ms (criterion stores in ns)
        MEAN_MS=$(echo "scale=3; $MEAN / 1000000" | bc)
        echo "  Mean time: ${MEAN_MS}ms"
    else
        echo "  (Install 'jq' for summary statistics)"
    fi
fi

echo
echo -e "${GREEN}Done!${NC}"
