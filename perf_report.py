#!/usr/bin/env python3
"""Analyze reth benchmark logs and generate markdown report.

Usage:
    ./bench.py                  # analyze ./reth.log
    ./bench.py path/to/reth.log # analyze custom log file
"""

import re
import subprocess
import sys
from datetime import datetime
from pathlib import Path


def strip_ansi(s):
    return re.sub(r'\x1b\[[0-9;]*m', '', s)


def parse_timestamp(line):
    m = re.search(r'(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z', line)
    if m:
        ts = m.group(1)
        parts = ts.split('.')
        if len(parts) == 2:
            parts[1] = parts[1][:6]
            ts = '.'.join(parts)
        return datetime.strptime(ts, '%Y-%m-%dT%H:%M:%S.%f')
    return None


def parse_payload_line(line):
    line = strip_ansi(line)
    if "payload build stage timing" not in line:
        return None
    ts = parse_timestamp(line)
    if not ts:
        return None
    fields = {}
    for key in ['txs_considered', 'txs_executed']:
        m = re.search(rf'{key}=(\d+)', line)
        if m:
            fields[key] = int(m.group(1))
    for key in ['txpool_next_ms', 'tx_execute_ms', 'finish_ms', 'payload_build_total_ms']:
        m = re.search(rf'{key}=([\d.]+)', line)
        if m:
            fields[key] = float(m.group(1))
    return ts, fields


def parse_finish_line(line):
    """Parse 'block builder finish timing' log for state_root_ms and assemble_ms."""
    line = strip_ansi(line)
    if "block builder finish timing" not in line:
        return None
    fields = {}
    for key in ['state_root_ms', 'assemble_ms']:
        m = re.search(rf'{key}=([\d.]+)', line)
        if m:
            fields[key] = float(m.group(1))
    return fields if fields else None


def parse_miner_line(line):
    line = strip_ansi(line)
    if "miner advance timing" not in line:
        return None
    ts = parse_timestamp(line)
    if not ts:
        return None
    fields = {}
    m = re.search(r'block_number=(\d+)', line)
    if m:
        fields['block_number'] = int(m.group(1))
    for key in ['cycle_ms', 'idle_ms', 'advance_ms', 'fcu_ms', 'resolve_ms', 'new_payload_ms']:
        m = re.search(rf'{key}=([\d.]+)', line)
        if m:
            fields[key] = float(m.group(1))
    return ts, fields


def parse_persistence_line(line):
    line = strip_ansi(line)
    if "Saved range of blocks" not in line:
        return None
    m = re.search(r'elapsed=([\d.]+)(µs|ms|s)', line)
    if not m:
        return None
    val = float(m.group(1))
    unit = m.group(2)
    if unit == 'µs':
        val /= 1000
    elif unit == 's':
        val *= 1000
    return val


def parse_gas_limit(line):
    line = strip_ansi(line)
    m = re.search(r'gas_limit=([\d.]+)([KMG]?)gas', line)
    if m:
        val = float(m.group(1))
        unit = m.group(2)
        if unit == 'K':
            val *= 1_000
        elif unit == 'M':
            val *= 1_000_000
        elif unit == 'G':
            val *= 1_000_000_000
        return int(val)
    return None


def pct(values, p):
    if not values:
        return 0
    s = sorted(values)
    k = (len(s) - 1) * p / 100
    f = int(k)
    c = f + 1
    if c >= len(s):
        return s[f]
    return s[f] + (k - f) * (s[c] - s[f])


def avg(values):
    return sum(values) / len(values) if values else 0


def git_info():
    def run(cmd):
        try:
            return subprocess.check_output(cmd, shell=True, stderr=subprocess.DEVNULL).decode().strip()
        except Exception:
            return "unknown"

    return {
        'commit': run("git rev-parse --short HEAD"),
        'branch': run("git rev-parse --abbrev-ref HEAD"),
        'commit_msg': run("git log -1 --pretty=%s"),
    }


def load_log(log_path):
    if not log_path.exists():
        print(f"Error: {log_path} not found")
        sys.exit(1)

    payloads = []
    miners = []
    persists = []
    gas_limit = None
    pending_finish = None

    with open(log_path) as f:
        for line in f:
            # "block builder finish timing" appears right before "payload build stage timing"
            fl = parse_finish_line(line)
            if fl:
                pending_finish = fl
                continue
            p = parse_payload_line(line)
            if p:
                if pending_finish:
                    p[1].update(pending_finish)
                    pending_finish = None
                payloads.append(p)
                continue
            m = parse_miner_line(line)
            if m:
                miners.append(m)
            per = parse_persistence_line(line)
            if per is not None:
                persists.append(per)
            if gas_limit is None:
                gl = parse_gas_limit(line)
                if gl:
                    gas_limit = gl

    # Block intervals from consecutive payload timestamps
    intervals = []
    for i in range(1, len(payloads)):
        iv = (payloads[i][0] - payloads[i - 1][0]).total_seconds() * 1000
        intervals.append((iv, payloads[i][1]))

    # Filter blocks with >=40k txs
    high = [(iv, f) for iv, f in intervals if f.get('txs_executed', 0) >= 40000]
    high_miners = [f for _, f in miners if f.get('advance_ms', 0) > 200]

    return {
        'high': high,
        'high_miners': high_miners,
        'persists': persists,
        'gas_limit': gas_limit or 2_500_000_000,
        'total_payloads': len(payloads),
    }


def generate_report(data, git):
    high = data['high']
    hm = data['high_miners']
    persists = data['persists']
    gas_limit = data['gas_limit']

    if not high:
        print("Error: no blocks with >=40k txs found in log")
        sys.exit(1)

    # Collect raw values
    ivs = [iv for iv, _ in high]
    txs_list = [f['txs_executed'] for _, f in high]
    builds = [f['payload_build_total_ms'] for _, f in high]
    txpools = [f['txpool_next_ms'] for _, f in high]
    executes = [f['tx_execute_ms'] for _, f in high]
    state_roots = [f.get('state_root_ms', 0) for _, f in high]
    assembles = [f.get('assemble_ms', 0) for _, f in high]
    new_payloads = [f['new_payload_ms'] for f in hm] if hm else []
    idles = [f['idle_ms'] for f in hm] if hm else []
    fcu_others = [
        f['cycle_ms'] - f['idle_ms'] - f['fcu_ms'] - f['resolve_ms'] - f['new_payload_ms']
        for f in hm
    ] if hm else []
    fcus = [f['fcu_ms'] for f in hm] if hm else []

    a_iv = avg(ivs)
    a_txs = avg(txs_list)
    a_build = avg(builds)
    a_txpool = avg(txpools)
    a_execute = avg(executes)
    a_state_root = avg(state_roots)
    a_assemble = avg(assembles)
    a_np = avg(new_payloads)
    a_idle = avg(idles)
    a_fcu_other = avg(fcus) + avg(fcu_others)
    a_persist = avg(persists)

    iv_s = a_iv / 1000
    tps = a_txs / iv_s if iv_s > 0 else 0
    max_txs = max(txs_list)
    gas_per_tx = gas_limit / max_txs if max_txs > 0 else 51000
    gas_used = a_txs * gas_per_tx
    ggas_s = gas_used / iv_s / 1e9 if iv_s > 0 else 0
    gas_util = gas_used / gas_limit * 100

    now = datetime.now().strftime('%Y-%m-%d %H:%M')

    lines = []
    w = lines.append

    # === Header ===
    w(f"# Benchmark Report")
    w("")
    w(f"| Key | Value |")
    w(f"|-----|-------|")
    w(f"| Date | {now} |")
    w(f"| Branch | `{git['branch']}` |")
    w(f"| Commit | `{git['commit']}` |")
    w(f"| Commit Message | {git['commit_msg']} |")
    w(f"| Gas Limit | {gas_limit / 1e9:.1f}B |")
    w(f"| Sample Blocks (>=40k txs) | {len(high)} |")
    w(f"| Total Payload Blocks | {data['total_payloads']} |")
    w("")

    # === Table 1: Phase Breakdown ===
    w("## Table 1: Phase Breakdown")
    w("")
    w("| Phase | Avg | % | Note |")
    w("|-------|----:|--:|------|")

    def row(name, ms, note="", bold=False):
        p = ms / a_iv * 100 if a_iv > 0 else 0
        label = f"**{name}**" if bold else name
        w(f"| {label} | ~{int(round(ms))}ms | {p:.1f}% | {note} |")

    row("Total Block Interval", a_iv, bold=True)
    row("idle", a_idle, "waiting for block trigger")
    row("payload_build", a_build, "build block")
    row("→ txpool_next", a_txpool, "fetch txs from pool")
    row("→ tx_execute", a_execute, "execute txs")
    row("→ state_root", a_state_root, "compute state root")
    row("→ assemble", a_assemble, "assemble block")
    row("new_payload", a_np, "validate block")
    row("fcu + commit", a_fcu_other, "other overhead")

    w("")

    # === Table 2: Summary ===
    w("## Table 2: Summary")
    w("")
    w("| Metric | Value |")
    w("|--------|------:|")
    w(f"| Sample Blocks (>=40k txs) | {len(high)} |")
    w(f"| Txs / Block (avg) | {a_txs:,.0f} |")
    w(f"| **TPS** | **~{tps:,.0f}** |")
    w(f"| Gas Throughput | ~{ggas_s:.1f} Ggas/s |")
    w(f"| Gas Utilization | ~{gas_util:.0f}% |")
    w(f"| Persistence (avg) | ~{a_persist:.0f}ms |")

    w("")

    # === Table 3: Percentile Distribution ===
    w("## Table 3: Percentile Distribution")
    w("")
    w("| Phase | P50 | P95 | P99 |")
    w("|-------|----:|----:|----:|")

    def pct_row(name, values):
        if not values:
            w(f"| {name} | - | - | - |")
            return
        w(f"| {name} | {pct(values, 50):.0f}ms | {pct(values, 95):.0f}ms | {pct(values, 99):.0f}ms |")

    pct_row("Block Interval", ivs)
    pct_row("idle", idles)
    pct_row("payload_build", builds)
    pct_row("txpool_next", txpools)
    pct_row("tx_execute", executes)
    pct_row("state_root", state_roots)
    pct_row("assemble", assembles)
    pct_row("new_payload", new_payloads)
    pct_row("persistence", persists)

    w("")

    tps_list = [txs / (iv / 1000) for iv, txs in zip(ivs, txs_list) if iv > 0]
    w("| TPS | P50 | P75 | P90 | P95 | P99 |")
    w("|-----|----:|----:|----:|----:|----:|")
    w(f"| | {pct(tps_list, 50):,.0f} | {pct(tps_list, 75):,.0f} | {pct(tps_list, 90):,.0f} | {pct(tps_list, 95):,.0f} | {pct(tps_list, 99):,.0f} |")

    w("")
    return "\n".join(lines)


def generate_html(data, git):
    high = data['high']
    hm = data['high_miners']
    persists = data['persists']
    gas_limit = data['gas_limit']

    ivs = [iv for iv, _ in high]
    txs_list = [f['txs_executed'] for _, f in high]
    builds = [f['payload_build_total_ms'] for _, f in high]
    txpools = [f['txpool_next_ms'] for _, f in high]
    executes = [f['tx_execute_ms'] for _, f in high]
    state_roots = [f.get('state_root_ms', 0) for _, f in high]
    assembles = [f.get('assemble_ms', 0) for _, f in high]
    new_payloads = [f['new_payload_ms'] for f in hm] if hm else []
    idles = [f['idle_ms'] for f in hm] if hm else []
    fcu_others = [
        f['cycle_ms'] - f['idle_ms'] - f['fcu_ms'] - f['resolve_ms'] - f['new_payload_ms']
        for f in hm
    ] if hm else []
    fcus = [f['fcu_ms'] for f in hm] if hm else []

    a_iv = avg(ivs)
    a_txs = avg(txs_list)
    a_build = avg(builds)
    a_txpool = avg(txpools)
    a_execute = avg(executes)
    a_state_root = avg(state_roots)
    a_assemble = avg(assembles)
    a_np = avg(new_payloads)
    a_idle = avg(idles)
    a_fcu_other = avg(fcus) + avg(fcu_others)
    a_persist = avg(persists)

    iv_s = a_iv / 1000
    tps = a_txs / iv_s if iv_s > 0 else 0
    max_txs = max(txs_list)
    gas_per_tx = gas_limit / max_txs if max_txs > 0 else 51000
    gas_used = a_txs * gas_per_tx
    ggas_s = gas_used / iv_s / 1e9 if iv_s > 0 else 0
    gas_util = gas_used / gas_limit * 100

    tps_list = [txs / (iv / 1000) for iv, txs in zip(ivs, txs_list) if iv > 0]

    now = datetime.now().strftime('%Y-%m-%d %H:%M')

    # Phase data for the breakdown table
    phases = [
        {"name": "idle", "avg": a_idle, "p50": pct(idles, 50), "p95": pct(idles, 95), "p99": pct(idles, 99),
         "color": "#94a3b8", "note": "waiting for block trigger", "indent": 0},
        {"name": "payload_build", "avg": a_build, "p50": pct(builds, 50), "p95": pct(builds, 95), "p99": pct(builds, 99),
         "color": "#3b82f6", "note": "build block", "indent": 0},
        {"name": "txpool_next", "avg": a_txpool, "p50": pct(txpools, 50), "p95": pct(txpools, 95), "p99": pct(txpools, 99),
         "color": "#60a5fa", "note": "fetch txs from pool", "indent": 1},
        {"name": "tx_execute", "avg": a_execute, "p50": pct(executes, 50), "p95": pct(executes, 95), "p99": pct(executes, 99),
         "color": "#2563eb", "note": "execute txs", "indent": 1},
        {"name": "state_root", "avg": a_state_root, "p50": pct(state_roots, 50), "p95": pct(state_roots, 95), "p99": pct(state_roots, 99),
         "color": "#7c3aed", "note": "compute state root", "indent": 1},
        {"name": "assemble", "avg": a_assemble, "p50": pct(assembles, 50), "p95": pct(assembles, 95), "p99": pct(assembles, 99),
         "color": "#1d4ed8", "note": "assemble block", "indent": 1},
        {"name": "new_payload", "avg": a_np, "p50": pct(new_payloads, 50), "p95": pct(new_payloads, 95), "p99": pct(new_payloads, 99),
         "color": "#f59e0b", "note": "validate block", "indent": 0},
        {"name": "fcu + commit", "avg": a_fcu_other, "p50": 0, "p95": 0, "p99": 0,
         "color": "#a3a3a3", "note": "other overhead", "indent": 0},
    ]

    # Build stacked bar segments (top-level only)
    bar_segments = ""
    for ph in phases:
        if ph["indent"] == 0:
            w_pct = ph["avg"] / a_iv * 100 if a_iv > 0 else 0
            bar_segments += f'<div style="width:{w_pct:.1f}%;background:{ph["color"]}" title="{ph["name"]}: ~{int(ph["avg"])}ms ({w_pct:.1f}%)"></div>\n'

    # Build phase rows
    phase_rows = ""
    for ph in phases:
        p = ph["avg"] / a_iv * 100 if a_iv > 0 else 0
        bar_w = p
        indent_style = f'padding-left:{20 + ph["indent"] * 24}px;' if ph["indent"] > 0 else 'padding-left:20px;font-weight:600;'
        arrow = "→ " if ph["indent"] > 0 else ""
        phase_rows += f"""<tr>
  <td style="{indent_style}">
    <span style="display:inline-block;width:10px;height:10px;border-radius:2px;background:{ph['color']};margin-right:8px;"></span>{arrow}{ph['name']}
  </td>
  <td class="num">~{int(round(ph['avg']))}ms</td>
  <td class="num">{p:.1f}%</td>
  <td class="num dim">{int(round(ph['p50']))}ms</td>
  <td class="num dim">{int(round(ph['p95']))}ms</td>
  <td class="num dim">{int(round(ph['p99']))}ms</td>
  <td class="bar-cell"><div class="bar" style="width:{bar_w:.1f}%;background:{ph['color']};"></div></td>
  <td class="note">{ph['note']}</td>
</tr>\n"""

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Bench — {git['branch']} @ {git['commit']}</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background: #f8fafc; color: #1e293b; padding: 32px; max-width: 1100px; margin: 0 auto; }}
  h1 {{ font-size: 22px; font-weight: 700; margin-bottom: 8px; }}
  .subtitle {{ color: #64748b; font-size: 14px; margin-bottom: 28px; }}
  .meta {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 12px; margin-bottom: 32px; }}
  .meta-card {{ background: #fff; border: 1px solid #e2e8f0; border-radius: 8px; padding: 14px 18px; }}
  .meta-card .label {{ font-size: 12px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: 4px; }}
  .meta-card .value {{ font-size: 16px; font-weight: 600; }}
  .hero {{ background: linear-gradient(135deg, #1e40af, #3b82f6); color: #fff; border-radius: 12px; padding: 28px 32px; margin-bottom: 32px; display: flex; align-items: center; gap: 40px; flex-wrap: wrap; }}
  .hero-item .label {{ font-size: 13px; opacity: 0.8; margin-bottom: 2px; }}
  .hero-item .value {{ font-size: 36px; font-weight: 800; }}
  .hero-item .value small {{ font-size: 18px; font-weight: 400; }}
  .hero-item.secondary .value {{ font-size: 24px; }}
  section {{ background: #fff; border: 1px solid #e2e8f0; border-radius: 10px; padding: 24px; margin-bottom: 24px; }}
  section h2 {{ font-size: 16px; font-weight: 700; margin-bottom: 16px; }}
  table {{ width: 100%; border-collapse: collapse; font-size: 13px; }}
  th {{ text-align: left; padding: 8px 12px; border-bottom: 2px solid #e2e8f0; color: #64748b; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; }}
  td {{ padding: 7px 12px; border-bottom: 1px solid #f1f5f9; }}
  td.num {{ text-align: right; font-variant-numeric: tabular-nums; font-family: 'SF Mono', Menlo, monospace; font-size: 13px; }}
  td.note {{ color: #94a3b8; font-size: 12px; }}
  td.dim {{ color: #b0bec5; font-size: 12px; }}
  th.dim {{ color: #b0bec5; }}
  td.bar-cell {{ width: 160px; padding-right: 16px; }}
  .bar {{ height: 14px; border-radius: 3px; min-width: 2px; }}
  .stacked-bar {{ display: flex; height: 22px; border-radius: 6px; overflow: hidden; margin-bottom: 0; }}
  .stacked-bar > div {{ height: 100%; }}
  .legend {{ display: flex; gap: 16px; flex-wrap: wrap; margin-top: 10px; font-size: 12px; color: #64748b; }}
  .legend span {{ display: inline-flex; align-items: center; gap: 4px; }}
  .legend i {{ display: inline-block; width: 10px; height: 10px; border-radius: 2px; }}
  tr:hover {{ background: #f8fafc; }}
</style>
</head>
<body>

<h1>Benchmark Report</h1>
<div class="subtitle">{now} &middot; <code>{git['branch']}</code> @ <code>{git['commit']}</code> &middot; {git['commit_msg']}</div>

<div class="meta">
  <div class="meta-card"><div class="label">Branch</div><div class="value">{git['branch']}</div></div>
  <div class="meta-card"><div class="label">Commit</div><div class="value">{git['commit']}</div></div>
  <div class="meta-card"><div class="label">Gas Limit</div><div class="value">{gas_limit / 1e9:.1f}B</div></div>
  <div class="meta-card"><div class="label">Sample Blocks</div><div class="value">{len(high)}</div></div>
  <div class="meta-card"><div class="label">Gas Utilization</div><div class="value">~{gas_util:.0f}%</div></div>
  <div class="meta-card"><div class="label">Persistence</div><div class="value">~{int(a_persist)}ms</div></div>
</div>

<div class="hero">
  <div class="hero-item"><div class="label">TPS (avg)</div><div class="value">{tps:,.0f}</div></div>
  <div class="hero-item secondary"><div class="label">Txs / Block</div><div class="value">{a_txs:,.0f}</div></div>
  <div class="hero-item secondary"><div class="label">Gas Throughput</div><div class="value">{ggas_s:.1f} <small>Ggas/s</small></div></div>
  <div class="hero-item secondary"><div class="label">Block Interval</div><div class="value">{int(a_iv)} <small>ms</small></div></div>
</div>

<section>
  <h2>Phase Breakdown</h2>
  <div class="stacked-bar">
    {bar_segments}
  </div>
  <div class="legend">
    {"".join(f'<span><i style="background:{ph["color"]}"></i>{ph["name"]}</span>' for ph in phases if ph["indent"] == 0)}
  </div>
  <br>
  <table>
    <thead>
      <tr><th>Phase</th><th style="text-align:right">Avg</th><th style="text-align:right">%</th><th class="dim" style="text-align:right">P50</th><th class="dim" style="text-align:right">P95</th><th class="dim" style="text-align:right">P99</th><th></th><th>Note</th></tr>
    </thead>
    <tbody>
      <tr style="font-weight:700;">
        <td style="padding-left:20px;">Total Block Interval</td>
        <td class="num">~{int(a_iv)}ms</td><td class="num">100%</td>
        <td class="num dim">{int(pct(ivs, 50))}ms</td><td class="num dim">{int(pct(ivs, 95))}ms</td><td class="num dim">{int(pct(ivs, 99))}ms</td>
        <td></td><td></td>
      </tr>
      {phase_rows}
    </tbody>
  </table>
</section>

<section>
  <h2>TPS Distribution</h2>
  <table>
    <thead><tr><th style="text-align:right">P50</th><th style="text-align:right">P75</th><th style="text-align:right">P90</th><th style="text-align:right">P95</th><th style="text-align:right">P99</th></tr></thead>
    <tbody><tr>
      <td class="num">{pct(tps_list, 50):,.0f}</td>
      <td class="num">{pct(tps_list, 75):,.0f}</td>
      <td class="num">{pct(tps_list, 90):,.0f}</td>
      <td class="num">{pct(tps_list, 95):,.0f}</td>
      <td class="num">{pct(tps_list, 99):,.0f}</td>
    </tr></tbody>
  </table>
</section>

</body>
</html>"""


def main():
    log_path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("./reth.log")
    git = git_info()
    data = load_log(log_path)

    branch = git['branch'].replace('/', '-')
    commit = git['commit']

    # Generate markdown
    report_md = generate_report(data, git)
    md_path = Path("docs") / f"bench-{branch}-{commit}.md"
    md_path.parent.mkdir(parents=True, exist_ok=True)
    md_path.write_text(report_md + "\n")

    # Generate HTML
    report_html = generate_html(data, git)
    html_path = Path("docs") / f"bench-{branch}-{commit}.html"
    html_path.write_text(report_html + "\n")

    print(f"Reports saved to: {md_path}, {html_path}")


if __name__ == "__main__":
    main()
