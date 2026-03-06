# High Load Benchmark (500K+ Transactions)

Optimized benchmark script for testing pre-warming with large-scale transaction loads.

## Features

- **10 parallel senders** - Uses all available dev accounts
- **Configurable delays** - Simulate real-world transaction arrival patterns
- **Periodic metrics capture** - Track performance over time
- **Batch progress reporting** - Monitor long-running tests
- **Retry logic** - Handle nonce conflicts gracefully

## Usage

### Basic (500K ETH transfers)
```bash
./high_load_benchmark.sh --txns 500000 --senders 10 --tx-type eth
```

### Mixed mode (ETH + ERC20)
```bash
./high_load_benchmark.sh --txns 500000 --senders 10 --tx-type mixed --skip-build
```

### Full options
```bash
./high_load_benchmark.sh \
  --txns 500000 \
  --senders 10 \
  --tx-type mixed \
  --tx-delay 10 \
  --prewarm-workers 12 \
  --prefetch-workers 12 \
  --skip-build
```

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--txns` | 500000 | Total transactions to send |
| `--senders` | 10 | Number of parallel senders (max 10) |
| `--tx-type` | eth | Transaction type: `eth`, `erc20`, `mixed` |
| `--tx-delay` | 5 | Random delay 0-N ms between txs (realistic mode) |
| `--prewarm-workers` | CPU count | Pre-warming simulation workers |
| `--prefetch-workers` | CPU count | Prefetch workers |
| `--batch-size` | 10000 | Metrics capture interval |
| `--block-time` | 1 | Block time in seconds |
| `--skip-build` | false | Skip cargo build step |

## Expected Runtime

For 500K transactions with 10 senders and 5ms average delay:
- **Per sender:** 50K txs × 2.5ms avg = ~125 seconds
- **Total:** ~2-3 minutes per phase (OFF and ON)
- **Full benchmark:** ~5-10 minutes

## Output

Results saved to: `.high-load-benchmark-YYYYMMDD_HHMMSS/`

```
├── results_off.json       # Pre-warming OFF metrics
├── results_on.json        # Pre-warming ON metrics
├── OFF_node.log           # Node logs (OFF phase)
├── ON_node.log            # Node logs (ON phase)
├── OFF_sender_*.out       # Per-sender output (OFF)
├── ON_sender_*.out        # Per-sender output (ON)
├── OFF_snapshots.log      # Periodic metrics snapshots
├── ON_snapshots.log       # Periodic metrics snapshots
└── data-off/, data-on/    # Node data directories
```

## Sample Report

```
┌──────────────────────────────────────────────────────────────────────────────┐
│  HIGH LOAD BENCHMARK RESULTS                                                │
├──────────────────────────────────────────────────────────────────────────────┤
│  Total Transactions:      500,000                                     │
│  Parallel Senders:             10                                     │
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│  TPS (Transactions Per Second)                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│  Pre-warming OFF:        125.0 TPS                                      │
│  Pre-warming ON:         118.5 TPS                                      │
│  Change:                  -5.2%                                        │
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│  CACHE HIT RATE                                                             │
├──────────────────────────────────────────────────────────────────────────────┤
│  Pre-warming OFF:        28.5%                                          │
│  Pre-warming ON:         85.2%                                          │
│  IMPROVEMENT:           +56.7%                                          │
└──────────────────────────────────────────────────────────────────────────────┘
```

