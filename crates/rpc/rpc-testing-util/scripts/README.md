# XLayer Mainnet Legacy RPC Testing

Integration test for XLayer mainnet migration from Erigon to Reth.

## XLayer Mainnet Configuration

**Migration Point:**
- Erigon last block: `42810020`
- Reth first block: `42810021`
- Legacy RPC: `https://xlayerrpc.okx.com`

## Start Reth with Legacy RPC

```bash
reth node \
  --http \
  --http.api eth \
  --legacy-rpc-url "https://xlayerrpc.okx.com" \
  --legacy-cutoff-block 42810021 \
  --legacy-rpc-timeout 30s
```

**Configuration Parameters:**
- `--legacy-rpc-url`: Legacy Erigon RPC endpoint
- `--legacy-cutoff-block`: First block served by Reth (blocks < this go to legacy)
- `--legacy-rpc-timeout`: Request timeout for legacy RPC

## Run Integration Test

```bash
./test_legacy_rpc.sh http://localhost:8545 42810021
```

This will test:
- ✅ Legacy routing (blocks < 42810021 → Erigon)
- ✅ Local routing (blocks ≥ 42810021 → Reth)
- ✅ Cross-boundary eth_getLogs (critical!)
- ✅ Hash-based fallback queries
- ✅ Filter lifecycle management
- ✅ Edge case handling (non-existent hashes, future blocks)
- ✅ 37+ RPC methods and edge cases total

**Requirements:** `curl`, `jq`

```bash
# Install dependencies
brew install jq  # macOS
# or
sudo apt-get install jq curl  # Ubuntu/Debian
```

## Test Output

The script will show:
- **Phase 1-9**: Core RPC method tests (blocks, transactions, state, logs, filters)
- **Phase 10**: Edge case tests (non-existent hashes, future blocks, zero balances)
- Detailed test results for each RPC method
- Success/failure counts
- Cross-boundary merge verification
- Log sorting validation
- Edge case handling verification

**Success criteria:** All tests pass (100%)

## Test Phases

| Phase | Description | Test Count |
|-------|-------------|------------|
| Phase 1 | Basic Block Queries | 4 tests |
| Phase 2 | Transaction Counts | 2 tests |
| Phase 3 | Uncle Queries | 3 tests |
| Phase 4 | Transaction Queries | 4 tests |
| Phase 5 | State Queries | 4 tests |
| Phase 6 | Execution Tests (call/estimateGas) | 2 tests |
| Phase 7 | eth_getLogs (cross-boundary critical) | 3 tests |
| Phase 8 | Filter Lifecycle | 5 tests |
| Phase 9 | Additional Methods | 2 tests |
| Phase 10 | **Edge Cases** | 12 tests |
| **Total** | | **41 tests** |

### Edge Case Tests (Phase 10)

Tests boundary conditions and error handling:
- Non-existent transaction hash
- Non-existent block hash
- Non-existent receipt
- Future block number
- Zero balance account
- Non-existent contract code
- Zero nonce account
- Non-existent storage
- Invalid block hashes
- Empty log results

