#!/usr/bin/env python3
"""
leakwatch — a sidecar memory-leak monitor for a Django app that spawns subprocesses.

It does NOT run inside your app. You point it at your Django master PID and it watches
the whole process tree that Django creates, samples CPU RSS and per-process GPU VRAM on
a timer, stores the history in SQLite, fits a slope over a long window to flag slow leaks,
and writes a self-refreshing HTML dashboard plus an append-only log.

Why a sidecar: if the monitor itself has a bug or a leak, it can't take your app down,
and the exact same script runs unchanged in production.

Usage
-----
    # watch a specific Django master process and everything it spawns:
    python3 leakwatch.py --pid 12345

    # or let it find the Django master by a string in the command line:
    python3 leakwatch.py --match "manage.py runserver"
    python3 leakwatch.py --match "gunicorn"

    # tune it:
    python3 leakwatch.py --pid 12345 --interval 30 --window-min 60 --outdir ./leakwatch_out

Outputs (in --outdir, default ./leakwatch_out)
----------------------------------------------
    dashboard.html   self-refreshing visual dashboard (open in a browser)
    leakwatch.db     SQLite history, survives restarts (a week of slow-leak data is fine)
    leakwatch.log    append-only text log + leak alerts

GPU detection is automatic via NVML: 0, 1, or N GPUs, no configuration. If no NVIDIA GPU
or driver is present, GPU columns are simply blank and CPU monitoring continues.
"""

import argparse
import json
import os
import sqlite3
import sys
import time
from collections import defaultdict, deque
from datetime import datetime, timezone

import psutil

# ---- GPU layer (optional, auto-detected) -----------------------------------

class GpuProbe:
    """Wraps NVML. Degrades gracefully to a no-op if no GPU/driver is present."""

    def __init__(self):
        self.ok = False
        self.count = 0
        self._handles = []
        self._names = []
        try:
            import pynvml
            self._nvml = pynvml
            pynvml.nvmlInit()
            self.count = pynvml.nvmlDeviceGetCount()
            for i in range(self.count):
                h = pynvml.nvmlDeviceGetHandleByIndex(i)
                self._handles.append(h)
                name = pynvml.nvmlDeviceGetName(h)
                self._names.append(name.decode() if isinstance(name, bytes) else name)
            self.ok = self.count > 0
        except Exception as e:
            self._nvml = None
            self._init_error = str(e)

    def vram_by_pid(self):
        """Return {pid: vram_bytes} summed across all GPUs, and per-device totals."""
        usage = defaultdict(int)
        device_totals = []
        if not self.ok:
            return usage, device_totals
        nvml = self._nvml
        for idx, h in enumerate(self._handles):
            used = total = 0
            try:
                mem = nvml.nvmlDeviceGetMemoryInfo(h)
                used, total = mem.used, mem.total
            except Exception:
                pass
            device_totals.append({
                "index": idx, "name": self._names[idx],
                "used": used, "total": total,
            })
            for fn in ("nvmlDeviceGetComputeRunningProcesses_v3",
                       "nvmlDeviceGetComputeRunningProcesses_v2",
                       "nvmlDeviceGetComputeRunningProcesses"):
                try:
                    procs = getattr(nvml, fn)(h)
                    for p in procs:
                        mem_used = getattr(p, "usedGpuMemory", 0) or 0
                        # NVML reports a sentinel when it can't measure per-proc memory
                        if mem_used and mem_used < (1 << 62):
                            usage[p.pid] += mem_used
                    break
                except Exception:
                    continue
        return usage, device_totals

    def shutdown(self):
        if self._nvml:
            try:
                self._nvml.nvmlShutdown()
            except Exception:
                pass


# ---- identity ---------------------------------------------------------------

def label_for(proc):
    """A stable, human label for a process: the script name it's running.

    This is what makes the monitor self-describing — whatever Django spawns names
    itself via its command line, so 'which module' is answered automatically.
    """
    try:
        cmd = proc.cmdline()
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return proc.name() if proc.is_running() else "gone"
    if not cmd:
        try:
            return proc.name()
        except Exception:
            return "unknown"
    exe = os.path.basename(cmd[0]) if cmd else "proc"
    # find the first real script argument (skip interpreter flags like -u, -m).
    # only a .py file or a path-like token counts as the script — this avoids
    # mistaking shell redirect targets (e.g. "lw.out") for the program.
    script = None
    for tok in cmd[1:]:
        if tok.startswith("-"):
            continue
        if tok.endswith(".py"):
            script = os.path.basename(tok)
            break
        if "/" in tok and not tok.startswith(("/dev", "/tmp")):
            base = os.path.basename(tok)
            if base:
                script = base
                break
    if script:
        return f"{exe}:{script}"
    return exe


# ---- storage ----------------------------------------------------------------

SCHEMA = """
CREATE TABLE IF NOT EXISTS samples (
    ts        REAL    NOT NULL,
    pid       INTEGER NOT NULL,
    label     TEXT    NOT NULL,
    rss       INTEGER NOT NULL,
    vram      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_samples_pid ON samples(pid);
CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(ts);
CREATE TABLE IF NOT EXISTS gpus (
    ts     REAL NOT NULL,
    idx    INTEGER NOT NULL,
    name   TEXT,
    used   INTEGER,
    total  INTEGER
);
"""


class Store:
    def __init__(self, path):
        self.db = sqlite3.connect(path)
        self.db.executescript(SCHEMA)
        self.db.commit()

    def write_samples(self, ts, rows):
        self.db.executemany(
            "INSERT INTO samples(ts,pid,label,rss,vram) VALUES(?,?,?,?,?)",
            [(ts, r["pid"], r["label"], r["rss"], r["vram"]) for r in rows],
        )
        self.db.commit()

    def write_gpus(self, ts, devices):
        if devices:
            self.db.executemany(
                "INSERT INTO gpus(ts,idx,name,used,total) VALUES(?,?,?,?,?)",
                [(ts, d["index"], d["name"], d["used"], d["total"]) for d in devices],
            )
            self.db.commit()

    def series_by_label(self, since_ts):
        """Aggregate history grouped by label so it survives PID churn."""
        cur = self.db.execute(
            "SELECT label, ts, SUM(rss), SUM(vram) FROM samples "
            "WHERE ts>=? GROUP BY label, ts ORDER BY ts",
            (since_ts,),
        )
        out = defaultdict(lambda: {"ts": [], "rss": [], "vram": []})
        for label, ts, rss, vram in cur:
            out[label]["ts"].append(ts)
            out[label]["rss"].append(rss or 0)
            out[label]["vram"].append(vram or 0)
        return out

    def prune(self, before_ts):
        self.db.execute("DELETE FROM samples WHERE ts<?", (before_ts,))
        self.db.execute("DELETE FROM gpus WHERE ts<?", (before_ts,))
        self.db.commit()


# ---- leak math --------------------------------------------------------------

def slope_per_hour(ts, ys):
    """Least-squares slope of ys over ts, returned in units/hour. None if too few points."""
    n = len(ts)
    if n < 6:
        return None
    t0 = ts[0]
    xs = [(t - t0) for t in ts]  # seconds
    mx = sum(xs) / n
    my = sum(ys) / n
    num = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    den = sum((x - mx) ** 2 for x in xs)
    if den == 0:
        return None
    per_sec = num / den
    return per_sec * 3600.0


def floor_rise_per_hour(ts, ys, buckets=8):
    """Rise of the running-minimum (the floor) per hour.

    A true leak raises the floor — memory it never gives back — which is a cleaner
    signal than peak growth for sawtooth allocators like ONNX Runtime's arena.
    """
    n = len(ts)
    if n < buckets * 2:
        return None
    span = ts[-1] - ts[0]
    if span <= 0:
        return None
    size = span / buckets
    mins_t, mins_y = [], []
    for b in range(buckets):
        lo, hi = ts[0] + b * size, ts[0] + (b + 1) * size
        vals = [(t, y) for t, y in zip(ts, ys) if lo <= t < hi]
        if vals:
            ty = min(vals, key=lambda p: p[1])
            mins_t.append(ty[0])
            mins_y.append(ty[1])
    return slope_per_hour(mins_t, mins_y)


def human(n):
    n = float(n)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if abs(n) < 1024:
            return f"{n:.1f} {unit}"
        n /= 1024
    return f"{n:.1f} PB"


def assess(series, window_sec, min_slope_mb_hr):
    """Return per-label verdicts: slope, floor-rise, and a leak flag."""
    cutoff = time.time() - window_sec
    verdicts = []
    for label, s in series.items():
        ts = [t for t in s["ts"] if t >= cutoff]
        if len(ts) < 6:
            continue
        keep = len(ts)
        rss = s["rss"][-keep:]
        vram = s["vram"][-keep:]
        rss_slope = slope_per_hour(ts, rss)
        rss_floor = floor_rise_per_hour(ts, rss)
        vram_slope = slope_per_hour(ts, vram)
        vram_floor = floor_rise_per_hour(ts, vram)
        thr = min_slope_mb_hr * 1024 * 1024
        # leak if the floor keeps rising AND the overall trend agrees, on either resource
        rss_leak = (rss_floor or 0) > thr and (rss_slope or 0) > thr * 0.5
        vram_leak = (vram_floor or 0) > thr and (vram_slope or 0) > thr * 0.5
        verdicts.append({
            "label": label,
            "samples": keep,
            "rss_last": rss[-1],
            "vram_last": vram[-1],
            "rss_slope": rss_slope or 0,
            "rss_floor": rss_floor or 0,
            "vram_slope": vram_slope or 0,
            "vram_floor": vram_floor or 0,
            "leak": rss_leak or vram_leak,
            "leak_kind": ("VRAM" if vram_leak else "") + ("+" if rss_leak and vram_leak else "") + ("RSS" if rss_leak else ""),
        })
    verdicts.sort(key=lambda v: max(v["rss_floor"], v["vram_floor"]), reverse=True)
    return verdicts


# ---- dashboard --------------------------------------------------------------

def render_html(verdicts, series, gpu_devices, meta, window_sec):
    cutoff = time.time() - window_sec
    chart_data = {}
    for label, s in series.items():
        pts_t = [t for t in s["ts"] if t >= cutoff]
        keep = len(pts_t)
        if keep < 2:
            continue
        chart_data[label] = {
            "t": pts_t,
            "rss": [v / (1024*1024) for v in s["rss"][-keep:]],   # MB
            "vram": [v / (1024*1024) for v in s["vram"][-keep:]], # MB
        }
    payload = json.dumps({
        "verdicts": verdicts,
        "charts": chart_data,
        "gpus": gpu_devices,
        "meta": meta,
    })

    # design: a dark instrument panel. one accent (amber) reserved for leak alarms.
    # signature element = the per-process "drift" sparkline row that turns amber when
    # the floor is rising. everything else stays quiet graphite so the alarm reads.
    return """<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="refresh" content="%(refresh)d">
<title>leakwatch</title>
<script src="https://cdnjs.cloudflare.com/ajax/libs/Chart.js/4.4.1/chart.umd.min.js"></script>
<style>
:root{
  --bg:#0d1117; --panel:#151b23; --panel2:#1b232d; --line:#26313d;
  --ink:#e6edf3; --mut:#8b98a5; --dim:#5c6773;
  --ok:#3fb950; --alarm:#f0a020; --alarm-soft:#3a2c10;
  --rss:#58a6ff; --vram:#bc8cff;
}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);
  font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:14px;line-height:1.5}
.wrap{max-width:1100px;margin:0 auto;padding:28px 20px 80px}
header{display:flex;justify-content:space-between;align-items:baseline;
  border-bottom:1px solid var(--line);padding-bottom:14px;margin-bottom:22px;flex-wrap:wrap;gap:10px}
h1{font-size:15px;letter-spacing:.22em;text-transform:uppercase;font-weight:600;margin:0}
h1 .dot{color:var(--ok)}
.meta{color:var(--mut);font-size:12px}
.meta b{color:var(--ink);font-weight:600}
.grid-gpu{display:flex;gap:12px;flex-wrap:wrap;margin-bottom:22px}
.gpu{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:12px 14px;min-width:200px;flex:1}
.gpu .name{color:var(--mut);font-size:11px;letter-spacing:.08em;text-transform:uppercase}
.gpu .bar{height:6px;background:var(--panel2);border-radius:3px;margin-top:8px;overflow:hidden}
.gpu .bar i{display:block;height:100%%;background:var(--vram)}
.gpu .val{margin-top:6px;font-size:12px;color:var(--ink)}
.section-label{color:var(--dim);font-size:11px;letter-spacing:.18em;text-transform:uppercase;margin:0 0 10px}
table{width:100%%;border-collapse:collapse;background:var(--panel);
  border:1px solid var(--line);border-radius:8px;overflow:hidden;margin-bottom:26px}
th,td{text-align:left;padding:10px 12px;border-bottom:1px solid var(--line);white-space:nowrap}
th{color:var(--dim);font-size:10px;letter-spacing:.12em;text-transform:uppercase;font-weight:600}
td.num{text-align:right;font-variant-numeric:tabular-nums}
tr:last-child td{border-bottom:none}
tr.leak{background:var(--alarm-soft)}
tr.leak td:first-child{box-shadow:inset 3px 0 0 var(--alarm)}
.tag{display:inline-block;font-size:10px;padding:2px 7px;border-radius:999px;letter-spacing:.06em}
.tag.ok{color:var(--ok);border:1px solid #1d3424}
.tag.alarm{color:var(--alarm);border:1px solid #4a3a12;background:#20180a}
.rise{color:var(--alarm)} .flat{color:var(--dim)}
.charts{display:grid;grid-template-columns:1fr;gap:18px}
@media(min-width:760px){.charts{grid-template-columns:1fr 1fr}}
.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px}
.card h3{margin:0 0 4px;font-size:12px;color:var(--ink);letter-spacing:.04em}
.card .sub{color:var(--dim);font-size:11px;margin-bottom:8px}
.empty{color:var(--dim);padding:30px;text-align:center;border:1px dashed var(--line);border-radius:8px}
footer{color:var(--dim);font-size:11px;margin-top:30px;text-align:center}
</style></head>
<body><div class="wrap">
<header>
  <h1><span class="dot">&#9679;</span> leakwatch</h1>
  <div class="meta">watching <b id="root"></b> &middot; tree <b id="treecount"></b> procs
   &middot; window <b id="win"></b> &middot; updated <b id="upd"></b></div>
</header>
<div id="gpus" class="grid-gpu"></div>
<p class="section-label">Processes &mdash; grouped by what they run</p>
<div id="tablewrap"></div>
<p class="section-label">Drift over the window</p>
<div id="charts" class="charts"></div>
<footer>auto-refresh every %(refresh)ds &middot; floor-rise is memory never given back &middot; sidecar pid %(selfpid)d</footer>
</div>
<script>
const DATA = %(payload)s;
const fmtMB = v => v>=1024 ? (v/1024).toFixed(2)+" GB" : v.toFixed(0)+" MB";
const slopeCell = mbhr => {
  const a = Math.abs(mbhr);
  if (a < 0.5) return '<span class="flat">~flat</span>';
  const cls = mbhr>0 ? 'rise' : 'flat';
  const sign = mbhr>0 ? '+' : '';
  return '<span class="'+cls+'">'+sign+fmtMB(mbhr)+'/hr</span>';
};
document.getElementById('root').textContent = DATA.meta.root_label + ' (pid '+DATA.meta.root_pid+')';
document.getElementById('treecount').textContent = DATA.meta.tree_count;
document.getElementById('win').textContent = DATA.meta.window_label;
document.getElementById('upd').textContent = DATA.meta.updated;

// GPUs
const gpuWrap = document.getElementById('gpus');
if (!DATA.gpus.length){ gpuWrap.innerHTML = '<div class="gpu"><div class="name">GPU</div><div class="val">none detected &mdash; CPU only</div></div>'; }
DATA.gpus.forEach(g=>{
  const pct = g.total ? (100*g.used/g.total) : 0;
  const d=document.createElement('div'); d.className='gpu';
  d.innerHTML='<div class="name">GPU '+g.index+' &middot; '+g.name+'</div>'+
    '<div class="bar"><i style="width:'+pct.toFixed(1)+'%%"></i></div>'+
    '<div class="val">'+fmtMB(g.used/1048576)+' / '+fmtMB(g.total/1048576)+' &middot; '+pct.toFixed(0)+'%%</div>';
  gpuWrap.appendChild(d);
});

// table
const v = DATA.verdicts;
const tw = document.getElementById('tablewrap');
if(!v.length){ tw.innerHTML='<div class="empty">No tracked processes yet. Samples are still accumulating.</div>'; }
else{
  let h='<table><thead><tr><th>Process</th><th>Status</th>'+
    '<th class="num">RSS now</th><th class="num">RSS floor</th>'+
    '<th class="num">VRAM now</th><th class="num">VRAM floor</th><th class="num">n</th></tr></thead><tbody>';
  v.forEach(r=>{
    h+='<tr class="'+(r.leak?'leak':'')+'">'+
      '<td>'+r.label+'</td>'+
      '<td>'+(r.leak?'<span class="tag alarm">LEAK '+r.leak_kind+'</span>':'<span class="tag ok">steady</span>')+'</td>'+
      '<td class="num">'+fmtMB(r.rss_last/1048576)+'</td>'+
      '<td class="num">'+slopeCell(r.rss_floor/1048576)+'</td>'+
      '<td class="num">'+(r.vram_last? fmtMB(r.vram_last/1048576):'&mdash;')+'</td>'+
      '<td class="num">'+(r.vram_last? slopeCell(r.vram_floor/1048576):'&mdash;')+'</td>'+
      '<td class="num">'+r.samples+'</td></tr>';
  });
  tw.innerHTML=h+'</tbody></table>';
}

// charts
const cw = document.getElementById('charts');
const labels = Object.keys(DATA.charts);
if(!labels.length){ cw.innerHTML='<div class="empty">Charts appear once there are at least two samples.</div>'; }
labels.forEach((lab,i)=>{
  const c = DATA.charts[lab];
  const card=document.createElement('div'); card.className='card';
  card.innerHTML='<h3>'+lab+'</h3><div class="sub">RSS &amp; VRAM, MB</div><canvas id="c'+i+'" height="150"></canvas>';
  cw.appendChild(card);
  const t0=c.t[0];
  const xs=c.t.map(t=>((t-t0)/60).toFixed(1));
  const cv=document.getElementById('c'+i);
  if(typeof Chart!=='undefined'){
    new Chart(cv,{
      type:'line',
      data:{labels:xs,datasets:[
        {label:'RSS',data:c.rss,borderColor:'#58a6ff',backgroundColor:'transparent',borderWidth:1.5,pointRadius:0,tension:.25},
        {label:'VRAM',data:c.vram,borderColor:'#bc8cff',backgroundColor:'transparent',borderWidth:1.5,pointRadius:0,tension:.25},
      ]},
      options:{responsive:true,interaction:{intersect:false,mode:'index'},
        scales:{x:{ticks:{color:'#5c6773',maxTicksLimit:6,callback:(v,idx)=>xs[idx]+'m'},grid:{color:'#1b232d'}},
                y:{ticks:{color:'#5c6773'},grid:{color:'#1b232d'}}},
        plugins:{legend:{labels:{color:'#8b98a5',boxWidth:10,font:{size:10}}}}}
    });
  } else {
    drawFallback(cv,c);  // CDN unreachable — draw with plain canvas
  }
});

function drawFallback(cv,c){
  const dpr=window.devicePixelRatio||1;
  const w=cv.clientWidth||520, h=160;
  cv.width=w*dpr; cv.height=h*dpr; cv.style.height=h+'px';
  const x=cv.getContext('2d'); x.scale(dpr,dpr);
  const pad={l:46,r:10,t:10,b:18};
  const series=[['#58a6ff',c.rss],['#bc8cff',c.vram]];
  let max=0; series.forEach(([,d])=>d.forEach(v=>{if(v>max)max=v;}));
  if(max<=0)max=1;
  // grid + y labels
  x.strokeStyle='#1b232d'; x.fillStyle='#5c6773'; x.font='10px monospace';
  for(let g=0;g<=3;g++){
    const yy=pad.t+(h-pad.t-pad.b)*g/3;
    x.beginPath();x.moveTo(pad.l,yy);x.lineTo(w-pad.r,yy);x.stroke();
    x.fillText(((max*(3-g)/3)).toFixed(0),4,yy+3);
  }
  series.forEach(([col,d])=>{
    x.strokeStyle=col; x.lineWidth=1.5; x.beginPath();
    d.forEach((v,k)=>{
      const px=pad.l+(w-pad.l-pad.r)*(d.length<2?0:k/(d.length-1));
      const py=pad.t+(h-pad.t-pad.b)*(1-v/max);
      k?x.lineTo(px,py):x.moveTo(px,py);
    });
    x.stroke();
  });
}
</script>
</body></html>""" % {
        "refresh": meta["refresh"],
        "payload": payload,
        "selfpid": os.getpid(),
    }


# ---- main loop --------------------------------------------------------------

def find_root(pid, match):
    if pid:
        try:
            return psutil.Process(pid)
        except psutil.NoSuchProcess:
            sys.exit(f"leakwatch: no process with pid {pid}")
    if match:
        me = os.getpid()
        for p in psutil.process_iter(["pid", "cmdline"]):
            if p.info["pid"] == me:
                continue
            cmd = " ".join(p.info["cmdline"] or [])
            if match in cmd and "leakwatch" not in cmd:
                return p
        sys.exit(f"leakwatch: no process matching {match!r}")
    sys.exit("leakwatch: pass --pid or --match")


def log(logf, msg):
    line = f"{datetime.now(timezone.utc).isoformat(timespec='seconds')}  {msg}"
    print(line, flush=True)
    with open(logf, "a") as f:
        f.write(line + "\n")


def main():
    ap = argparse.ArgumentParser(description="Sidecar leak monitor for a Django process tree.")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--pid", type=int, help="PID of the Django master process to watch")
    g.add_argument("--match", help="substring of the target's command line (e.g. 'manage.py runserver')")
    ap.add_argument("--interval", type=int, default=30, help="seconds between samples (default 30)")
    ap.add_argument("--window-min", type=int, default=60, help="leak-analysis window in minutes (default 60)")
    ap.add_argument("--min-slope-mb-hr", type=float, default=2.0,
                    help="floor-rise above this (MB/hr) flags a leak (default 2.0)")
    ap.add_argument("--retain-hours", type=float, default=240, help="prune samples older than this (default 240h=10d)")
    ap.add_argument("--outdir", default="./leakwatch_out", help="output directory")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    db_path = os.path.join(args.outdir, "leakwatch.db")
    html_path = os.path.join(args.outdir, "dashboard.html")
    log_path = os.path.join(args.outdir, "leakwatch.log")

    store = Store(db_path)
    gpu = GpuProbe()
    root = find_root(args.pid, args.match)
    root_label = label_for(root)

    window_sec = args.window_min * 60
    win_label = f"{args.window_min}m" if args.window_min < 120 else f"{args.window_min/60:.0f}h"

    log(log_path, f"leakwatch start: root pid={root.pid} ({root_label}) "
                  f"interval={args.interval}s window={win_label} "
                  f"gpus={'yes('+str(gpu.count)+')' if gpu.ok else 'none'}")

    already_flagged = set()
    next_prune = time.time() + 3600

    try:
        while True:
            ts = time.time()
            if not root.is_running():
                log(log_path, f"root pid {root.pid} exited — stopping")
                break

            # discover the full live tree, self-describing by command line
            try:
                tree = [root] + root.children(recursive=True)
            except psutil.NoSuchProcess:
                log(log_path, "root vanished mid-scan — stopping")
                break

            vram_map, devices = gpu.vram_by_pid()
            rows = []
            for p in tree:
                try:
                    rss = p.memory_info().rss
                    lab = label_for(p)
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    continue
                rows.append({"pid": p.pid, "label": lab,
                             "rss": rss, "vram": int(vram_map.get(p.pid, 0))})

            store.write_samples(ts, rows)
            store.write_gpus(ts, devices)

            series = store.series_by_label(ts - window_sec)
            verdicts = assess(series, window_sec, args.min_slope_mb_hr)

            # alert on newly flagged leakers
            for v in verdicts:
                if v["leak"] and v["label"] not in already_flagged:
                    already_flagged.add(v["label"])
                    log(log_path,
                        f"LEAK FLAGGED  {v['label']}  "
                        f"floor RSS +{human(v['rss_floor'])}/hr, VRAM +{human(v['vram_floor'])}/hr  "
                        f"(now RSS {human(v['rss_last'])}, VRAM {human(v['vram_last'])})")
                if not v["leak"]:
                    already_flagged.discard(v["label"])

            meta = {
                "root_pid": root.pid, "root_label": root_label,
                "tree_count": len(rows), "window_label": win_label,
                "refresh": max(10, args.interval),
                "updated": datetime.now().strftime("%H:%M:%S"),
            }
            html = render_html(verdicts, series, devices, meta, window_sec)
            tmp = html_path + ".tmp"
            with open(tmp, "w") as f:
                f.write(html)
            os.replace(tmp, html_path)  # atomic; browser never reads a half-written file

            if ts >= next_prune:
                store.prune(ts - args.retain_hours * 3600)
                next_prune = ts + 3600

            time.sleep(args.interval)

    except KeyboardInterrupt:
        log(log_path, "stopped by user")
    finally:
        gpu.shutdown()


if __name__ == "__main__":
    main()