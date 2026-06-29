# sysjsonl — whole-machine resource sampler

Logs the **entire machine's** CPU, memory, and GPU memory to JSONL, one line per sample.
Separate from `leakwatch` (which watches your Django process tree) — use this to correlate
memory behaviour across programs that have nothing to do with your app, and across machines.

Cross-platform: same script, same JSON schema, on **Linux and Windows**.

## Install

    pip install psutil
    pip install nvidia-ml-py     # optional — direct GPU readout without nvidia-smi

`nvidia-ml-py` is optional. GPU is read in this order: NVML (if NVIDIA driver present) →
`nvidia-smi` on PATH → otherwise GPU fields are `null`. The schema never changes, so a
machine with no GPU produces lines that line up column-for-column with a GPU machine.

## Run

**Linux**

    python3 sysjsonl.py --interval 10 --out /var/log/sysmon.jsonl

Leave it running (background it, or use systemd / `nohup ... &` / `tmux`).

**Windows**

    python sysjsonl.py --interval 10 --out C:\logs\sysmon.jsonl

For unattended runs on Windows, either keep a terminal open, or use **Task Scheduler**
with `--once` on a repeating trigger (one sample per run appends one line).

**One-shot** (for cron / Task Scheduler):

    python sysjsonl.py --once --out /path/sysmon.jsonl

**Rotate** the file when it gets big (keeps comparisons manageable):

    python sysjsonl.py --interval 10 --rotate-mb 100 --out sysmon.jsonl

## Schema (one JSON object per line)

    {
      "ts": 1719660000.0,                       // unix seconds
      "iso": "2026-06-29T11:40:00+00:00",       // UTC
      "host": "boxname",
      "cpu": {"pct": 23.4, "count": 16, "load1": 1.2},   // load1 null on Windows
      "mem": {"total": <bytes>, "used": <bytes>,
              "available": <bytes>, "pct": 41.0},
      "gpu": {"available": true, "source": "nvml", "count": 1,
              "mem_total": <bytes>, "mem_used": <bytes>, "mem_free": <bytes>,
              "pct": 62.0,
              "devices": [{"index": 0, "name": "...",
                           "mem_total": <bytes>, "mem_used": <bytes>,
                           "util_gpu_pct": 55.0, "temp_c": 64.0}]}
    }

All sizes are **bytes**. Percentages are 0-100 floats. Anything unreadable on a given
machine is `null`, never an error.

## Analysing

Load straight into pandas:

    import pandas as pd, json
    rows = [json.loads(l) for l in open("sysmon.jsonl")]
    df = pd.json_normalize(rows)            # columns like cpu.pct, mem.pct, gpu.pct
    df["t"] = pd.to_datetime(df["iso"])
    df.set_index("t")[["mem.pct", "gpu.pct"]].plot()

To compare two machines, load both, tag with `host`, and concat — the identical schema
means columns align directly. A program with a leak shows whole-machine `mem.used` or
`gpu.mem_used` ratcheting up while your app's own tree (from leakwatch) stays flat — that's
how you tell "it's not me."
