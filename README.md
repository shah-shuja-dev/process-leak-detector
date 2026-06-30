<h1 align="center">Shuja's Leak Detector</h1>

<p align="center">
  <em>A sidecar memory-leak monitor for a process tree.</em><br>
  Watches whatever your app spawns, finds the slow leaker, draws you a live dashboard.
</p>

<p align="center">
  <img alt="language" src="https://img.shields.io/badge/built_with-Rust-orange">
  <img alt="platform" src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows-blue">
  <img alt="gpu" src="https://img.shields.io/badge/GPU-NVML%20auto--detect-76b900">
  <img alt="version" src="https://img.shields.io/badge/version-2.0.0-success">
  <img alt="license" src="https://img.shields.io/badge/license-MIT-lightgrey">
</p>

---

Built for the real-world case of a Django app that fans work out to `subprocess` inference
workers (ONNX Runtime / TensorRT) and slowly bleeds memory until it OOMs days later.
leakwatch runs **beside** the app, not inside it — so it can't crash or leak into your
process — attaches to the master PID, walks the whole child tree, samples CPU RSS and
per-process GPU VRAM on a timer, and flags the worker whose memory floor keeps rising.

Single self-contained binary. No runtime to install on the target box.

## Features

- **Auto-discovers the process tree** from one root PID — no wiring up references; every
  child labels itself by the script it runs.
- **Per-process CPU RSS + GPU VRAM**, GPU attributed by PID via NVML (auto-detects 0/1/N
  GPUs; blank and harmless on machines with no NVIDIA GPU).
- **Slow-leak detection** via floor-rise: tracks the memory a process *never gives back*,
  which is the right signal for sawtooth allocators like ONNX Runtime's arena.
- **Self-refreshing HTML dashboard** with a per-process drift chart (works fully offline —
  falls back to a hand-drawn canvas if the CDN is unreachable).
- **SQLite history** that survives restarts + an append-only log with `LEAK FLAGGED` alerts.

---

## Changelog

### v2.0.0 — the Rust rewrite

So. The whole thing got rewritten from Python into Rust, and I'd be lying if I said that
was painless.

v1 was Python — `psutil`, `pynvml`, a loop, done. It *worked*. It found leaks. But a memory
monitor written in a language with a garbage collector and a fat interpreter is a little bit
of a joke: the watcher itself wants ~30-40 MB resident and pulls in a Python runtime on every
box you want to watch. A tool whose entire job is "notice when memory creeps up" should not
itself be a creeping pile of memory. So: Rust.

And then the fun started.

- `clap` and half the modern crate ecosystem now quietly demand **edition2026**, which the
  distro's Rust (1.75) flatly refuses to build. Cue an afternoon of pinning every dependency
  to the last version that still respects an older compiler.
- `rayon` (pulled in transitively, of course) wanted **rustc 1.80+**. Downgrade. Pin. Re-resolve.
- NVML in Rust is `nvml-wrapper`, which is lovely, but the enum dance for
  `UsedGpuMemory::Used(b)` vs `Unavailable` is *not* the one-liner `pynvml` gave me.
- And porting the floor-rise math meant re-deriving the least-squares slope by hand instead
  of leaning on numpy-adjacent comfort. Worth it, but humbling.

The payoff: a **~3 MB binary**, no Python on the target, near-zero footprint, and it starts
cold and instantly. Same detection logic, same dashboard, same SQLite schema as v1 — just
compiled, and not embarrassing to leave running next to the thing it's judging.

Was it strictly necessary for a leak that takes a week to surface? No. The monitor's overhead
was never the bottleneck. But if you're going to ship a tool that points at other programs and
says *"you're leaking,"* it had better not be leaking itself. Now it doesn't.

### v1.0.0 — Python ~ Now deprecated

Original `psutil` + `pynvml` implementation. Functional, validated, retired in favour of v2.

---

## Build

You need the Rust toolchain (`cargo`). Install once from <https://rustup.rs> (recommended)
or your package manager.

```bash
cd leakwatch-rs
cargo build --release
```

The binary lands at:

- Linux:   `target/release/leakwatch`
- Windows: `target\release\leakwatch.exe`

Copy that single file wherever you need it — it has no other dependencies.

### Build prerequisites

**Linux** — a C toolchain for the bundled SQLite:

```bash
# Debian/Ubuntu
sudo apt-get install -y build-essential
# Fedora/RHEL
sudo dnf install -y gcc
```

GPU support uses NVML, loaded at runtime from the NVIDIA driver (`libnvidia-ml.so`).
Nothing to install at build time. On a box with no NVIDIA GPU, GPU columns stay blank and
CPU monitoring continues.

**Windows** — install the MSVC build tools (the "Desktop development with C++" workload from
the Visual Studio Build Tools) so the bundled SQLite can compile. Then `cargo build --release`
works in PowerShell or cmd. NVML loads at runtime from `nvml.dll`, which ships with the NVIDIA
driver — nothing extra to install.

> **Toolchain note:** dependency versions in `Cargo.lock` are pinned to build on Rust 1.75+.
> A current stable Rust (via rustup) builds it without any pins; the pins only matter on an
> older distro-packaged Rust.

---

## Usage

Point it at your Django/gunicorn master PID:

```bash
# Linux
./target/release/leakwatch --pid 12345

# Windows
.\target\release\leakwatch.exe --pid 12345
```

or find the process by a substring of its command line:

```bash
leakwatch --match "manage.py runserver"
leakwatch --match "gunicorn"
```

Open `leakwatch_out/dashboard.html` in a browser — it refreshes itself. Stop with `Ctrl-C`.

### Tuning for a slow leak (a week to OOM)

```bash
leakwatch --pid 12345 \
  --interval 60 \
  --window-min 360 \
  --min-slope-mb-hr 1.0 \
  --retain-hours 240
```

| flag                | meaning                                                     | default          |
|---------------------|-------------------------------------------------------------|------------------|
| `--pid`             | PID of the master process to watch                          | —                |
| `--match`           | substring of the target's command line (instead of `--pid`) | —                |
| `--interval`        | seconds between samples                                     | 30               |
| `--window-min`      | leak-analysis window in minutes (longer = steadier slope)   | 60               |
| `--min-slope-mb-hr` | floor-rise above this (MB/hr) flags a leak                  | 2.0              |
| `--retain-hours`    | prune samples older than this                               | 240              |
| `--outdir`          | output directory                                            | `./leakwatch_out`|

Leave it running for hours to days. The leaker is the row whose **floor** keeps rising.

### Keeping it running after logout

```bash
# Linux
nohup ./leakwatch --pid 12345 --interval 60 --window-min 360 > lw.out 2>&1 &
```

On **Windows**, run it as a Scheduled Task or via a service wrapper like NSSM, pointing at
`leakwatch.exe` with the same flags.

---

## How leak detection works

Two signals per process, grouped by the script name it runs (so history survives PIDs
restarting):

1. **Slope** — least-squares trend of memory over the window, in MB/hr.
2. **Floor-rise** — trend of the running minimum. A true leak raises the floor: memory the
   process never gives back. Cleaner than peak growth for sawtooth allocators like ONNX
   Runtime's arena, which spike and partly release on every call.

A process is flagged only when the floor keeps rising **and** the overall trend agrees, on
RSS or VRAM. That combination rejects normal allocation jitter.

## Outputs (`leakwatch_out/`)

| file             | what it is                                                                 |
|------------------|----------------------------------------------------------------------------|
| `dashboard.html` | live panel — GPU usage, per-process table with leak flags, per-process drift charts (offline-capable) |
| `leakwatch.db`   | SQLite history; survives restarts                                          |
| `leakwatch.log`  | start line + a `LEAK FLAGGED` entry the moment a process trips             |

---

## ONNX Runtime / TensorRT leak playbook

Once leakwatch names the worker, check in order:

1. **A session/context created per inference instead of once** — the #1 cause. Reuse one
   `InferenceSession` / TensorRT execution context for the worker's life.
2. **ONNX Runtime arena growth on variable input sizes** — set
   `arena_extend_strategy = kSameAsRequested`, or pad inputs to fixed buckets.
3. **TensorRT dynamic shapes** churning allocations — prefer fixed shapes.
4. **Output / IO buffers reallocated per call** — pre-allocate and reuse.
5. **CUDA streams / graphs created per call.**

**Stopgap** while you investigate: recycle a worker after N inferences or above a memory
threshold (gunicorn `max_requests` style) to kill the OOM without finding the leak first.

---

## Author

**Syed Shuja Hussain Shah**

## License

MIT