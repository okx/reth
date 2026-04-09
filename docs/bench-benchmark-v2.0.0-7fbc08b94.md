# Benchmark Report

| Key | Value |
|-----|-------|
| Date | 2026-04-09 19:34 |
| Branch | `benchmark-v2.0.0` |
| Commit | `7fbc08b94` |
| Commit Message | feat: add block builder finish timing logs and perf analysis scripts |
| Gas Limit | 2.5B |
| Sample Blocks (>=40k txs) | 154 |
| Total Payload Blocks | 233 |

## Table 1: Phase Breakdown

| Phase | Avg | % | Note |
|-------|----:|--:|------|
| **Total Block Interval** | ~1024ms | 100.0% |  |
| idle | ~155ms | 15.2% | waiting for block trigger |
| payload_build | ~705ms | 68.8% | build block |
| → txpool_next | ~55ms | 5.4% | fetch txs from pool |
| → tx_execute | ~293ms | 28.6% | execute txs |
| → state_root | ~124ms | 12.1% | compute state root |
| → assemble | ~196ms | 19.1% | assemble block |
| new_payload | ~76ms | 7.4% | validate block |
| fcu + commit | ~28ms | 2.7% | other overhead |

## Table 2: Summary

| Metric | Value |
|--------|------:|
| Sample Blocks (>=40k txs) | 154 |
| Txs / Block (avg) | 54,179 |
| **TPS** | **~52,891** |
| Gas Throughput | ~1.9 Ggas/s |
| Gas Utilization | ~76% |
| Persistence (avg) | ~417ms |

## Table 3: Percentile Distribution

| Phase | P50 | P95 | P99 |
|-------|----:|----:|----:|
| Block Interval | 995ms | 1382ms | 1469ms |
| idle | 141ms | 364ms | 732ms |
| payload_build | 707ms | 881ms | 896ms |
| txpool_next | 53ms | 84ms | 108ms |
| tx_execute | 292ms | 384ms | 389ms |
| state_root | 123ms | 135ms | 143ms |
| assemble | 194ms | 256ms | 269ms |
| new_payload | 74ms | 102ms | 114ms |
| persistence | 532ms | 727ms | 751ms |

| TPS | P50 | P75 | P90 | P95 | P99 |
|-----|----:|----:|----:|----:|----:|
| | 52,978 | 55,007 | 57,413 | 59,869 | 61,414 |

