# leakwatch

A memory-leak monitor for Python applications that spawn inference subprocesses. Watches process trees, detects slow memory leaks, and provides a real-time dashboard.

## Author
:gem: [@shah-shuja-dev](https://github.com/shah-shuja-dev)

## Why leakwatch?

When your Python app spawns heavy inference workers (ONNX Runtime, TensorRT, PyTorch), memory leaks can take days to crash your service. Traditional monitors watch the main process and miss what's happening in subprocesses. leakwatch watches the **entire process tree** — parent and all children — so leaks in workers are caught wherever they happen.

## Installation

```bash
pip install psutil nvidia-ml-py
```

`nvidia-ml-py` is optional. Without an NVIDIA GPU or driver, GPU monitoring is disabled automatically. CPU monitoring continues normally. GPU count (0, 1, or N) is auto-detected — no configuration needed.

## Quick Start

Point leakwatch at your process:

```bash
# By PID
python3 leakwatch.py --pid 12345

# By command name
python3 leakwatch.py --match "manage.py runserver"
python3 leakwatch.py --match "gunicorn"
```

Open `leakwatch_out/dashboard.html` in a browser. It auto-refreshes.

## How It Works

### Process Tree Monitoring

| Component             | Description                                                                  |
| --------------------- | ---------------------------------------------------------------------------- |
| **Process Discovery** | Uses `psutil.Process(pid).children(recursive=True)` to find all subprocesses |
| **Memory Sampling**   | Records RSS (CPU memory) and VRAM (GPU memory) per process at intervals      |
| **Grouping**          | Processes grouped by script name, surviving PID restarts                     |
| **Storage**           | SQLite database stores history, survives process restarts                    |

### Leak Detection Method

Two signals are analyzed per process group:

| Signal         | What It Measures                                | Why It Matters                                                    |
| -------------- | ----------------------------------------------- | ----------------------------------------------------------------- |
| **Slope**      | Least-squares trend of memory over time (MB/hr) | Overall growth rate                                               |
| **Floor-rise** | Trend of running minimum memory                 | A true leak never releases memory; this ignores allocation spikes |

A process is flagged only when **both signals agree** — floor keeps rising AND overall trend confirms growth. This filters out normal allocation jitter and sawtooth patterns from arena allocators.

### Detection Parameters

```
Growth
  ↑
  │     ╱╲    ╱╲  ← Arena spikes
```
