#!/usr/bin/env python3
"""Preview: v2 benchmark report design with dark dashboard + waterfall chart."""

import math
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
        if m: fields[key] = int(m.group(1))
    for key in ['txpool_next_ms', 'tx_execute_ms', 'finish_ms', 'payload_build_total_ms']:
        m = re.search(rf'{key}=([\d.]+)', line)
        if m: fields[key] = float(m.group(1))
    return ts, fields

def parse_miner_line(line):
    line = strip_ansi(line)
    if "miner advance timing" not in line:
        return None
    ts = parse_timestamp(line)
    if not ts:
        return None
    fields = {}
    m = re.search(r'block_number=(\d+)', line)
    if m: fields['block_number'] = int(m.group(1))
    for key in ['cycle_ms', 'idle_ms', 'advance_ms', 'fcu_ms', 'resolve_ms', 'new_payload_ms']:
        m = re.search(rf'{key}=([\d.]+)', line)
        if m: fields[key] = float(m.group(1))
    return ts, fields

def parse_persistence_line(line):
    line = strip_ansi(line)
    if "Saved range of blocks" not in line:
        return None
    m = re.search(r'elapsed=([\d.]+)(µs|ms|s)', line)
    if not m: return None
    val = float(m.group(1))
    unit = m.group(2)
    if unit == 'µs': val /= 1000
    elif unit == 's': val *= 1000
    return val

def parse_finish_detail_line(line):
    line = strip_ansi(line)
    if "block builder finish timing" not in line:
        return None
    fields = {}
    for key in ['state_root_ms', 'assemble_ms']:
        m = re.search(rf'{key}=([\d.]+)', line)
        if m: fields[key] = float(m.group(1))
    return fields

def parse_gas_limit(line):
    line = strip_ansi(line)
    m = re.search(r'gas_limit=([\d.]+)([KMG]?)gas', line)
    if m:
        val = float(m.group(1))
        unit = m.group(2)
        if unit == 'K': val *= 1_000
        elif unit == 'M': val *= 1_000_000
        elif unit == 'G': val *= 1_000_000_000
        return int(val)
    return None

def pct(values, p):
    if not values: return 0
    s = sorted(values)
    k = (len(s) - 1) * p / 100
    f = int(k)
    c = f + 1
    if c >= len(s): return s[f]
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
        print(f"Error: {log_path} not found"); sys.exit(1)
    payloads, miners, persists, gas_limit = [], [], [], None
    pending_finish_detail = None
    with open(log_path) as f:
        for line in f:
            fd = parse_finish_detail_line(line)
            if fd:
                pending_finish_detail = fd
                continue
            p = parse_payload_line(line)
            if p:
                if pending_finish_detail:
                    p[1].update(pending_finish_detail)
                    pending_finish_detail = None
                payloads.append(p)
                continue
            m = parse_miner_line(line)
            if m: miners.append(m)
            per = parse_persistence_line(line)
            if per is not None: persists.append(per)
            if gas_limit is None:
                gl = parse_gas_limit(line)
                if gl: gas_limit = gl
    intervals = []
    for i in range(1, len(payloads)):
        iv = (payloads[i][0] - payloads[i - 1][0]).total_seconds() * 1000
        intervals.append((iv, payloads[i][1]))
    high = [(iv, f) for iv, f in intervals if f.get('txs_executed', 0) >= 40000]
    high_miners = [f for _, f in miners if f.get('advance_ms', 0) > 200]
    return {
        'high': high, 'high_miners': high_miners, 'persists': persists,
        'gas_limit': gas_limit or 2_500_000_000, 'total_payloads': len(payloads),
    }


def svg_waterfall(phases, total_ms, width=600, height=200):
    """Generate an SVG waterfall/timeline chart showing the block lifecycle."""
    bar_h = 28
    gap = 6
    pad_left = 120
    pad_right = 60
    pad_top = 10
    chart_w = width - pad_left - pad_right
    ms_scale = chart_w / total_ms if total_ms > 0 else 1

    y = pad_top
    bars = []
    for ph in phases:
        x = pad_left + ph['offset'] * ms_scale
        w = max(ph['duration'] * ms_scale, 2)
        # label
        bars.append(f'<text x="{pad_left - 8}" y="{y + bar_h/2 + 4}" text-anchor="end" '
                     f'fill="#94a3b8" font-size="12" font-family="-apple-system, sans-serif">{ph["name"]}</text>')
        # bar
        bars.append(f'<rect x="{x}" y="{y}" width="{w}" height="{bar_h}" rx="4" fill="{ph["color"]}" opacity="0.9">'
                     f'<title>{ph["name"]}: ~{int(ph["duration"])}ms</title></rect>')
        # duration label
        if w > 30:
            bars.append(f'<text x="{x + w/2}" y="{y + bar_h/2 + 4}" text-anchor="middle" '
                         f'fill="#fff" font-size="11" font-weight="600" font-family="SF Mono, Menlo, monospace">'
                         f'{int(ph["duration"])}ms</text>')
        else:
            bars.append(f'<text x="{x + w + 4}" y="{y + bar_h/2 + 4}" text-anchor="start" '
                         f'fill="#64748b" font-size="11" font-family="SF Mono, Menlo, monospace">'
                         f'{int(ph["duration"])}ms</text>')
        y += bar_h + gap

    total_h = y + 20

    # Time axis
    axis_y = y
    ticks = ""
    for ms_val in range(0, int(total_ms) + 1, 100):
        tx = pad_left + ms_val * ms_scale
        ticks += f'<line x1="{tx}" y1="{pad_top - 5}" x2="{tx}" y2="{axis_y}" stroke="#334155" stroke-width="1" stroke-dasharray="3,3"/>'
        ticks += f'<text x="{tx}" y="{axis_y + 14}" text-anchor="middle" fill="#64748b" font-size="10" font-family="SF Mono, Menlo, monospace">{ms_val}ms</text>'

    return f'''<svg width="100%" viewBox="0 0 {width} {total_h + 20}" xmlns="http://www.w3.org/2000/svg">
  {ticks}
  {"".join(bars)}
</svg>'''


def svg_histogram(values, width=120, height=32, color="#3b82f6", bins=20):
    """Tiny inline histogram sparkline."""
    if not values:
        return ""
    mn, mx = min(values), max(values)
    if mn == mx:
        return f'<svg width="{width}" height="{height}"><rect x="50" y="4" width="20" height="{height-8}" rx="2" fill="{color}" opacity="0.5"/></svg>'
    bin_w = (mx - mn) / bins
    counts = [0] * bins
    for v in values:
        idx = min(int((v - mn) / bin_w), bins - 1)
        counts[idx] += 1
    max_c = max(counts)
    bar_w = width / bins
    bars = []
    for i, c in enumerate(counts):
        h = (c / max_c) * (height - 4) if max_c > 0 else 0
        bars.append(f'<rect x="{i * bar_w:.1f}" y="{height - h - 2:.1f}" width="{bar_w - 1:.1f}" height="{h:.1f}" rx="1" fill="{color}" opacity="0.6"/>')
    return f'<svg width="{width}" height="{height}" viewBox="0 0 {width} {height}">{"".join(bars)}</svg>'


def generate_html_v2(data, git):
    high = data['high']
    hm = data['high_miners']
    persists = data['persists']
    gas_limit = data['gas_limit']

    ivs = [iv for iv, _ in high]
    txs_list = [f['txs_executed'] for _, f in high]
    builds = [f['payload_build_total_ms'] for _, f in high]
    txpools = [f['txpool_next_ms'] for _, f in high]
    executes = [f['tx_execute_ms'] for _, f in high]
    finishes = [f['finish_ms'] for _, f in high]
    state_roots = [f.get('state_root_ms', 0) for _, f in high]
    assembles = [f.get('assemble_ms', 0) for _, f in high]
    new_payloads = [f['new_payload_ms'] for f in hm] if hm else []
    idles = [f['idle_ms'] for f in hm] if hm else []
    fcus = [f['fcu_ms'] for f in hm] if hm else []
    fcu_others = [
        f['cycle_ms'] - f['idle_ms'] - f['fcu_ms'] - f['resolve_ms'] - f['new_payload_ms']
        for f in hm
    ] if hm else []

    a_iv = avg(ivs)
    a_txs = avg(txs_list)
    a_idle = avg(idles)
    a_build = avg(builds)
    a_txpool = avg(txpools)
    a_execute = avg(executes)
    a_finish = avg(finishes)
    a_state_root = avg(state_roots)
    a_assemble = avg(assembles)
    a_np = avg(new_payloads)
    a_fcu = avg(fcus)
    a_fcu_other = avg(fcu_others)
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

    # Waterfall data
    offset = 0
    wf_phases = []
    for name, dur, color in [
        ("idle", a_idle, "#475569"),
        ("txpool_next", a_txpool, "#60a5fa"),
        ("tx_execute", a_execute, "#2563eb"),
        ("state_root", a_state_root, "#7c3aed"),
        ("assemble", a_assemble, "#1d4ed8"),
        ("new_payload", a_np, "#f59e0b"),
        ("fcu+commit", a_fcu + a_fcu_other, "#64748b"),
    ]:
        wf_phases.append({"name": name, "offset": offset, "duration": dur, "color": color})
        offset += dur
    waterfall = svg_waterfall(wf_phases, a_iv)

    # Gas gauge (SVG arc)
    gauge_pct = min(gas_util, 100)
    gauge_angle = gauge_pct / 100 * 270  # 270 degree arc
    gauge_r = 52
    gauge_cx, gauge_cy = 64, 64
    start_angle = 135  # start from bottom-left
    end_angle = start_angle + gauge_angle
    def arc_point(angle, r):
        rad = math.radians(angle)
        return gauge_cx + r * math.cos(rad), gauge_cy + r * math.sin(rad)
    sx, sy = arc_point(start_angle, gauge_r)
    ex, ey = arc_point(end_angle, gauge_r)
    large = 1 if gauge_angle > 180 else 0
    # background arc
    bex, bey = arc_point(start_angle + 270, gauge_r)
    gauge_svg = f'''<svg width="128" height="128" viewBox="0 0 128 128">
      <path d="M {sx:.1f} {sy:.1f} A {gauge_r} {gauge_r} 0 1 1 {bex:.1f} {bey:.1f}" fill="none" stroke="#1e293b" stroke-width="10" stroke-linecap="round"/>
      <path d="M {sx:.1f} {sy:.1f} A {gauge_r} {gauge_r} 0 {large} 1 {ex:.1f} {ey:.1f}" fill="none" stroke="#3b82f6" stroke-width="10" stroke-linecap="round"/>
      <text x="64" y="62" text-anchor="middle" fill="#f1f5f9" font-size="22" font-weight="800" font-family="SF Mono, Menlo, monospace">{gas_util:.0f}%</text>
      <text x="64" y="78" text-anchor="middle" fill="#64748b" font-size="10" font-family="-apple-system, sans-serif">gas used</text>
    </svg>'''

    # Phase table with histograms
    phase_table_data = [
        ("idle", a_idle, idles, "#475569", 0),
        ("payload_build", a_build, builds, "#3b82f6", 0),
        ("txpool_next", a_txpool, txpools, "#60a5fa", 1),
        ("tx_execute", a_execute, executes, "#2563eb", 1),
        ("state_root", a_state_root, state_roots, "#7c3aed", 1),
        ("assemble", a_assemble, assembles, "#1d4ed8", 1),
        ("new_payload", a_np, new_payloads, "#f59e0b", 0),
        ("fcu+commit", a_fcu + a_fcu_other, [], "#64748b", 0),
    ]

    phase_rows = ""
    for name, a_val, values, color, indent in phase_table_data:
        p = a_val / a_iv * 100 if a_iv > 0 else 0
        p50 = f"{int(pct(values, 50))}ms" if values else "-"
        p95 = f"{int(pct(values, 95))}ms" if values else "-"
        p99 = f"{int(pct(values, 99))}ms" if values else "-"
        hist = svg_histogram(values, color=color) if values else ""
        indent_px = 16 + indent * 20
        arrow = '<span style="color:#475569;">→</span> ' if indent > 0 else ""
        weight = "400" if indent > 0 else "600"
        phase_rows += f'''<tr>
  <td style="padding-left:{indent_px}px;font-weight:{weight};">
    <span class="dot" style="background:{color};"></span>{arrow}{name}
  </td>
  <td class="mono r">~{int(round(a_val))}ms</td>
  <td class="mono r">{p:.1f}%</td>
  <td class="mono r dim">{p50}</td>
  <td class="mono r dim">{p95}</td>
  <td class="mono r dim">{p99}</td>
  <td>{hist}</td>
</tr>\n'''

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Bench — {git['branch']} @ {git['commit']}</title>
<style>
  :root {{
    --bg: #0f172a; --card: #1e293b; --border: #334155;
    --text: #f1f5f9; --text2: #94a3b8; --text3: #64748b;
    --accent: #3b82f6; --accent2: #60a5fa;
  }}
  * {{ margin:0; padding:0; box-sizing:border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Inter, sans-serif;
    background: var(--bg); color: var(--text);
    padding: 32px; max-width: 1080px; margin: 0 auto;
    -webkit-font-smoothing: antialiased;
  }}

  /* Header */
  .header {{ margin-bottom: 32px; }}
  .header h1 {{ font-size: 28px; font-weight: 800; letter-spacing: -0.5px; }}
  .header .sub {{
    color: var(--text3); font-size: 13px; margin-top: 6px;
    font-family: 'SF Mono', Menlo, monospace;
  }}
  .header .sub code {{
    background: var(--card); padding: 2px 7px; border-radius: 4px;
    border: 1px solid var(--border); color: var(--text2);
  }}

  /* Hero metrics */
  .hero {{
    display: grid; grid-template-columns: 1fr 1fr 1fr 128px;
    gap: 20px; margin-bottom: 28px; align-items: center;
  }}
  .metric-card {{
    background: var(--card); border: 1px solid var(--border);
    border-radius: 12px; padding: 20px 24px;
  }}
  .metric-card .label {{ font-size: 12px; color: var(--text3); text-transform: uppercase; letter-spacing: 0.06em; }}
  .metric-card .val {{
    font-size: 36px; font-weight: 800; margin-top: 4px;
    font-family: 'SF Mono', Menlo, monospace;
    background: linear-gradient(135deg, #60a5fa, #a78bfa);
    -webkit-background-clip: text; -webkit-text-fill-color: transparent;
  }}
  .metric-card .val small {{ font-size: 16px; font-weight: 500; }}
  .metric-card.secondary .val {{
    font-size: 24px;
    background: none; -webkit-text-fill-color: var(--text);
  }}
  .gauge-card {{
    display: flex; align-items: center; justify-content: center;
  }}

  /* Sections */
  .section {{
    background: var(--card); border: 1px solid var(--border);
    border-radius: 12px; padding: 24px; margin-bottom: 20px;
  }}
  .section h2 {{
    font-size: 14px; font-weight: 700; color: var(--text2);
    text-transform: uppercase; letter-spacing: 0.06em; margin-bottom: 16px;
  }}

  /* Table */
  table {{ width: 100%; border-collapse: collapse; font-size: 13px; }}
  th {{
    text-align: left; padding: 8px 10px; border-bottom: 1px solid var(--border);
    color: var(--text3); font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em;
  }}
  th.r {{ text-align: right; }}
  td {{ padding: 7px 10px; border-bottom: 1px solid rgba(51,65,85,0.5); }}
  td.mono {{ font-family: 'SF Mono', Menlo, monospace; }}
  td.r {{ text-align: right; }}
  td.dim {{ color: var(--text3); font-size: 12px; }}
  .dot {{
    display: inline-block; width: 8px; height: 8px; border-radius: 2px;
    margin-right: 8px; vertical-align: middle;
  }}
  tr:hover {{ background: rgba(59,130,246,0.04); }}

  /* Waterfall */
  .waterfall-wrap {{ overflow-x: auto; }}

  /* TPS dist */
  .tps-grid {{
    display: grid; grid-template-columns: repeat(5, 1fr); gap: 12px;
  }}
  .tps-cell {{
    text-align: center; padding: 12px;
    background: rgba(59,130,246,0.06); border-radius: 8px;
  }}
  .tps-cell .label {{ font-size: 11px; color: var(--text3); text-transform: uppercase; }}
  .tps-cell .val {{ font-size: 20px; font-weight: 700; font-family: 'SF Mono', Menlo, monospace; margin-top: 2px; }}

  /* Meta */
  .meta-row {{
    display: flex; gap: 16px; flex-wrap: wrap; font-size: 12px; color: var(--text3);
    margin-bottom: 28px; padding: 12px 16px; background: var(--card);
    border: 1px solid var(--border); border-radius: 8px;
  }}
  .meta-row span {{ display: flex; align-items: center; gap: 4px; }}
  .meta-row .v {{ color: var(--text2); font-family: 'SF Mono', Menlo, monospace; }}
</style>
</head>
<body>

<div class="header">
  <h1>Benchmark Report</h1>
  <div class="sub">
    {now} &middot; <code>{git['branch']}</code> &middot; <code>{git['commit']}</code> &middot; {git['commit_msg']}
  </div>
</div>

<div class="meta-row">
  <span>Gas Limit: <b class="v">{gas_limit / 1e9:.1f}B</b></span>
  <span>Sample Blocks: <b class="v">{len(high)}</b></span>
  <span>Block Interval: <b class="v">~{int(a_iv)}ms</b></span>
  <span>Persistence: <b class="v">~{int(a_persist)}ms</b></span>
</div>

<div class="hero">
  <div class="metric-card">
    <div class="label">TPS</div>
    <div class="val">{tps:,.0f}</div>
  </div>
  <div class="metric-card secondary">
    <div class="label">Txs / Block</div>
    <div class="val">{a_txs:,.0f}</div>
  </div>
  <div class="metric-card secondary">
    <div class="label">Gas Throughput</div>
    <div class="val">{ggas_s:.1f} <small>Ggas/s</small></div>
  </div>
  <div class="gauge-card">
    {gauge_svg}
  </div>
</div>

<div class="section">
  <h2>Block Lifecycle (Waterfall)</h2>
  <div class="waterfall-wrap">
    {waterfall}
  </div>
</div>

<div class="section">
  <h2>Phase Breakdown</h2>
  <table>
    <thead>
      <tr>
        <th>Phase</th><th class="r">Avg</th><th class="r">%</th>
        <th class="r" style="color:var(--text3);opacity:0.6;">P50</th>
        <th class="r" style="color:var(--text3);opacity:0.6;">P95</th>
        <th class="r" style="color:var(--text3);opacity:0.6;">P99</th>
        <th>Distribution</th>
      </tr>
    </thead>
    <tbody>
      <tr style="font-weight:700;">
        <td><span class="dot" style="background:var(--accent);"></span>Total Block Interval</td>
        <td class="mono r">~{int(a_iv)}ms</td><td class="mono r">100%</td>
        <td class="mono r dim">{int(pct(ivs, 50))}ms</td>
        <td class="mono r dim">{int(pct(ivs, 95))}ms</td>
        <td class="mono r dim">{int(pct(ivs, 99))}ms</td>
        <td>{svg_histogram(ivs, color="#3b82f6")}</td>
      </tr>
      {phase_rows}
    </tbody>
  </table>
</div>

<div class="section">
  <h2>TPS Distribution</h2>
  <div class="tps-grid">
    <div class="tps-cell"><div class="label">P50</div><div class="val">{pct(tps_list, 50):,.0f}</div></div>
    <div class="tps-cell"><div class="label">P75</div><div class="val">{pct(tps_list, 75):,.0f}</div></div>
    <div class="tps-cell"><div class="label">P90</div><div class="val">{pct(tps_list, 90):,.0f}</div></div>
    <div class="tps-cell"><div class="label">P95</div><div class="val">{pct(tps_list, 95):,.0f}</div></div>
    <div class="tps-cell"><div class="label">P99</div><div class="val">{pct(tps_list, 99):,.0f}</div></div>
  </div>
</div>

</body>
</html>"""


def main():
    log_path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("./reth.log")
    git = git_info()
    data = load_log(log_path)
    html = generate_html_v2(data, git)
    branch = git['branch'].replace('/', '-')
    commit = git['commit']
    out = Path("docs") / f"perf-dashboard-{branch}-{commit}.html"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(html + "\n")
    print(f"Dashboard saved to {out}")


if __name__ == "__main__":
    main()
