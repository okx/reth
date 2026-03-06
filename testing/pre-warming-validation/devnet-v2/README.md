# Devnet v2 Benchmark Suite

Pre-warming benchmark tools for testing `op-reth` with parallel transaction sending and comprehensive metrics capture.

---

## Files

| File | Description |
|------|-------------|
| `full_load_devnet_simulation_v2.sh` | Main benchmark script - runs 2-phase test (pre-warming OFF vs ON) |
| `devnet_v2_report.sh` | Report generator - prints comparison from JSON results |

---

## Prerequisites

### 1. Build op-reth with pre-warming feature

```bash
cd /Users/lakshmikanth/Documents/optimisation/reth
cargo build --release --package op-reth --features pre-warming
```

### 2. Install Python dependencies

```bash
pip3 install web3 eth-account
```

---

## Usage

### Run Benchmark

```bash
cd /Users/lakshmikanth/Documents/optimisation/reth

# Kill any running node first
pkill -9 op-reth 2>/dev/null || true

# Run benchmark
./testing/pre-warming-validation/devnet-v2/full_load_devnet_simulation_v2.sh [OPTIONS]
```

### Generate Report

```bash
# From latest run directory
DIR=$(ls -td .devnet-sim-v2-* | head -1)
./testing/pre-warming-validation/devnet-v2/devnet_v2_report.sh "$DIR"

# Or pass JSON files directly
./testing/pre-warming-validation/devnet-v2/devnet_v2_report.sh \
  .devnet-sim-v2-YYYYMMDD_HHMMSS/results_off.json \
  .devnet-sim-v2-YYYYMMDD_HHMMSS/results_on.json
```

---

## CLI Options

### full_load_devnet_simulation_v2.sh

| Option | Default | Description |
|--------|---------|-------------|
| `--txns N` | 50000 | Total transactions to send per phase |
| `--senders N` | 10 | Number of parallel sender processes (max: 10) |
| `--tx-type TYPE` | eth | Transaction type: `eth`, `erc20`, or `mixed` |
| `--prewarm-workers N` | (all CPUs) | Number of pre-warming simulation workers |
| `--prefetch-workers N` | (all CPUs) | Number of pre-fetch workers |
| `--burst N` | 100 | Transactions per sender batch (internal) |
| `--skip-build` | false | Skip cargo build step |

### Transaction Types

| Type | Description |
|------|-------------|
| `eth` | Simple ETH transfers (21,000 gas each) - fastest |
| `erc20` | ERC20 token transfers via deployed contract |
| `mixed` | ~50% ETH, ~25% ERC20 transfer, ~25% ERC20 transferFrom |

---

## Examples

### Quick Test (small load)

```bash
./testing/pre-warming-validation/devnet-v2/full_load_devnet_simulation_v2.sh \
  --skip-build \
  --txns 1000 \
  --senders 5 \
  --tx-type eth
```

### Full Mixed Load with Max Workers

```bash
./testing/pre-warming-validation/devnet-v2/full_load_devnet_simulation_v2.sh \
  --txns 50000 \
  --senders 10 \
  --tx-type mixed \
  --prewarm-workers 12 \
  --prefetch-workers 12
```

### ERC20-Only High Load

```bash
./testing/pre-warming-validation/devnet-v2/full_load_devnet_simulation_v2.sh \
  --skip-build \
  --txns 20000 \
  --senders 10 \
  --tx-type erc20 \
  --prewarm-workers 12 \
  --prefetch-workers 12
```

### Skip Build (binary already compiled)

```bash
./testing/pre-warming-validation/devnet-v2/full_load_devnet_simulation_v2.sh \
  --skip-build \
  --txns 5000 \
  --senders 10 \
  --tx-type mixed
```

---

## Output

Results are saved to `.devnet-sim-v2-YYYYMMDD_HHMMSS/` in the repo root:

| File | Description |
|------|-------------|
| `results_off.json` | Metrics from pre-warming OFF phase |
| `results_on.json` | Metrics from pre-warming ON phase |
| `summary.txt` | Human-readable summary |
| `node_off.log` | Node logs (OFF phase) |
| `node_on.log` | Node logs (ON phase) |
| `sender_N.out` | Per-sender output files |

---

## Report Fields

The `devnet_v2_report.sh` output includes:

### EXECUTION CACHE (most important)
```
  OFF: hits=115 misses=816 total=931 hit_rate=12.4%
   ON: hits=869 misses=143 total=1012 hit_rate=85.9%
  Δpt: +73.5 points
```

### TPS
```
  OFF: 285.7
   ON: 142.9
  Δ% : -50.0%
```

### BLOCK TIMING
```
  Block exec (OFF): 0.7224 ms
  Block exec ( ON): 0.4378 ms
  Block exec Δ%   : -39.4%
```

### PRE-WARMING STATS
```
  simulations_completed: 2001
  prefetch_operations  : 31
  prefetch_accounts    : 14040
```

---

## Metrics Captured

| Metric Key | Description |
|------------|-------------|
| `reth_payloads_cached_reads_hits` | Execution cache hits |
| `reth_payloads_cached_reads_misses` | Execution cache misses |
| `reth_txpool_pre_warming_simulations_completed` | Simulations run |
| `reth_txpool_pre_warming_prefetch_operations` | Prefetch ops executed |
| `reth_txpool_pre_warming_prefetch_accounts` | Accounts prefetched |
| `reth_block_timing_build_exec_mempool_transactions_*` | Block execution time |
| `reth_block_timing_build_calc_state_root_*` | State root calculation time |

---

## Troubleshooting

### "sender is not an EOA"
This happens in Optimism dev mode when deposit transactions add bytecode to the dev account. Solution: use `--senders 10` with fresh datadir (script handles this automatically).

### JSON parse errors in report
If `devnet_v2_report.sh` fails, the JSON files may be truncated. Re-run the benchmark to generate fresh results.

### Low TPS with pre-warming ON
Pre-warming adds overhead. The benefit is **cache hit rate improvement** and **block timing reduction**, not raw TPS increase during load generation.

