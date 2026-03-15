#!/bin/bash
#===============================================================================
#  DEVNET v2 REPORT (for full_load_devnet_simulation_v2.sh)
#===============================================================================
#
#  Prints a compact, human-readable comparison report from the two JSON files
#  produced by `full_load_devnet_simulation_v2.sh`.
#
#  Usage:
#    ./devnet_v2_report.sh <results_off.json> <results_on.json>
#    ./devnet_v2_report.sh <results_dir>
#
#  Examples:
#    ./devnet_v2_report.sh .devnet-sim-v2-20260305_202628
#    ./devnet_v2_report.sh .devnet-sim-v2-20260305_202628/results_off.json \
#      .devnet-sim-v2-20260305_202628/results_on.json
#
#  Notes:
#    - Cache hit/miss: reth_payloads_cached_reads_hits/misses (execution cache)
#    - Block timing:   reth_block_timing_build_exec_* and build_calc_state_root_*
#    - Prewarm stats:  reth_txpool_pre_warming_* (only meaningful when ON)
#===============================================================================

set -euo pipefail

if [ $# -lt 1 ]; then
  echo "Usage: $0 <results_off.json> <results_on.json>" >&2
  echo "   or: $0 <results_dir>" >&2
  exit 2
fi

OFF_JSON=""
ON_JSON=""

if [ $# -eq 1 ]; then
  # Directory mode
  if [ -d "$1" ]; then
    OFF_JSON="$1/results_off.json"
    ON_JSON="$1/results_on.json"
  else
    echo "Error: '$1' is not a directory and ON/OFF JSON paths weren't provided" >&2
    exit 2
  fi
else
  OFF_JSON="$1"
  ON_JSON="$2"
fi

if [ ! -f "$OFF_JSON" ]; then
  echo "Error: OFF json not found: $OFF_JSON" >&2
  exit 2
fi
if [ ! -f "$ON_JSON" ]; then
  echo "Error: ON json not found: $ON_JSON" >&2
  exit 2
fi

python3 - <<PY
import json
from pathlib import Path
import sys

def load_json(path: str):
    p = Path(path)
    try:
        return json.loads(p.read_text())
    except Exception as e:
        sys.stderr.write(f"Error: failed to parse JSON: {path}\n  {e}\n")
        try:
            head = "\n".join(p.read_text().splitlines()[:20])
            sys.stderr.write("--- file head (first 20 lines) ---\n")
            sys.stderr.write(head + "\n")
            sys.stderr.write("----------------------------------\n")
        except Exception:
            pass
        raise

off = load_json("$OFF_JSON")
on = load_json("$ON_JSON")

def f(num, digits=1):
    try:
        return f"{float(num):.{digits}f}"
    except Exception:
        return str(num)

def pct_change(off_v, on_v):
    try:
        off_v = float(off_v)
        on_v = float(on_v)
    except Exception:
        return "N/A"
    if off_v == 0:
        return "N/A"
    return f"{((on_v - off_v) / off_v) * 100:+.1f}%"

def safe_float(v, default=0.0):
    try:
        return float(v)
    except Exception:
        return default

def calc_hit_rate(h, m):
    total = h + m
    if total <= 0:
        return 0.0
    return (h / total) * 100.0

# Core fields
hit_off = safe_float(off.get("cache_hit_rate", 0) or 0)
hit_on = safe_float(on.get("cache_hit_rate", 0) or 0)

# Cache hit/miss counts (preferred, if present in JSON)
hits_off = int(safe_float(off.get("cache_hits", 0) or 0))
miss_off = int(safe_float(off.get("cache_misses", 0) or 0))
hits_on = int(safe_float(on.get("cache_hits", 0) or 0))
miss_on = int(safe_float(on.get("cache_misses", 0) or 0))

# If hit_rate wasn't in JSON for some reason, derive it from counts
if hit_off == 0 and (hits_off + miss_off) > 0:
    hit_off = calc_hit_rate(hits_off, miss_off)
if hit_on == 0 and (hits_on + miss_on) > 0:
    hit_on = calc_hit_rate(hits_on, miss_on)

tps_off = safe_float(off.get("tps", 0) or 0)
tps_on = safe_float(on.get("tps", 0) or 0)

tx_type = on.get("tx_type") or off.get("tx_type") or "unknown"

sent_off = int(safe_float(off.get("sent_success", 0) or 0))
sent_on = int(safe_float(on.get("sent_success", 0) or 0))
failed_off = int(safe_float(off.get("sent_failed", 0) or 0))
failed_on = int(safe_float(on.get("sent_failed", 0) or 0))

t_eth_off = int(safe_float(off.get("sent_eth", 0) or 0))
t_erc_off = int(safe_float(off.get("sent_erc20", 0) or 0))
t_eth_on = int(safe_float(on.get("sent_eth", 0) or 0))
t_erc_on = int(safe_float(on.get("sent_erc20", 0) or 0))

exec_off = safe_float(off.get("block_execution_ms", 0) or 0)
exec_on = safe_float(on.get("block_execution_ms", 0) or 0)

root_off = safe_float(off.get("state_root_ms", 0) or 0)
root_on = safe_float(on.get("state_root_ms", 0) or 0)

sims = int(safe_float(on.get("simulations", 0) or 0))
pref_ops = int(safe_float(on.get("prefetch_ops", 0) or 0))
pref_accts = int(safe_float(on.get("prefetch_accounts", 0) or 0))

# Print
print("=" * 80)
print("DEVNET v2 REPORT (full_load_devnet_simulation_v2)")
print("=" * 80)
print(f"TX type: {tx_type}")
print("")

print("LOAD")
print(f"  OFF: sent={sent_off} failed={failed_off} (eth={t_eth_off}, erc20={t_erc_off})")
print(f"   ON: sent={sent_on} failed={failed_on} (eth={t_eth_on}, erc20={t_erc_on})")
print("")

print("TPS")
print(f"  OFF: {f(tps_off)}")
print(f"   ON: {f(tps_on)}")
print(f"  Δ% : {pct_change(tps_off, tps_on)}")
print("")

print("EXECUTION CACHE")
print(f"  OFF: hits={hits_off} misses={miss_off} total={hits_off + miss_off} hit_rate={f(hit_off)}%")
print(f"   ON: hits={hits_on} misses={miss_on} total={hits_on + miss_on} hit_rate={f(hit_on)}%")
print(f"  Δpt: {hit_on - hit_off:+.1f} points")
print("")

print("BLOCK TIMING (avg, from node metrics)")
print(f"  Block exec (OFF): {f(exec_off, 4)} ms")
print(f"  Block exec ( ON): {f(exec_on, 4)} ms")
print(f"  Block exec Δ%   : {pct_change(exec_off, exec_on)}")
print(f"  State root (OFF): {f(root_off, 4)} ms")
print(f"  State root ( ON): {f(root_on, 4)} ms")
print(f"  State root Δ%   : {pct_change(root_off, root_on)}")
print("")

print("PRE-WARMING (ON phase only)")
print(f"  simulations_completed: {sims}")
print(f"  prefetch_operations  : {pref_ops}")
print(f"  prefetch_accounts    : {pref_accts}")
print("")

# Simple verdict line
verdict = []
if hit_on > hit_off:
    verdict.append("cache hit rate improved")
if tps_on > tps_off:
    verdict.append("TPS improved")
if exec_on and exec_off and exec_on < exec_off:
    verdict.append("block exec faster")
if root_on and root_off and root_on < root_off:
    verdict.append("state root faster")

print("SUMMARY")
if verdict:
    print("  " + "; ".join(verdict))
else:
    print("  no clear improvements detected")
print("=" * 80)
PY

