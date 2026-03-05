#!/usr/bin/env bash
# ============================================================================
# QMDB Provider Pipeline Benchmark
# ============================================================================
#
# Runs end-to-end block execution benchmarks with QMDB as state backend:
#   1. Pre-populates QMDB with 100k accounts
#   2. Generates synthetic transactions (ETH transfers + ERC20 transfers)
#   3. Executes blocks via revm EVM → produces BundleState
#   4. Commits BundleState to QMDB via QmdbStore
#   5. Reports per-block timing and throughput (tx/s)
#
# Usage:
#   ./run_bench.sh              # Run all benchmarks
#   ./run_bench.sh sync         # Only sync mode (flush per block)
#   ./run_bench.sh pipeline     # Only pipeline mode (batch flush)
#   ./run_bench.sh eth          # Only ETH transfer workload
#   ./run_bench.sh erc20        # Only ERC20 transfer workload
# ============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

cd "$REPO_ROOT"

FILTER="${1:-}"

echo "============================================"
echo " QMDB Provider Pipeline Benchmark"
echo "============================================"
echo ""
echo "Config: 100k pre-populated accounts, 10 blocks, 20000 tx/block"
echo ""

if [ -n "$FILTER" ]; then
    echo "Filter: $FILTER"
    echo ""
    cargo bench --bench pipeline -p xlayer-qmdb-provider -- "$FILTER"
else
    cargo bench --bench pipeline -p xlayer-qmdb-provider
fi
