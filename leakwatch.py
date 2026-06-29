#!/usr/bin/env python3
"""
leakwatch — a sidecar memory-leak monitor for a Django app that spawns subprocesses.

Usage
-----
    python3 leakwatch.py --pid 12345
    python3 leakwatch.py --match "manage.py runserver"
    python3 leakwatch.py --pid 12345 --interval 30 --window-min 60 --outdir ./leakwatch_out
"""

import argparse
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
    """A stable, human label for a process: the script name it's running."""
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
    xs = [(t - t0) for t in ts]
    mx = sum(xs) / n
    my = sum(ys) / n
    num = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    den = sum((x - mx) ** 2 for x in xs)
    if den == 0:
        return None
    per_sec = num / den
    return per_sec * 3600.0


def floor_rise_per_hour(ts, ys, buckets=8):
    """Rise of the running-minimum (the floor) per hour."""
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

            for v in verdicts:
                if v["leak"] and v["label"] not in already_flagged:
                    already_flagged.add(v["label"])
                    log(log_path,
                        f"LEAK FLAGGED  {v['label']}  "
                        f"floor RSS +{human(v['rss_floor'])}/hr, VRAM +{human(v['vram_floor'])}/hr  "
                        f"(now RSS {human(v['rss_last'])}, VRAM {human(v['vram_last'])})")
                if not v["leak"]:
                    already_flagged.discard(v["label"])

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