# XLayer Legacy RPC Testing

Integration test for XLayer migration from Erigon to Reth with Legacy RPC fallback support.

## Network Configurations

### XLayer Testnet Configuration

**Migration Point:**
- Erigon last block: `12241700`
- Reth first block: `12241701`
- Legacy RPC: `https://testrpc.xlayer.tech`

### XLayer Mainnet Configuration

**Migration Point:**
- Erigon last block: `42810020`
- Reth first block: `42810021`
- Legacy RPC: `https://xlayerrpc.okx.com`

## Quick Start

### Start Reth with Legacy RPC (Testnet)

```bash
reth node \
  --http \
  --http.api eth \
  --legacy-rpc-url "https://testrpc.xlayer.tech" \
  --legacy-cutoff-block 12241701 \
  --legacy-rpc-timeout 30s
```

### Start Reth with Legacy RPC (Mainnet)

```bash
reth node \
  --http \
  --http.api eth \
  --legacy-rpc-url "https://xlayerrpc.okx.com" \
  --legacy-cutoff-block 42810021 \
  --legacy-rpc-timeout 30s
```

**Configuration Parameters:**
- `--legacy-rpc-url`: Legacy Erigon RPC endpoint URL
- `--legacy-cutoff-block`: First block served by Reth (blocks < this go to legacy RPC)
- `--legacy-rpc-timeout`: Request timeout for legacy RPC calls (default: 30s)

## Run Integration Test

### Test Testnet

```bash
# Use default RPC URL (http://localhost:8545)
./test_legacy_rpc.sh testnet

# Or specify custom RPC URL
./test_legacy_rpc.sh testnet http://localhost:8545
```

### Test Mainnet

```bash
# Use default RPC URL (http://localhost:8545)
./test_legacy_rpc.sh mainnet

# Or specify custom RPC URL
./test_legacy_rpc.sh mainnet http://your-reth-node:8545
```

### Command Syntax

```bash
./test_legacy_rpc.sh <network> [reth_url]

Arguments:
  network   - Required: "mainnet" or "testnet"
  reth_url  - Optional: RPC endpoint (default: http://localhost:8545)
```

**Note**: The script automatically sets the correct cutoff block based on the network:
- **Testnet**: 12241701
- **Mainnet**: 42810021

## What This Tests

This comprehensive test suite validates:
- ✅ Legacy routing (blocks < cutoff → Erigon)
- ✅ Local routing (blocks ≥ cutoff → Reth)
- ✅ **Cross-boundary eth_getLogs** (critical! merges results from both sources)
- ✅ Hash-based fallback queries (automatic fallback when block not found locally)
- ✅ Filter lifecycle management (create, query, uninstall)
- ✅ Edge case handling (non-existent hashes, future blocks, zero balances)
- ✅ **41 total test cases** covering all major RPC methods

## Prerequisites

**Required tools:** `curl`, `jq`

```bash
# Install on macOS
brew install jq curl

# Install on Ubuntu/Debian
sudo apt-get install jq curl

# Install on RHEL/CentOS
sudo yum install jq curl
```

## Test Output

The test script provides detailed results showing:

### Test Phases (1-10)
- **Phase 1-9**: Core RPC method tests (blocks, transactions, state, logs, filters)
- **Phase 10**: Edge case tests (boundary conditions, error handling)

### Results Include
- ✅ Pass/fail status for each test
- 🔍 Detailed error messages on failures
- 📊 Success rate percentage
- ⚠️ Cross-boundary merge verification
- 📝 Log sorting validation
- 🎯 Edge case handling verification

### Success Criteria
- All 41 tests pass (100% success rate)
- Cross-boundary queries properly merge results
- Logs are correctly sorted by block number
- Edge cases return expected responses

## Test Coverage Details

### Test Phases Overview

| Phase | Description | Tests | Critical |
|-------|-------------|-------|----------|
| **Phase 1** | Basic Block Queries | 4 | ⭐ |
| **Phase 2** | Transaction Counts | 2 | |
| **Phase 3** | Uncle Queries | 3 | |
| **Phase 4** | Transaction Queries | 4 | ⭐ |
| **Phase 5** | State Queries | 4 | ⭐ |
| **Phase 6** | Execution Tests | 2 | |
| **Phase 7** | eth_getLogs Tests | 3 | ⭐⭐⭐ |
| **Phase 8** | Filter Lifecycle | 5 | ⭐⭐ |
| **Phase 9** | Additional Methods | 2 | |
| **Phase 10** | Edge Cases | 12 | ⭐ |
| **Total** | | **41** | |

### Phase 1: Basic Block Queries
- `eth_getBlockByNumber` (legacy, local, boundary)
- `eth_getBlockByNumber` with full transactions

### Phase 2: Transaction Counts
- `eth_getBlockTransactionCountByNumber`
- `eth_getBlockTransactionCountByHash`

### Phase 3: Uncle Queries
- `eth_getUncleCountByBlockNumber`
- `eth_getUncleCountByBlockHash`
- `eth_getUncleByBlockNumberAndIndex`

### Phase 4: Transaction Queries
- `eth_getTransactionByHash` (hash-based fallback)
- `eth_getTransactionReceipt` (hash-based fallback)
- `eth_getTransactionByBlockHashAndIndex`
- `eth_getTransactionByBlockNumberAndIndex`

### Phase 5: State Queries
- `eth_getBalance` at historical block
- `eth_getCode` at historical block
- `eth_getStorageAt` at historical block
- `eth_getTransactionCount` at historical block

### Phase 6: Execution Tests
- `eth_call` at legacy block
- `eth_estimateGas` at legacy block

### Phase 7: eth_getLogs Tests ⭐⭐⭐ CRITICAL
- Pure legacy range (blocks entirely in Erigon)
- Pure local range (blocks entirely in Reth)
- **Cross-boundary range** (spans both Erigon and Reth)
  - Tests result merging from both sources
  - Validates proper log sorting by block number
  - **Most important test for migration success!**

### Phase 8: Filter Lifecycle
- `eth_newFilter` (legacy range)
- `eth_getFilterLogs`
- `eth_getFilterChanges`
- `eth_uninstallFilter`
- **Cross-boundary filter** (critical!)

### Phase 9: Additional Methods
- `eth_getBlockReceipts`
- `eth_getBlockByHash`

### Phase 10: Edge Cases
Validates proper error handling for:
- Non-existent transaction hash → `null`
- Non-existent block hash → `null`
- Non-existent receipt → `null`
- Future block number → `null`
- Zero balance account → `"0x0"`
- Non-existent contract code → `"0x"`
- Zero nonce account → `"0x0"`
- Non-existent storage → valid zero value
- Invalid block transaction count → `null`
- Invalid uncle count → `null`
- Empty log result set → `[]`
- Non-existent block receipts → `null`

## Example Test Results

```
========================================
Test Summary
========================================

Total Tests:   41
Passed:        41
Failed:        0
Skipped:       0

Success Rate: 100.0%

╔════════════════════════════════════════╗
║   ✓ ALL TESTS PASSED!                 ║
║   Legacy RPC is working correctly!    ║
╚════════════════════════════════════════╝
```

## Troubleshooting

### Common Issues

**Issue: Connection refused**
```
Solution: Ensure Reth is running and HTTP API is enabled
```

**Issue: Legacy RPC timeout**
```
Solution: Increase --legacy-rpc-timeout or check network connectivity
```

**Issue: Cross-boundary tests fail**
```
Solution: Verify cutoff_block matches your migration point exactly
```

**Issue: Some tests skipped**
```
Reason: Block not yet mined or no transactions in test block
Status: Normal for newly synced nodes
```

## Advanced Usage

### Custom RPC Endpoint and Port
```bash
# Test with custom port
./test_legacy_rpc.sh testnet http://localhost:9545

# Test remote node
./test_legacy_rpc.sh mainnet http://your-reth-node.example.com:8545

# Test with IP address
./test_legacy_rpc.sh testnet http://192.168.1.100:8545
```

### Test Specific Block Range
The script automatically calculates test blocks based on the cutoff block:
```bash
LEGACY_BLOCK  = CUTOFF_BLOCK - 1000  # Tests legacy routing
BOUNDARY_BLOCK = CUTOFF_BLOCK         # Tests exact cutoff
LOCAL_BLOCK   = CUTOFF_BLOCK + 1000  # Tests local data
```

To adjust the test range, edit these values in the script:
```bash
LEGACY_BLOCK=$((CUTOFF_BLOCK - 1000))  # Change offset as needed
LOCAL_BLOCK=$((CUTOFF_BLOCK + 1000))   # Change offset as needed
```

## Migration Validation Checklist

Before declaring migration successful, ensure:

- [ ] All 41 tests pass (100% success rate)
- [ ] Cross-boundary `eth_getLogs` works correctly
- [ ] Cross-boundary filters work correctly
- [ ] Hash-based fallback queries work (non-local blocks)
- [ ] Edge cases return expected responses
- [ ] No errors in Reth logs during test execution
- [ ] Performance is acceptable (< 30s timeout)

## Notes

- Tests are designed to be **non-destructive** (read-only queries)
- Safe to run against production nodes
- Can be run repeatedly for regression testing
- Results may vary slightly based on chain state at test time

