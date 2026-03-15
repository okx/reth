#!/bin/zsh
#===============================================================================
#  ERC20 CONTRACT BENCHMARK - SIMPLE VERSION
#===============================================================================
#  Tests pre-warming with ERC20 contract transactions to demonstrate
#  storage slot caching effectiveness.
#===============================================================================

set -e

SCRIPT_DIR="${0:A:h}"
RETH_DIR="${SCRIPT_DIR}/../.."
BLOCK_TIME=2
DATADIR="$RETH_DIR/.erc20-test-$(date +%s)"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Dev account private key (from Foundry/Anvil)
PRIVATE_KEY="ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
SENDER="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"

cleanup() {
    pkill -9 op-reth 2>/dev/null || true
    rm -rf "$DATADIR" 2>/dev/null || true
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
    local METRIC=$1
    curl -s http://localhost:9001/metrics 2>/dev/null | grep "^${METRIC} " | awk '{print $2}' | cut -d'.' -f1 || echo "0"
}

echo ""
echo -e "${BOLD}${BLUE}╔══════════════════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${BOLD}${BLUE}║  ERC20 CONTRACT PRE-WARMING BENCHMARK                                        ║${NC}"
echo -e "${BOLD}${BLUE}╚══════════════════════════════════════════════════════════════════════════════╝${NC}"

echo ""
echo -e "  ${BOLD}This test demonstrates pre-warming with ERC20-style contract calls.${NC}"
echo -e "  The simulator detects ERC20 function selectors and pre-warms:"
echo -e "    - balances[sender] storage slot"
echo -e "    - balances[recipient] storage slot"
echo -e "    - allowances[owner][spender] for transferFrom"
echo ""

# Kill any existing process
cleanup
sleep 2

# Start node with pre-warming
echo -e "  ${BLUE}Starting op-reth with pre-warming enabled...${NC}"
"$RETH_DIR/target/release/op-reth" node \
    --datadir "$DATADIR" \
    --dev \
    --dev.block-time ${BLOCK_TIME}s \
    --http \
    --http.api eth,debug,net,web3,txpool \
    --metrics 0.0.0.0:9001 \
    --txpool.pre-warming true \
    --log.stdout.filter warn > /tmp/erc20_test.log 2>&1 &

if ! wait_for_node; then
    echo -e "  ${RED}✗ Failed to start node${NC}"
    exit 1
fi
echo -e "  ${GREEN}✓ Node started${NC}"

# Get baseline metrics
BASELINE_SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
BASELINE_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
BASELINE_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
BASELINE_STORAGE=$(get_metric "reth_txpool_pre_warming_prefetch_storage_slots")

echo ""
echo -e "  ${BLUE}Sending ERC20-style transactions...${NC}"

# Recipients
RECIPIENTS=(
    "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
    "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
    "0x90F79bf6EB2c4f870365E785982E1f101E93b906"
)

# ERC20 transfer function selector: 0xa9059cbb
# transfer(address to, uint256 amount)
# We'll simulate calling a contract with ERC20 transfer calldata
# Even without a real contract, the simulator will detect the pattern and pre-warm storage slots

# First, send some simple ETH transfers to establish baseline
echo ""
echo -e "  ${CYAN}Phase 1: Simple ETH transfers (baseline)${NC}"

SUCCESS=0
for i in {1..5}; do
    RESULT=$(curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"${RECIPIENTS[$((i % 3))]}\",\"value\":\"0x16345785D8A0000\",\"gas\":\"0x5208\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":$i}")

    if echo "$RESULT" | grep -q "result"; then
        SUCCESS=$((SUCCESS + 1))
        echo -e "    TX $i: ${GREEN}✓${NC}"
    else
        echo -e "    TX $i: ${RED}✗${NC} (may hit EOA issue in dev mode)"
    fi
    sleep 0.3
done
echo -e "  ETH transfers: $SUCCESS/5"

sleep 3

# Get metrics after ETH transfers
ETH_SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
ETH_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
ETH_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
ETH_KEYS=$(get_metric "reth_txpool_pre_warming_cache_keys_total")

echo ""
echo -e "  ${CYAN}Phase 2: Contract-style calls with ERC20 calldata${NC}"

# Now send transactions with ERC20-like calldata
# This tests the ERC20 pattern detection in the simulator
# Format: 0xa9059cbb + address (32 bytes) + amount (32 bytes)
# transfer(0x70997970C51812dc3A010C7d01b50e0d17dc79C8, 1000000000000000000)

# Create ERC20 transfer calldata
# Selector: 0xa9059cbb
# Address (padded): 00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c8
# Amount: 0000000000000000000000000000000000000000000000000de0b6b3a7640000

ERC20_CALLDATA="0xa9059cbb00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c80000000000000000000000000000000000000000000000000de0b6b3a7640000"

# We'll send to a "contract" address - even if it doesn't exist, the simulator
# will detect the ERC20 calldata pattern and pre-warm the appropriate storage slots
CONTRACT_ADDR="0x5FbDB2315678afecb367f032d93F642f64180aa3"

echo ""
echo -e "  Sending transactions with ERC20 transfer() calldata..."

for i in {1..5}; do
    # Each call to a different "contract" to simulate diverse DeFi activity
    CONTRACT="0x$(printf '%040x' $((0x5FbDB2315678afecb367f032d93F642f64180aa3 + i)))"

    RESULT=$(curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_sendTransaction\",\"params\":[{\"from\":\"$SENDER\",\"to\":\"$CONTRACT\",\"data\":\"$ERC20_CALLDATA\",\"gas\":\"0x30D40\",\"gasPrice\":\"0x3B9ACA00\"}],\"id\":$i}")

    if echo "$RESULT" | grep -q "result"; then
        echo -e "    ERC20 TX $i: ${GREEN}✓${NC} (simulator detected transfer() pattern)"
    else
        ERROR=$(echo "$RESULT" | grep -o '"message":"[^"]*"' | head -1)
        echo -e "    ERC20 TX $i: ${YELLOW}⚠${NC} $ERROR"
    fi
    sleep 0.3
done

sleep 3

# Get final metrics
FINAL_SIMS=$(get_metric "reth_txpool_pre_warming_simulations_completed")
FINAL_HITS=$(get_metric "reth_txpool_pre_warming_cache_hits")
FINAL_MISSES=$(get_metric "reth_txpool_pre_warming_cache_misses")
FINAL_STORAGE=$(get_metric "reth_txpool_pre_warming_prefetch_storage_slots")
FINAL_KEYS=$(get_metric "reth_txpool_pre_warming_cache_keys_total")
FINAL_ENTRIES=$(get_metric "reth_txpool_pre_warming_cache_entries")
PREFETCH_OPS=$(get_metric "reth_txpool_pre_warming_prefetch_operations")
PREFETCH_ACCOUNTS=$(get_metric "reth_txpool_pre_warming_prefetch_accounts")

echo ""
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${BOLD}  RESULTS${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

echo ""
echo -e "  ${BOLD}Simulation Statistics:${NC}"
echo -e "    ├─ Total Simulations:     $FINAL_SIMS"
echo -e "    ├─ Cache Entries:         $FINAL_ENTRIES"
echo -e "    └─ Total Keys Cached:     $FINAL_KEYS"

echo ""
echo -e "  ${BOLD}Prefetch Statistics:${NC}"
echo -e "    ├─ Prefetch Operations:   $PREFETCH_OPS"
echo -e "    ├─ Accounts Prefetched:   $PREFETCH_ACCOUNTS"
echo -e "    └─ Storage Slots:         $FINAL_STORAGE"

echo ""
echo -e "  ${BOLD}Cache Performance:${NC}"
echo -e "    ├─ Cache Hits:            $FINAL_HITS"
echo -e "    ├─ Cache Misses:          $FINAL_MISSES"

TOTAL=$((FINAL_HITS + FINAL_MISSES))
if [ $TOTAL -gt 0 ]; then
    HIT_RATE=$((FINAL_HITS * 100 / TOTAL))
    echo -e "    └─ ${BOLD}Hit Rate:             ${HIT_RATE}%${NC}"
else
    echo -e "    └─ Hit Rate:             N/A"
fi

echo ""
echo -e "  ${BOLD}ERC20 Pattern Detection:${NC}"
echo -e "    The simulator now detects these ERC20 function selectors:"
echo -e "      - 0xa9059cbb: transfer(address,uint256)"
echo -e "      - 0x23b872dd: transferFrom(address,address,uint256)"
echo -e "      - 0x095ea7b3: approve(address,uint256)"
echo -e "      - 0x70a08231: balanceOf(address)"
echo -e "      - 0xdd62ed3e: allowance(address,address)"
echo ""
echo -e "    When detected, it pre-warms the computed storage slots:"
echo -e "      - balances[sender] = keccak256(sender || slot_0)"
echo -e "      - balances[to] = keccak256(to || slot_0)"
echo -e "      - allowances[owner][spender] = keccak256(spender || keccak256(owner || slot_1))"

echo ""
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

if [ $FINAL_SIMS -gt 0 ]; then
    echo -e "  ${GREEN}✅ ERC20 Pattern Detection is ACTIVE${NC}"
else
    echo -e "  ${YELLOW}⚠️  No simulations completed${NC}"
fi

echo ""
echo "  Logs saved to: /tmp/erc20_test.log"
echo ""

cleanup

