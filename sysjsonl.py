#!/usr/bin/env python3
"""
sysjsonl — whole-machine resource sampler. Cross-platform (Linux + Windows).

Every interval it appends one JSON object per line to a .jsonl file:

    {"ts": 1719660000.0, "iso": "2026-06-29T11:40:00+00:00", "host": "boxname",
     "cpu": {"pct": 23.4, "count": 16, "load1": 1.2},
     "mem": {"total": ..., "used": ..., "available": ..., "pct": 41.0},
     "gpu": {"available": true, "source": "nvml", "count": 1,
             "mem_total": ..., "mem_used": ..., "mem_free": ..., "pct": 62.0,
             "devices": [{"index":0,"name":"...","mem_total":...,"mem_used":...,
                          "util_gpu_pct":...,"temp_c":...}]}}

This samples the ENTIRE machine, not one process tree — so you can correlate memory
behaviour across programs that are completely separate from your Django app.

All sizes are bytes. Percentages are 0-100 floats. Any field that can't be read on a
given machine is null, never an exception, and the schema is identical on every OS so
two machines' logs are directly comparable.

Usage
-----
    python sysjsonl.py                       # 5s interval, ./sysmon.jsonl
    python sysjsonl.py --interval 10 --out /var/log/sysmon.jsonl
    python sysjsonl.py --once                # one sample, then exit (for cron/Task Scheduler)
    python sysjsonl.py --rotate-mb 100       # start a new file when current exceeds 100 MB

Install
-------
    pip install psutil
    pip install nvidia-ml-py     # optional; enables direct GPU readout without nvidia-smi
"""

import argparse
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone

import psutil


# ---- GPU layer: NVML primary, nvidia-smi fallback, null if neither ----------

class GpuReader:
    def __init__(self):
        self.source = None        # "nvml" | "nvidia-smi" | None
        self._nvml = None
        self._handles = []
        self._names = []
        self._init_nvml()
        if self.source is None:
            self._init_smi()

    def _init_nvml(self):
        try:
            import warnings
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                import pynvml  # provided by either pynvml or nvidia-ml-py
            pynvml.nvmlInit()
            n = pynvml.nvmlDeviceGetCount()
            if n <= 0:
                pynvml.nvmlShutdown()
                return
            for i in range(n):
                h = pynvml.nvmlDeviceGetHandleByIndex(i)
                self._handles.append(h)
                name = pynvml.nvmlDeviceGetName(h)
                self._names.append(name.decode() if isinstance(name, bytes) else name)
            self._nvml = pynvml
            self.source = "nvml"
        except Exception:
            self._nvml = None
            self._handles = []

    def _init_smi(self):
        # only usable if the binary is actually on PATH
        if shutil.which("nvidia-smi"):
            try:
                out = subprocess.run(
                    ["nvidia-smi", "--query-gpu=memory.total",
                     "--format=csv,noheader,nounits"],
                    capture_output=True, text=True, timeout=8,
                )
                if out.returncode == 0 and out.stdout.strip():
                    self.source = "nvidia-smi"
            except Exception:
                pass

    def read(self):
        if self.source == "nvml":
            return self._read_nvml()
        if self.source == "nvidia-smi":
            return self._read_smi()
        return {"available": False, "source": None, "count": 0,
                "mem_total": None, "mem_used": None, "mem_free": None,
                "pct": None, "devices": []}

    def _read_nvml(self):
        nvml = self._nvml
        devices, total, used = [], 0, 0
        for idx, h in enumerate(self._handles):
            d = {"index": idx, "name": self._names[idx],
                 "mem_total": None, "mem_used": None,
                 "util_gpu_pct": None, "temp_c": None}
            try:
                m = nvml.nvmlDeviceGetMemoryInfo(h)
                d["mem_total"], d["mem_used"] = m.total, m.used
                total += m.total; used += m.used
            except Exception:
                pass
            try:
                u = nvml.nvmlDeviceGetUtilizationRates(h)
                d["util_gpu_pct"] = float(u.gpu)
            except Exception:
                pass
            try:
                d["temp_c"] = float(nvml.nvmlDeviceGetTemperature(
                    h, nvml.NVML_TEMPERATURE_GPU))
            except Exception:
                pass
            devices.append(d)
        return {"available": True, "source": "nvml", "count": len(devices),
                "mem_total": total or None, "mem_used": used or None,
                "mem_free": (total - used) if total else None,
                "pct": round(100 * used / total, 1) if total else None,
                "devices": devices}

    def _read_smi(self):
        try:
            out = subprocess.run(
                ["nvidia-smi",
                 "--query-gpu=index,name,memory.total,memory.used,utilization.gpu,temperature.gpu",
                 "--format=csv,noheader,nounits"],
                capture_output=True, text=True, timeout=8,
            )
            if out.returncode != 0:
                return {"available": False, "source": "nvidia-smi", "count": 0,
                        "mem_total": None, "mem_used": None, "mem_free": None,
                        "pct": None, "devices": []}
            devices, total, used = [], 0, 0
            for line in out.stdout.strip().splitlines():
                parts = [p.strip() for p in line.split(",")]
                if len(parts) < 6:
                    continue
                idx, name, mt, mu, ug, tc = parts[:6]
                mt_b = int(float(mt)) * 1024 * 1024   # MiB -> bytes
                mu_b = int(float(mu)) * 1024 * 1024
                total += mt_b; used += mu_b
                devices.append({
                    "index": int(idx), "name": name,
                    "mem_total": mt_b, "mem_used": mu_b,
                    "util_gpu_pct": _f(ug), "temp_c": _f(tc),
                })
            return {"available": True, "source": "nvidia-smi", "count": len(devices),
                    "mem_total": total or None, "mem_used": used or None,
                    "mem_free": (total - used) if total else None,
                    "pct": round(100 * used / total, 1) if total else None,
                    "devices": devices}
        except Exception:
            return {"available": False, "source": "nvidia-smi", "count": 0,
                    "mem_total": None, "mem_used": None, "mem_free": None,
                    "pct": None, "devices": []}

    def shutdown(self):
        if self._nvml:
            try:
                self._nvml.nvmlShutdown()
            except Exception:
                pass


def _f(s):
    try:
        return float(s)
    except (TypeError, ValueError):
        return None


# ---- sampling ---------------------------------------------------------------

def sample(gpu, host):
    vm = psutil.virtual_memory()
    # load average: Unix-only; psutil emulates it on Windows but can raise on some builds
    try:
        load1 = os.getloadavg()[0]
    except (OSError, AttributeError):
        load1 = None
    return {
        "ts": round(time.time(), 3),
        "iso": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "host": host,
        "cpu": {
            "pct": psutil.cpu_percent(interval=None),
            "count": psutil.cpu_count(),
            "load1": round(load1, 2) if load1 is not None else None,
        },
        "mem": {
            "total": vm.total, "used": vm.used,
            "available": vm.available, "pct": vm.percent,
        },
        "gpu": gpu.read(),
    }


def maybe_rotate(path, rotate_mb):
    if not rotate_mb:
        return path
    try:
        if os.path.exists(path) and os.path.getsize(path) > rotate_mb * 1024 * 1024:
            stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
            root, ext = os.path.splitext(path)
            os.replace(path, f"{root}.{stamp}{ext}")
    except OSError:
        pass
    return path


def main():
    ap = argparse.ArgumentParser(
        description="Whole-machine CPU/MEM/GPU sampler to JSONL (Linux + Windows).")
    ap.add_argument("--out", default="sysmon.jsonl", help="output JSONL path")
    ap.add_argument("--interval", type=float, default=5.0, help="seconds between samples")
    ap.add_argument("--once", action="store_true", help="write a single sample then exit")
    ap.add_argument("--rotate-mb", type=float, default=0,
                    help="rotate the file once it exceeds this many MB (0 = never)")
    args = ap.parse_args()

    host = socket.gethostname()
    gpu = GpuReader()
    # prime cpu_percent so the first real sample isn't 0.0
    psutil.cpu_percent(interval=None)
    time.sleep(0.1)

    sys.stderr.write(
        f"sysjsonl: host={host} os={platform.system()} "
        f"gpu={gpu.source or 'none'} -> {args.out}\n")
    sys.stderr.flush()

    def write_one():
        path = maybe_rotate(args.out, args.rotate_mb)
        rec = sample(gpu, host)
        # write atomically-ish: one line, flushed; JSONL tolerates partial tail anyway
        with open(path, "a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
            f.flush()
        return rec

    try:
        if args.once:
            write_one()
        else:
            while True:
                write_one()
                time.sleep(args.interval)
    except KeyboardInterrupt:
        sys.stderr.write("sysjsonl: stopped\n")
    finally:
        gpu.shutdown()


if __name__ == "__main__":
    main()
