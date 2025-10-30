#!/bin/bash
#
# Legacy RPC Comprehensive Test Script
#
# This script tests all Legacy RPC functionality on a running Reth node
# Usage: ./test_legacy_rpc.sh [reth_url] [cutoff_block]
#
# Example:
#   ./test_legacy_rpc.sh http://localhost:8545 1000000
#

set -e

# ========================================
# Configuration
# ========================================

RETH_URL="${1:-http://localhost:8545}"
CUTOFF_BLOCK="${2:-1000000}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Test counters
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0
SKIPPED_TESTS=0

# Test results
declare -a FAILED_TEST_NAMES

# ========================================
# Helper Functions
# ========================================

log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[PASS]${NC} $1"
}

log_error() {
    echo -e "${RED}[FAIL]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_section() {
    echo ""
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}$1${NC}"
    echo -e "${BLUE}========================================${NC}"
}

# RPC call helper
rpc_call() {
    local method=$1
    local params=$2
    local response

    response=$(curl -s -X POST "$RETH_URL" \
        -H "Content-Type: application/json" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"$method\",\"params\":$params,\"id\":1}")

    echo "$response"
}

# Check if result is not null and not error
check_result() {
    local response=$1
    local test_name=$2

    TOTAL_TESTS=$((TOTAL_TESTS + 1))

    if echo "$response" | jq -e '.error' > /dev/null 2>&1; then
        log_error "$test_name"
        echo "       Error: $(echo "$response" | jq -r '.error.message')"
        FAILED_TESTS=$((FAILED_TESTS + 1))
        FAILED_TEST_NAMES+=("$test_name")
        return 1
    elif echo "$response" | jq -e '.result' > /dev/null 2>&1; then
        log_success "$test_name"
        PASSED_TESTS=$((PASSED_TESTS + 1))
        return 0
    else
        log_error "$test_name - Invalid response format"
        FAILED_TESTS=$((FAILED_TESTS + 1))
        FAILED_TEST_NAMES+=("$test_name")
        return 1
    fi
}

# Check if result is specifically non-null
check_result_not_null() {
    local response=$1
    local test_name=$2

    TOTAL_TESTS=$((TOTAL_TESTS + 1))

    if echo "$response" | jq -e '.error' > /dev/null 2>&1; then
        log_error "$test_name"
        echo "       Error: $(echo "$response" | jq -r '.error.message')"
        FAILED_TESTS=$((FAILED_TESTS + 1))
        FAILED_TEST_NAMES+=("$test_name")
        return 1
    elif echo "$response" | jq -e '.result != null' > /dev/null 2>&1; then
        log_success "$test_name"
        PASSED_TESTS=$((PASSED_TESTS + 1))
        return 0
    else
        log_warning "$test_name - Result is null (may be expected)"
        SKIPPED_TESTS=$((SKIPPED_TESTS + 1))
        return 2
    fi
}

# ========================================
# Pre-flight Checks
# ========================================

log_section "Pre-flight Checks"

log_info "Testing connection to Reth..."
if ! curl -s "$RETH_URL" > /dev/null 2>&1; then
    log_error "Cannot connect to Reth at $RETH_URL"
    exit 1
fi
log_success "Connected to Reth at $RETH_URL"

log_info "Getting chain info..."
CHAIN_ID=$(rpc_call "eth_chainId" "[]" | jq -r '.result')
LATEST_BLOCK=$(rpc_call "eth_blockNumber" "[]" | jq -r '.result')
LATEST_BLOCK_DEC=$((LATEST_BLOCK))

log_info "Chain ID: $CHAIN_ID"
log_info "Latest Block: $LATEST_BLOCK ($LATEST_BLOCK_DEC)"
log_info "Cutoff Block: $CUTOFF_BLOCK"

# Calculate test block numbers
LEGACY_BLOCK=$((CUTOFF_BLOCK - 1000))
LOCAL_BLOCK=$((CUTOFF_BLOCK + 1000))
BOUNDARY_BLOCK=$CUTOFF_BLOCK

LEGACY_BLOCK_HEX=$(printf "0x%x" $LEGACY_BLOCK)
LOCAL_BLOCK_HEX=$(printf "0x%x" $LOCAL_BLOCK)
BOUNDARY_BLOCK_HEX=$(printf "0x%x" $BOUNDARY_BLOCK)

log_info "Test Blocks:"
log_info "  Legacy Block:   $LEGACY_BLOCK_HEX ($LEGACY_BLOCK)"
log_info "  Boundary Block: $BOUNDARY_BLOCK_HEX ($BOUNDARY_BLOCK)"
log_info "  Local Block:    $LOCAL_BLOCK_HEX ($LOCAL_BLOCK)"

# ========================================
# Phase 1: Basic Block Query Tests
# ========================================

log_section "Phase 1: Basic Block Query Tests"

# Test 1.1: eth_getBlockByNumber (legacy)
log_info "Test 1.1: eth_getBlockByNumber (legacy block)"
response=$(rpc_call "eth_getBlockByNumber" "[\"$LEGACY_BLOCK_HEX\",false]")
check_result_not_null "$response" "eth_getBlockByNumber (legacy: $LEGACY_BLOCK_HEX)"

# Test 1.2: eth_getBlockByNumber (local)
log_info "Test 1.2: eth_getBlockByNumber (local block)"
if [ $LOCAL_BLOCK -le $LATEST_BLOCK_DEC ]; then
    response=$(rpc_call "eth_getBlockByNumber" "[\"$LOCAL_BLOCK_HEX\",false]")
    check_result_not_null "$response" "eth_getBlockByNumber (local: $LOCAL_BLOCK_HEX)"
else
    log_warning "Skipping local block test - block not yet mined"
    SKIPPED_TESTS=$((SKIPPED_TESTS + 1))
fi

# Test 1.3: eth_getBlockByNumber (boundary)
log_info "Test 1.3: eth_getBlockByNumber (boundary block)"
response=$(rpc_call "eth_getBlockByNumber" "[\"$BOUNDARY_BLOCK_HEX\",false]")
check_result_not_null "$response" "eth_getBlockByNumber (boundary: $BOUNDARY_BLOCK_HEX)"

# Test 1.4: eth_getBlockByNumber with full transactions
log_info "Test 1.4: eth_getBlockByNumber (full transactions)"
response=$(rpc_call "eth_getBlockByNumber" "[\"$LEGACY_BLOCK_HEX\",true]")
check_result_not_null "$response" "eth_getBlockByNumber (full tx)"

# ========================================
# Phase 2: Transaction Count Tests
# ========================================

log_section "Phase 2: Transaction Count Tests"

# Get a block hash first
BLOCK_HASH=$(rpc_call "eth_getBlockByNumber" "[\"$LEGACY_BLOCK_HEX\",false]" | jq -r '.result.hash')

# Test 2.1: eth_getBlockTransactionCountByNumber
log_info "Test 2.1: eth_getBlockTransactionCountByNumber"
response=$(rpc_call "eth_getBlockTransactionCountByNumber" "[\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getBlockTransactionCountByNumber"

# Test 2.2: eth_getBlockTransactionCountByHash
if [ "$BLOCK_HASH" != "null" ] && [ -n "$BLOCK_HASH" ]; then
    log_info "Test 2.2: eth_getBlockTransactionCountByHash"
    response=$(rpc_call "eth_getBlockTransactionCountByHash" "[\"$BLOCK_HASH\"]")
    check_result "$response" "eth_getBlockTransactionCountByHash"
else
    log_warning "Skipping hash test - no block hash available"
    SKIPPED_TESTS=$((SKIPPED_TESTS + 1))
fi

# ========================================
# Phase 3: Uncle Tests
# ========================================

log_section "Phase 3: Uncle Tests"

# Test 3.1: eth_getUncleCountByBlockNumber
log_info "Test 3.1: eth_getUncleCountByBlockNumber"
response=$(rpc_call "eth_getUncleCountByBlockNumber" "[\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getUncleCountByBlockNumber"

# Test 3.2: eth_getUncleCountByBlockHash
if [ "$BLOCK_HASH" != "null" ] && [ -n "$BLOCK_HASH" ]; then
    log_info "Test 3.2: eth_getUncleCountByBlockHash"
    response=$(rpc_call "eth_getUncleCountByBlockHash" "[\"$BLOCK_HASH\"]")
    check_result "$response" "eth_getUncleCountByBlockHash"
fi

# Test 3.3: eth_getUncleByBlockNumberAndIndex
log_info "Test 3.3: eth_getUncleByBlockNumberAndIndex"
response=$(rpc_call "eth_getUncleByBlockNumberAndIndex" "[\"$LEGACY_BLOCK_HEX\",\"0x0\"]")
check_result "$response" "eth_getUncleByBlockNumberAndIndex"

# ========================================
# Phase 4: Transaction Query Tests
# ========================================

log_section "Phase 4: Transaction Query Tests"

# Get a transaction hash first
log_info "Fetching a transaction hash from legacy block..."
TX_HASH=$(rpc_call "eth_getBlockByNumber" "[\"$LEGACY_BLOCK_HEX\",false]" | jq -r '.result.transactions[0]? // empty')

if [ -n "$TX_HASH" ] && [ "$TX_HASH" != "null" ]; then
    log_info "Found transaction: $TX_HASH"

    # Test 4.1: eth_getTransactionByHash
    log_info "Test 4.1: eth_getTransactionByHash (hash-based fallback)"
    response=$(rpc_call "eth_getTransactionByHash" "[\"$TX_HASH\"]")
    check_result_not_null "$response" "eth_getTransactionByHash"

    # Test 4.2: eth_getTransactionReceipt
    log_info "Test 4.2: eth_getTransactionReceipt (hash-based fallback)"
    response=$(rpc_call "eth_getTransactionReceipt" "[\"$TX_HASH\"]")
    check_result_not_null "$response" "eth_getTransactionReceipt"

    # Test 4.3: eth_getTransactionByBlockHashAndIndex
    log_info "Test 4.3: eth_getTransactionByBlockHashAndIndex"
    response=$(rpc_call "eth_getTransactionByBlockHashAndIndex" "[\"$BLOCK_HASH\",\"0x0\"]")
    check_result "$response" "eth_getTransactionByBlockHashAndIndex"

    # Test 4.4: eth_getTransactionByBlockNumberAndIndex
    log_info "Test 4.4: eth_getTransactionByBlockNumberAndIndex"
    response=$(rpc_call "eth_getTransactionByBlockNumberAndIndex" "[\"$LEGACY_BLOCK_HEX\",\"0x0\"]")
    check_result "$response" "eth_getTransactionByBlockNumberAndIndex"
else
    log_warning "No transactions found in legacy block, skipping transaction tests"
    SKIPPED_TESTS=$((SKIPPED_TESTS + 4))
fi

# ========================================
# Phase 5: State Query Tests
# ========================================

log_section "Phase 5: State Query Tests"

# Use a known address or get one from a block
TEST_ADDR="0x0000000000000000000000000000000000000000"

# Test 5.1: eth_getBalance
log_info "Test 5.1: eth_getBalance"
response=$(rpc_call "eth_getBalance" "[\"$TEST_ADDR\",\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getBalance (legacy block)"

# Test 5.2: eth_getCode
log_info "Test 5.2: eth_getCode"
response=$(rpc_call "eth_getCode" "[\"$TEST_ADDR\",\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getCode"

# Test 5.3: eth_getStorageAt
log_info "Test 5.3: eth_getStorageAt"
response=$(rpc_call "eth_getStorageAt" "[\"$TEST_ADDR\",\"0x0\",\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getStorageAt"

# Test 5.4: eth_getTransactionCount
log_info "Test 5.4: eth_getTransactionCount"
response=$(rpc_call "eth_getTransactionCount" "[\"$TEST_ADDR\",\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getTransactionCount"

# ========================================
# Phase 6: eth_call and eth_estimateGas Tests
# ========================================

log_section "Phase 6: Execution Tests"

# Test 6.1: eth_call
log_info "Test 6.1: eth_call (legacy block)"
CALL_DATA="{\"to\":\"$TEST_ADDR\",\"data\":\"0x\"}"
response=$(rpc_call "eth_call" "[$CALL_DATA,\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_call (legacy)"

# Test 6.2: eth_estimateGas
log_info "Test 6.2: eth_estimateGas (legacy block)"
response=$(rpc_call "eth_estimateGas" "[$CALL_DATA,\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_estimateGas (legacy)"

# ========================================
# Phase 7: eth_getLogs Tests
# ========================================

log_section "Phase 7: eth_getLogs Tests"

# Test 7.1: Pure legacy range
log_info "Test 7.1: eth_getLogs (pure legacy range)"
LEGACY_FROM=$((LEGACY_BLOCK - 10))
LEGACY_TO=$((LEGACY_BLOCK + 10))
LEGACY_FROM_HEX=$(printf "0x%x" $LEGACY_FROM)
LEGACY_TO_HEX=$(printf "0x%x" $LEGACY_TO)
response=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$LEGACY_FROM_HEX\",\"toBlock\":\"$LEGACY_TO_HEX\"}]")
check_result "$response" "eth_getLogs (pure legacy)"

# Test 7.2: Pure local range (if available)
if [ $LOCAL_BLOCK -le $LATEST_BLOCK_DEC ]; then
    log_info "Test 7.2: eth_getLogs (pure local range)"
    LOCAL_FROM=$((LOCAL_BLOCK - 10))
    LOCAL_TO=$((LOCAL_BLOCK + 10))
    LOCAL_FROM_HEX=$(printf "0x%x" $LOCAL_FROM)
    LOCAL_TO_HEX=$(printf "0x%x" $LOCAL_TO)
    response=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$LOCAL_FROM_HEX\",\"toBlock\":\"$LOCAL_TO_HEX\"}]")
    check_result "$response" "eth_getLogs (pure local)"
else
    log_warning "Skipping pure local getLogs test"
    SKIPPED_TESTS=$((SKIPPED_TESTS + 1))
fi

# Test 7.3: Cross-boundary range (THE MOST IMPORTANT TEST!)
log_info "Test 7.3: eth_getLogs (CROSS-BOUNDARY - Critical!)"
CROSS_FROM=$((CUTOFF_BLOCK - 50))
CROSS_TO=$((CUTOFF_BLOCK + 50))
CROSS_FROM_HEX=$(printf "0x%x" $CROSS_FROM)
CROSS_TO_HEX=$(printf "0x%x" $CROSS_TO)
response=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$CROSS_FROM_HEX\",\"toBlock\":\"$CROSS_TO_HEX\"}]")

if check_result "$response" "eth_getLogs (CROSS-BOUNDARY)"; then
    # Verify logs are sorted
    LOGS=$(echo "$response" | jq '.result')
    if [ "$LOGS" != "[]" ]; then
        # Check if block numbers are sorted
        IS_SORTED=$(echo "$LOGS" | jq '[.[].blockNumber] | . == sort')
        if [ "$IS_SORTED" = "true" ]; then
            log_success "  → Logs are properly sorted ✓"
        else
            log_error "  → Logs are NOT properly sorted ✗"
            FAILED_TESTS=$((FAILED_TESTS + 1))
            FAILED_TEST_NAMES+=("Log sorting verification")
        fi
    fi
fi

# ========================================
# Phase 8: Filter Tests
# ========================================

log_section "Phase 8: Filter Lifecycle Tests"

# Test 8.1: eth_newFilter (legacy range)
log_info "Test 8.1: eth_newFilter (legacy range)"
response=$(rpc_call "eth_newFilter" "[{\"fromBlock\":\"$LEGACY_FROM_HEX\",\"toBlock\":\"$LEGACY_TO_HEX\"}]")
if check_result_not_null "$response" "eth_newFilter (legacy)"; then
    FILTER_ID=$(echo "$response" | jq -r '.result')
    log_info "  → Created filter: $FILTER_ID"

    # Test 8.2: eth_getFilterLogs
    log_info "Test 8.2: eth_getFilterLogs"
    response=$(rpc_call "eth_getFilterLogs" "[\"$FILTER_ID\"]")
    check_result "$response" "eth_getFilterLogs"

    # Test 8.3: eth_getFilterChanges
    log_info "Test 8.3: eth_getFilterChanges"
    response=$(rpc_call "eth_getFilterChanges" "[\"$FILTER_ID\"]")
    check_result "$response" "eth_getFilterChanges"

    # Test 8.4: eth_uninstallFilter
    log_info "Test 8.4: eth_uninstallFilter"
    response=$(rpc_call "eth_uninstallFilter" "[\"$FILTER_ID\"]")
    check_result "$response" "eth_uninstallFilter"
else
    log_warning "Skipping filter tests - newFilter failed"
    SKIPPED_TESTS=$((SKIPPED_TESTS + 3))
fi

# Test 8.5: Cross-boundary filter (THE MOST IMPORTANT!)
log_info "Test 8.5: eth_newFilter (CROSS-BOUNDARY - Critical!)"
response=$(rpc_call "eth_newFilter" "[{\"fromBlock\":\"$CROSS_FROM_HEX\",\"toBlock\":\"$CROSS_TO_HEX\"}]")
if check_result_not_null "$response" "eth_newFilter (CROSS-BOUNDARY)"; then
    CROSS_FILTER_ID=$(echo "$response" | jq -r '.result')
    log_info "  → Created cross-boundary filter: $CROSS_FILTER_ID"

    # Get filter logs
    response=$(rpc_call "eth_getFilterLogs" "[\"$CROSS_FILTER_ID\"]")
    if check_result "$response" "eth_getFilterLogs (CROSS-BOUNDARY)"; then
        # Verify logs are sorted
        LOGS=$(echo "$response" | jq '.result')
        if [ "$LOGS" != "[]" ]; then
            IS_SORTED=$(echo "$LOGS" | jq '[.[].blockNumber] | . == sort')
            if [ "$IS_SORTED" = "true" ]; then
                log_success "  → Cross-boundary filter logs are properly sorted ✓"
            else
                log_error "  → Cross-boundary filter logs are NOT properly sorted ✗"
            fi
        fi
    fi

    # Cleanup
    rpc_call "eth_uninstallFilter" "[\"$CROSS_FILTER_ID\"]" > /dev/null
fi

# ========================================
# Phase 9: Additional Methods
# ========================================

log_section "Phase 9: Additional Methods"

# Test 9.1: eth_getBlockReceipts
log_info "Test 9.1: eth_getBlockReceipts"
response=$(rpc_call "eth_getBlockReceipts" "[\"$LEGACY_BLOCK_HEX\"]")
check_result "$response" "eth_getBlockReceipts"

# Test 9.2: eth_getBlockByHash
if [ "$BLOCK_HASH" != "null" ] && [ -n "$BLOCK_HASH" ]; then
    log_info "Test 9.2: eth_getBlockByHash"
    response=$(rpc_call "eth_getBlockByHash" "[\"$BLOCK_HASH\",false]")
    check_result_not_null "$response" "eth_getBlockByHash"
fi

# ========================================
# Phase 10: Edge Case Tests
# ========================================

log_section "Phase 10: Edge Case Tests (Boundary Conditions)"

# Non-existent hashes for testing
NON_EXISTENT_TX="0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
NON_EXISTENT_BLOCK="0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
INVALID_ADDRESS="0x0000000000000000000000000000000000000000"

# Test 10.1: eth_getTransactionByHash (non-existent)
log_info "Test 10.1: eth_getTransactionByHash (non-existent hash)"
response=$(rpc_call "eth_getTransactionByHash" "[\"$NON_EXISTENT_TX\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getTransactionByHash (non-existent) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getTransactionByHash (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getTransactionByHash (non-existent)")
fi

# Test 10.2: eth_getBlockByHash (non-existent)
log_info "Test 10.2: eth_getBlockByHash (non-existent hash)"
response=$(rpc_call "eth_getBlockByHash" "[\"$NON_EXISTENT_BLOCK\",false]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getBlockByHash (non-existent) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getBlockByHash (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getBlockByHash (non-existent)")
fi

# Test 10.3: eth_getTransactionReceipt (non-existent)
log_info "Test 10.3: eth_getTransactionReceipt (non-existent hash)"
response=$(rpc_call "eth_getTransactionReceipt" "[\"$NON_EXISTENT_TX\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getTransactionReceipt (non-existent) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getTransactionReceipt (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getTransactionReceipt (non-existent)")
fi

# Test 10.4: eth_getBlockByNumber (future block)
log_info "Test 10.4: eth_getBlockByNumber (future block)"
FUTURE_BLOCK="0xffffffff"  # Very large block number
response=$(rpc_call "eth_getBlockByNumber" "[\"$FUTURE_BLOCK\",false]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getBlockByNumber (future) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getBlockByNumber (future) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getBlockByNumber (future)")
fi

# Test 10.5: eth_getBalance (non-existent account, legacy block)
log_info "Test 10.5: eth_getBalance (zero balance account)"
response=$(rpc_call "eth_getBalance" "[\"$INVALID_ADDRESS\",\"$LEGACY_BLOCK_HEX\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == "0x0" and .error == null' > /dev/null 2>&1; then
    log_success "eth_getBalance (zero balance) - correctly returns 0x0"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getBalance (zero balance) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getBalance (zero balance)")
fi

# Test 10.6: eth_getCode (non-existent contract)
log_info "Test 10.6: eth_getCode (non-existent contract)"
response=$(rpc_call "eth_getCode" "[\"$INVALID_ADDRESS\",\"$LEGACY_BLOCK_HEX\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == "0x" and .error == null' > /dev/null 2>&1; then
    log_success "eth_getCode (non-existent) - correctly returns 0x"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getCode (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getCode (non-existent)")
fi

# Test 10.7: eth_getTransactionCount (zero nonce account)
log_info "Test 10.7: eth_getTransactionCount (zero nonce account)"
response=$(rpc_call "eth_getTransactionCount" "[\"$INVALID_ADDRESS\",\"$LEGACY_BLOCK_HEX\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == "0x0" and .error == null' > /dev/null 2>&1; then
    log_success "eth_getTransactionCount (zero nonce) - correctly returns 0x0"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getTransactionCount (zero nonce) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getTransactionCount (zero nonce)")
fi

# Test 10.8: eth_getStorageAt (non-existent storage)
log_info "Test 10.8: eth_getStorageAt (non-existent storage)"
response=$(rpc_call "eth_getStorageAt" "[\"$INVALID_ADDRESS\",\"0x0\",\"$LEGACY_BLOCK_HEX\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result and .error == null' > /dev/null 2>&1; then
    log_success "eth_getStorageAt (non-existent) - correctly returns value"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getStorageAt (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getStorageAt (non-existent)")
fi

# Test 10.9: eth_getBlockTransactionCountByHash (non-existent block)
log_info "Test 10.9: eth_getBlockTransactionCountByHash (non-existent)"
response=$(rpc_call "eth_getBlockTransactionCountByHash" "[\"$NON_EXISTENT_BLOCK\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getBlockTransactionCountByHash (non-existent) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getBlockTransactionCountByHash (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getBlockTransactionCountByHash (non-existent)")
fi

# Test 10.10: eth_getUncleCountByBlockHash (non-existent block)
log_info "Test 10.10: eth_getUncleCountByBlockHash (non-existent)"
response=$(rpc_call "eth_getUncleCountByBlockHash" "[\"$NON_EXISTENT_BLOCK\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getUncleCountByBlockHash (non-existent) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getUncleCountByBlockHash (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getUncleCountByBlockHash (non-existent)")
fi

# Test 10.11: eth_getLogs (empty result range)
log_info "Test 10.11: eth_getLogs (empty result range)"
EMPTY_FROM=$((LEGACY_BLOCK + 500))
EMPTY_TO=$((LEGACY_BLOCK + 501))
EMPTY_FROM_HEX=$(printf "0x%x" $EMPTY_FROM)
EMPTY_TO_HEX=$(printf "0x%x" $EMPTY_TO)
response=$(rpc_call "eth_getLogs" "[{\"fromBlock\":\"$EMPTY_FROM_HEX\",\"toBlock\":\"$EMPTY_TO_HEX\",\"address\":\"$NON_EXISTENT_BLOCK\"}]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == [] and .error == null' > /dev/null 2>&1; then
    log_success "eth_getLogs (empty result) - correctly returns []"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getLogs (empty result) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getLogs (empty result)")
fi

# Test 10.12: eth_getBlockReceipts (non-existent block)
log_info "Test 10.12: eth_getBlockReceipts (non-existent block)"
response=$(rpc_call "eth_getBlockReceipts" "[\"$FUTURE_BLOCK\"]")
TOTAL_TESTS=$((TOTAL_TESTS + 1))
if echo "$response" | jq -e '.result == null and .error == null' > /dev/null 2>&1; then
    log_success "eth_getBlockReceipts (non-existent) - correctly returns null"
    PASSED_TESTS=$((PASSED_TESTS + 1))
else
    log_error "eth_getBlockReceipts (non-existent) - unexpected response"
    FAILED_TESTS=$((FAILED_TESTS + 1))
    FAILED_TEST_NAMES+=("eth_getBlockReceipts (non-existent)")
fi

# ========================================
# Test Summary
# ========================================

log_section "Test Summary"

echo ""
echo "Total Tests:   $TOTAL_TESTS"
echo -e "${GREEN}Passed:        $PASSED_TESTS${NC}"
echo -e "${RED}Failed:        $FAILED_TESTS${NC}"
echo -e "${YELLOW}Skipped:       $SKIPPED_TESTS${NC}"
echo ""

if [ $FAILED_TESTS -gt 0 ]; then
    echo -e "${RED}Failed Tests:${NC}"
    for test_name in "${FAILED_TEST_NAMES[@]}"; do
        echo "  - $test_name"
    done
    echo ""
fi

# Calculate success rate
if [ $TOTAL_TESTS -gt 0 ]; then
    SUCCESS_RATE=$(awk "BEGIN {printf \"%.1f\", ($PASSED_TESTS / $TOTAL_TESTS) * 100}")
    echo "Success Rate: $SUCCESS_RATE%"
else
    echo "Success Rate: N/A"
fi

echo ""

# Final verdict
if [ $FAILED_TESTS -eq 0 ]; then
    echo -e "${GREEN}╔════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║   ✓ ALL TESTS PASSED!                 ║${NC}"
    echo -e "${GREEN}║   Legacy RPC is working correctly!    ║${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════╝${NC}"
    exit 0
else
    echo -e "${RED}╔════════════════════════════════════════╗${NC}"
    echo -e "${RED}║   ✗ SOME TESTS FAILED                  ║${NC}"
    echo -e "${RED}║   Please review the errors above       ║${NC}"
    echo -e "${RED}╚════════════════════════════════════════╝${NC}"
    exit 1
fi

