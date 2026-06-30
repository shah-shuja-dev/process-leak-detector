# pyleak

Lightweight Rust CLI that statically scans a Python project for patterns
that commonly cause memory leaks, and reports the specific functions
responsible. Produces a self-contained, dark-themed HTML report.

## Build

```
cargo build --release
```

## Run

```
./target/release/pyleak path/to/script.py
```

(Windows: `.\target\release\pyleak.exe path\to\script.py`)

By default this writes a timestamped HTML report — e.g.
`pyleak_report_script_20260630_141533.html` — straight to your **Desktop**,
cross-platform (Windows %USERPROFILE%\Desktop incl. OneDrive-redirected,
macOS ~/Desktop, Linux XDG Desktop). If no Desktop folder can be resolved it
falls back to the current directory.

```
./target/release/pyleak path/to/script.py -o C:\some\report\folder
```

`-o/--output-dir` overrides the destination and is created automatically,
including any missing parent directories, if it doesn't already exist.

Other flags:

```
--json         print raw JSON to stdout instead of writing HTML
--also-print   also print the plain-text report to the terminal
```

Given an entry script, pyleak scans the *entire surrounding directory tree*
(skipping .venv/.git/__pycache__), so helper scripts and modules spawned via
`subprocess`, `os.system`, or `multiprocessing.Process` are scanned too. The
HTML report includes an "execution tree" table showing every spawn/import
point it found, for transparency.

## What it checks (static, no execution)

- **unbounded-global-container** — a module-level list/dict/set that's
  appended to somewhere but never cleared/popped/reassigned anywhere in the
  file (the single most common real-world Python leak).
- **unbounded-cache** — `@lru_cache` / `@cache` with no `maxsize`.
- **unclosed-file-handle** — `x = open(...)` outside a `with` block with no
  matching `x.close()` in scope.
- **dangling-thread** — `threading.Thread`/`Timer` created without
  `daemon=True` and never `.join()`-ed.
- **unremoved-listener** — a list named like a listener/callback/handler
  registry that's appended to but never has entries removed.
- **del-with-possible-cycle** — classes defining `__del__`, which can delay
  or block garbage collection if the instance ends up in a reference cycle.

Try it on `demo_project/` (included) to see all six checks fire:

```
./target/release/pyleak demo_project/main.py
```

## Notes / limitations

This is a heuristic, indentation-based scanner — not a full Python parser
and not a runtime profiler. It will have false positives/negatives on
unusual code (e.g. containers cleared via a helper function whose name it
can't resolve). Treat findings as "worth a human look," not proof. For a
runtime-confirmed version (tracking real RSS growth and function-level
allocation diffs via `tracemalloc`), that's the natural next step.
