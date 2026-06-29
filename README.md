# leakwatch

A sidecar memory-leak monitor for a Django app that spawns inference subprocesses.

It watches the **process tree your Django creates**, samples CPU RSS and per-process
GPU VRAM on a timer, stores history in SQLite, fits a slope over a long window to flag
**slow leaks**, and writes a self-refreshing **HTML dashboard** plus an append-only **log**.

It runs as a separate process and only *reads* your app, so it can never crash or leak
into Django, and the same script runs unchanged in production.

## Why a sidecar, not a thread monitor

Your heavy work runs in `subprocess` children with their own memory spaces. A thread
monitor inside Django would watch the wrong thing. The OS already tracks the parent→child
tree for you; leakwatch reads it with `psutil.Process(pid).children(recursive=True)` and
samples every process it finds. Whatever Django spawns names itself via its command line,
so "which module" is answered automatically — no references to wire up.

## Install

    pip install psutil nvidia-ml-py

`nvidia-ml-py` is optional. With no NVIDIA GPU/driver, GPU columns stay blank and CPU
monitoring continues. GPU count (0, 1, or N) is auto-detected — no configuration.

## Run

Point it at your Django master process:

    python3 leakwatch.py --pid 12345

or find it by command line:

    python3 leakwatch.py --match "manage.py runserver"
    python3 leakwatch.py --match "gunicorn"

Open `leakwatch_out/dashboard.html` in a browser. It refreshes itself.

### Tuning for a slow leak (a week to OOM)

Default settings already suit a slow leak, but for a leak this gradual, widen the window
so the slope is statistically solid and the threshold is sensitive:

    python3 leakwatch.py --pid 12345 \
        --interval 60 \
        --window-min 360 \
        --min-slope-mb-hr 1.0 \
        --retain-hours 240

- `--interval`     seconds between samples (60 is plenty for a week-long leak)
- `--window-min`   leak-analysis window; longer = steadier slope on slow leaks
- `--min-slope-mb-hr`  floor-rise above this flags a leak; lower = more sensitive
- `--retain-hours`     how much history to keep in SQLite (default 10 days)

Leave it running for hours to days. The leaker is the row whose **floor** keeps rising.

## How leak detection works

Two signals per process, grouped by script name (so it survives PIDs restarting):

1. **Slope** — least-squares trend of memory over the window, in MB/hr.
2. **Floor-rise** — trend of the *running minimum*. A true leak raises the floor:
   memory the process never gives back. This is cleaner than peak growth for sawtooth
   allocators like ONNX Runtime's arena, which spike and partly release on every call.

A process is flagged only when the floor keeps rising **and** the overall trend agrees,
on RSS or VRAM. That combination rejects normal allocation jitter.

## Outputs (in `leakwatch_out/`)

- `dashboard.html` — live panel: GPU usage, a per-process table with leak flags, and a
  drift chart per process. Charts use Chart.js from a CDN, with a plain-canvas fallback
  if the CDN is unreachable.
- `leakwatch.db` — SQLite history; survives restarts.
- `leakwatch.log` — start line + a `LEAK FLAGGED` entry the moment a process trips.

---

## What's probably leaking (ONNX Runtime / TensorRT, long-running workers)

Once leakwatch names the worker, here's where to look, in order:

1. **A session/context created per inference instead of once.** The #1 cause. Each
   `ort.InferenceSession(...)` or new TensorRT execution context leaks a little GPU+CPU
   that never fully frees. Create them once at worker startup and reuse.

2. **ONNX Runtime arena growth on variable input sizes.** The arena grows to the largest
   input seen and doesn't shrink by default — a steady upward ratchet if your inputs vary
   (resolutions, sequence lengths, dynamic batch). Fixes: set
   `arena_extend_strategy = kSameAsRequested`, pad inputs to fixed buckets, or configure
   arena shrinkage.

3. **TensorRT dynamic shapes** — repeatedly setting varying input dims on the context can
   accumulate. Fixed shapes avoid it.

4. **Output/IO buffers reallocated per call** instead of pre-allocated and reused.

5. **CUDA streams/graphs created per call.**

## Stopgap while you investigate

For a leak this slow, the fastest way to kill the OOM today is to **recycle a worker
after N inferences or above a memory threshold** — like gunicorn's `max_requests`. It
doesn't fix the leak but removes the crash while you apply the real fix above. leakwatch's
log + threshold tells you the right N.
