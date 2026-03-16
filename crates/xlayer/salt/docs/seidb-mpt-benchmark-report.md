# sei-db MPT Benchmark Report

## 1. Environment

- **CPU**: (fill after run)
- **Cores**: (fill after run)
- **Memory**: (fill after run)
- **OS**: (fill after run)
- **Rust toolchain**: (fill after run)
- **Commands**:
  ```bash
  # Golden tests
  cargo test -p seidb-sc --test mpt_reth_golden

  # Scale tests (manual)
  cargo test -p seidb-sc --test mpt_scale -- --ignored --nocapture

  # Micro bench (sei-db MPT only)
  cargo bench -p seidb-sc --bench mpt_commit_bench

  # Micro bench with asm-keccak
  cargo bench -p seidb-sc --bench mpt_commit_bench --features asm-keccak

  # End-to-end: sei-db MPT vs reth MPT+MDBX
  cargo bench -p xlayer-salt --bench seidb_vs_reth

  # End-to-end with asm-keccak
  cargo bench -p xlayer-salt --bench seidb_vs_reth --features asm-keccak

  # Large-scale end-to-end (manual)
  BENCH_LARGE=1 cargo bench -p xlayer-salt --bench seidb_vs_reth
  ```

## 2. Golden Test Results (G1.*)

| Test | Description | Result |
|------|-------------|--------|
| G1.1 | empty state == EMPTY_ROOT_HASH | |
| G1.2 | single EOA root matches reth | |
| G1.3 | contract + 1 slot matches reth | |
| G1.4 | contract + 20 slots matches reth | |
| G1.5 | zero slot delete matches reth | |
| G1.6 | wipe + recreate matches reth | |
| G1.7 | overlapping prefixes matches reth | |
| G1.8 | multi-block sequence matches reth | |
| G1.9 | rollback + recommit matches reth | |
| G1.10 | prune+gc preserves latest root | |
| G1.11 | account_proof verify latest | |
| G1.12 | historical proof verify | |
| G1.13 | snapshot roundtrip root match | |

## 3. Micro Bench (B3.*)

### B3.1: Account-heavy (no storage)

| Accounts | Default keccak | asm-keccak |
|----------|---------------|------------|
| 100 | | |
| 1,000 | | |
| 10,000 | | |

### B3.2: Storage-heavy

| Accounts x Slots | Default keccak | asm-keccak |
|-------------------|---------------|------------|
| 100 x 10 | | |
| 100 x 100 | | |
| 1,000 x 10 | | |

### B3.3: Mixed workload

| Metric | Value |
|--------|-------|
| Accounts | 500 |
| Time | |

### B3.4: Multi-block incremental

| Blocks | Time | Per-block avg |
|--------|------|---------------|
| 10 | | |
| 100 | | |

## 4. End-to-End Baseline (B4.*)

### B4.1: Fresh-state one-shot (100 accounts, 10 slots each)

| Engine | Time |
|--------|------|
| sei-db MPT | |
| reth MPT+MDBX | |

### B4.2: Pre-populated + single block (1K pre-pop, 200 updates)

| Engine | Time |
|--------|------|
| sei-db MPT | |
| reth MPT+MDBX | |

### B4.3: 10 blocks incremental (1K pre-pop, 200 updates/block)

| Engine | Total | Per-block avg |
|--------|-------|---------------|
| sei-db MPT | | |
| reth MPT+MDBX | | |

### B4.4: Large-scale (200K pre-pop + 2K updates/block, 10 blocks) [manual]

| Engine | Total | Per-block avg |
|--------|-------|---------------|
| sei-db MPT | | |
| reth MPT+MDBX | | |

## 5. Scale Tests (S2.*)

| Test | Accounts | Slots | Commit OK | Reopen Consistent | Root |
|------|----------|-------|-----------|-------------------|------|
| S2.1 | 100K | 0 | | | |
| S2.2 | 100K | 4/acct | | | |
| S2.3 | 1M | 0 | | | |
| S2.4 | 1M | sparse | | | |
| S2.5 | 10-block incremental | varied | | | |

## 6. Conclusions

### Correctness
- (fill after golden tests)

### Performance bottlenecks
- (fill after benchmarks)

### Optimization directions
- (fill after analysis)
