//! leakwatch — sidecar memory-leak monitor for a process tree (e.g. a Django app
//! that spawns inference subprocesses). Cross-platform (Linux + Windows).
//!
//! Watches the process tree rooted at a given PID, samples CPU RSS and per-process
//! GPU VRAM on a timer, stores history in SQLite, fits a slope over a long window to
//! flag slow leaks, and writes a self-refreshing HTML dashboard plus an append-only log.
//!
//! It only reads your app, so it can't crash or leak into it, and the same binary runs
//! unchanged in production.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, Utc};
use clap::Parser;
use rusqlite::Connection;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

#[derive(Parser, Debug)]
#[command(name = "leakwatch", about = "Sidecar leak monitor for a process tree.")]
struct Args {
    /// PID of the master process to watch (e.g. your Django/gunicorn master)
    #[arg(long)]
    pid: Option<u32>,

    /// Substring of the target's command line (alternative to --pid)
    #[arg(long)]
    r#match: Option<String>,

    /// Seconds between samples
    #[arg(long, default_value_t = 30)]
    interval: u64,

    /// Leak-analysis window in minutes
    #[arg(long = "window-min", default_value_t = 60)]
    window_min: u64,

    /// Floor-rise above this (MB/hr) flags a leak
    #[arg(long = "min-slope-mb-hr", default_value_t = 2.0)]
    min_slope_mb_hr: f64,

    /// Prune samples older than this many hours
    #[arg(long = "retain-hours", default_value_t = 240.0)]
    retain_hours: f64,

    /// Output directory
    #[arg(long, default_value = "./leakwatch_out")]
    outdir: String,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

// ---- GPU layer: NVML, degrades to none ------------------------------------

struct GpuDevice {
    index: u32,
    name: String,
    used: u64,
    total: u64,
}

struct GpuProbe {
    nvml: Option<nvml_wrapper::Nvml>,
}

impl GpuProbe {
    fn new() -> Self {
        let nvml = nvml_wrapper::Nvml::init().ok();
        GpuProbe { nvml }
    }

    fn ok(&self) -> bool {
        self.nvml
            .as_ref()
            .and_then(|n| n.device_count().ok())
            .map(|c| c > 0)
            .unwrap_or(false)
    }

    fn count(&self) -> u32 {
        self.nvml
            .as_ref()
            .and_then(|n| n.device_count().ok())
            .unwrap_or(0)
    }

    /// Returns (vram_by_pid, device_totals).
    fn sample(&self) -> (HashMap<u32, u64>, Vec<GpuDevice>) {
        let mut by_pid: HashMap<u32, u64> = HashMap::new();
        let mut devices = Vec::new();
        let nvml = match &self.nvml {
            Some(n) => n,
            None => return (by_pid, devices),
        };
        let count = nvml.device_count().unwrap_or(0);
        for i in 0..count {
            let dev = match nvml.device_by_index(i) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let name = dev.name().unwrap_or_else(|_| format!("GPU{i}"));
            let (used, total) = match dev.memory_info() {
                Ok(m) => (m.used, m.total),
                Err(_) => (0, 0),
            };
            devices.push(GpuDevice { index: i, name, used, total });
            if let Ok(procs) = dev.running_compute_processes() {
                for p in procs {
                    let mem = match p.used_gpu_memory {
                        nvml_wrapper::enums::device::UsedGpuMemory::Used(b) => b,
                        nvml_wrapper::enums::device::UsedGpuMemory::Unavailable => 0,
                    };
                    if mem > 0 {
                        *by_pid.entry(p.pid).or_insert(0) += mem;
                    }
                }
            }
        }
        (by_pid, devices)
    }
}

// ---- identity: label a process by the script it runs ----------------------

fn label_for(sys: &System, pid: Pid) -> String {
    let proc = match sys.process(pid) {
        Some(p) => p,
        None => return "gone".to_string(),
    };
    let cmd = proc.cmd();
    if cmd.is_empty() {
        return proc.name().to_string();
    }
    let exe = basename(&cmd[0]);
    // first real script token: a .py file, or a path-like token (not flags, not /dev,/tmp)
    for tok in cmd.iter().skip(1) {
        if tok.starts_with('-') {
            continue;
        }
        if tok.ends_with(".py") {
            return format!("{exe}:{}", basename(tok));
        }
        if tok.contains('/') && !tok.starts_with("/dev") && !tok.starts_with("/tmp") {
            let b = basename(tok);
            if !b.is_empty() {
                return format!("{exe}:{b}");
            }
        }
    }
    exe
}

fn basename(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).to_string()
}

// ---- process tree walk -----------------------------------------------------

fn collect_tree(sys: &System, root: Pid) -> Vec<Pid> {
    // build child adjacency from parent pointers
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, proc) in sys.processes() {
        if let Some(parent) = proc.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut out = vec![root];
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if let Some(kids) = children.get(&p) {
            for &k in kids {
                out.push(k);
                stack.push(k);
            }
        }
    }
    out
}

// ---- storage ---------------------------------------------------------------

struct Store {
    db: Connection,
}

impl Store {
    fn new(path: &PathBuf) -> Self {
        let db = Connection::open(path).expect("open sqlite");
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS samples(
                ts REAL NOT NULL, pid INTEGER NOT NULL, label TEXT NOT NULL,
                rss INTEGER NOT NULL, vram INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(ts);
             CREATE TABLE IF NOT EXISTS gpus(
                ts REAL NOT NULL, idx INTEGER NOT NULL, name TEXT,
                used INTEGER, total INTEGER);",
        )
        .expect("schema");
        Store { db }
    }

    fn write_samples(&self, ts: f64, rows: &[Row]) {
        let tx = self.db.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO samples(ts,pid,label,rss,vram) VALUES(?,?,?,?,?)",
                )
                .unwrap();
            for r in rows {
                stmt.execute(rusqlite::params![ts, r.pid, r.label, r.rss as i64, r.vram as i64])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    fn write_gpus(&self, ts: f64, devices: &[GpuDevice]) {
        let tx = self.db.unchecked_transaction().unwrap();
        {
            let mut stmt = tx
                .prepare_cached("INSERT INTO gpus(ts,idx,name,used,total) VALUES(?,?,?,?,?)")
                .unwrap();
            for d in devices {
                stmt.execute(rusqlite::params![ts, d.index, d.name, d.used as i64, d.total as i64])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }

    /// Aggregate by label so history survives PID churn: label -> (ts, rss, vram) series.
    fn series_by_label(&self, since: f64) -> HashMap<String, Series> {
        let mut stmt = self
            .db
            .prepare(
                "SELECT label, ts, SUM(rss), SUM(vram) FROM samples
                 WHERE ts>=? GROUP BY label, ts ORDER BY ts",
            )
            .unwrap();
        let rows = stmt
            .query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .unwrap();
        let mut out: HashMap<String, Series> = HashMap::new();
        for row in rows.flatten() {
            let (label, ts, rss, vram) = row;
            let s = out.entry(label).or_default();
            s.ts.push(ts);
            s.rss.push(rss.max(0) as f64);
            s.vram.push(vram.max(0) as f64);
        }
        out
    }

    fn prune(&self, before: f64) {
        let _ = self.db.execute("DELETE FROM samples WHERE ts<?", [before]);
        let _ = self.db.execute("DELETE FROM gpus WHERE ts<?", [before]);
    }
}

struct Row {
    pid: u32,
    label: String,
    rss: u64,
    vram: u64,
}

#[derive(Default, Clone)]
struct Series {
    ts: Vec<f64>,
    rss: Vec<f64>,
    vram: Vec<f64>,
}

// ---- leak math -------------------------------------------------------------

fn slope_per_hour(ts: &[f64], ys: &[f64]) -> Option<f64> {
    let n = ts.len();
    if n < 6 {
        return None;
    }
    let t0 = ts[0];
    let xs: Vec<f64> = ts.iter().map(|t| t - t0).collect();
    let mx = xs.iter().sum::<f64>() / n as f64;
    let my = ys.iter().sum::<f64>() / n as f64;
    let num: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if den == 0.0 {
        return None;
    }
    Some((num / den) * 3600.0)
}

/// Rise of the running-minimum (the floor) per hour — memory never given back.
fn floor_rise_per_hour(ts: &[f64], ys: &[f64]) -> Option<f64> {
    let buckets = 8usize;
    let n = ts.len();
    if n < buckets * 2 {
        return None;
    }
    let span = ts[n - 1] - ts[0];
    if span <= 0.0 {
        return None;
    }
    let size = span / buckets as f64;
    let mut mt = Vec::new();
    let mut my = Vec::new();
    for b in 0..buckets {
        let lo = ts[0] + b as f64 * size;
        let hi = ts[0] + (b as f64 + 1.0) * size;
        let mut best: Option<(f64, f64)> = None;
        for (t, y) in ts.iter().zip(ys) {
            if *t >= lo && *t < hi {
                if best.map(|(_, by)| *y < by).unwrap_or(true) {
                    best = Some((*t, *y));
                }
            }
        }
        if let Some((t, y)) = best {
            mt.push(t);
            my.push(y);
        }
    }
    slope_per_hour(&mt, &my)
}

#[derive(serde::Serialize, Clone)]
struct Verdict {
    label: String,
    samples: usize,
    rss_last: f64,
    vram_last: f64,
    rss_slope: f64,
    rss_floor: f64,
    vram_slope: f64,
    vram_floor: f64,
    leak: bool,
    leak_kind: String,
}

fn assess(series: &HashMap<String, Series>, window: f64, min_slope_mb_hr: f64) -> Vec<Verdict> {
    let cutoff = now_secs() - window;
    let thr = min_slope_mb_hr * 1024.0 * 1024.0;
    let mut out = Vec::new();
    for (label, s) in series {
        let keep: Vec<usize> = (0..s.ts.len()).filter(|&i| s.ts[i] >= cutoff).collect();
        if keep.len() < 6 {
            continue;
        }
        let ts: Vec<f64> = keep.iter().map(|&i| s.ts[i]).collect();
        let rss: Vec<f64> = keep.iter().map(|&i| s.rss[i]).collect();
        let vram: Vec<f64> = keep.iter().map(|&i| s.vram[i]).collect();
        let rss_slope = slope_per_hour(&ts, &rss).unwrap_or(0.0);
        let rss_floor = floor_rise_per_hour(&ts, &rss).unwrap_or(0.0);
        let vram_slope = slope_per_hour(&ts, &vram).unwrap_or(0.0);
        let vram_floor = floor_rise_per_hour(&ts, &vram).unwrap_or(0.0);
        let rss_leak = rss_floor > thr && rss_slope > thr * 0.5;
        let vram_leak = vram_floor > thr && vram_slope > thr * 0.5;
        let mut kind = String::new();
        if vram_leak {
            kind.push_str("VRAM");
        }
        if rss_leak && vram_leak {
            kind.push('+');
        }
        if rss_leak {
            kind.push_str("RSS");
        }
        out.push(Verdict {
            label: label.clone(),
            samples: keep.len(),
            rss_last: *rss.last().unwrap(),
            vram_last: *vram.last().unwrap(),
            rss_slope,
            rss_floor,
            vram_slope,
            vram_floor,
            leak: rss_leak || vram_leak,
            leak_kind: kind,
        });
    }
    out.sort_by(|a, b| {
        b.rss_floor
            .max(b.vram_floor)
            .partial_cmp(&a.rss_floor.max(a.vram_floor))
            .unwrap()
    });
    out
}

fn human(n: f64) -> String {
    let mut x = n;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if x.abs() < 1024.0 {
            return format!("{x:.1} {unit}");
        }
        x /= 1024.0;
    }
    format!("{x:.1} PB")
}

// ---- dashboard -------------------------------------------------------------

#[derive(serde::Serialize)]
struct ChartSeries {
    t: Vec<f64>,
    rss: Vec<f64>,
    vram: Vec<f64>,
}

#[derive(serde::Serialize)]
struct GpuJson {
    index: u32,
    name: String,
    used: u64,
    total: u64,
}

#[derive(serde::Serialize)]
struct Meta {
    root_pid: u32,
    root_label: String,
    tree_count: usize,
    window_label: String,
    refresh: u64,
    updated: String,
}

#[derive(serde::Serialize)]
struct Payload {
    verdicts: Vec<Verdict>,
    charts: HashMap<String, ChartSeries>,
    gpus: Vec<GpuJson>,
    meta: Meta,
}

fn render_html(
    verdicts: Vec<Verdict>,
    series: &HashMap<String, Series>,
    devices: &[GpuDevice],
    meta: Meta,
    window: f64,
) -> String {
    let cutoff = now_secs() - window;
    let mut charts: HashMap<String, ChartSeries> = HashMap::new();
    for (label, s) in series {
        let keep: Vec<usize> = (0..s.ts.len()).filter(|&i| s.ts[i] >= cutoff).collect();
        if keep.len() < 2 {
            continue;
        }
        charts.insert(
            label.clone(),
            ChartSeries {
                t: keep.iter().map(|&i| s.ts[i]).collect(),
                rss: keep.iter().map(|&i| s.rss[i] / 1_048_576.0).collect(),
                vram: keep.iter().map(|&i| s.vram[i] / 1_048_576.0).collect(),
            },
        );
    }
    let gpus: Vec<GpuJson> = devices
        .iter()
        .map(|d| GpuJson {
            index: d.index,
            name: d.name.clone(),
            used: d.used,
            total: d.total,
        })
        .collect();
    let refresh = meta.refresh;
    let payload = serde_json::to_string(&Payload {
        verdicts,
        charts,
        gpus,
        meta,
    })
    .unwrap();

    HTML_TEMPLATE
        .replace("__REFRESH__", &refresh.to_string())
        .replace("__PAYLOAD__", &payload)
}

// ---- main loop -------------------------------------------------------------

fn find_root(sys: &System, args: &Args) -> Pid {
    if let Some(pid) = args.pid {
        let p = Pid::from_u32(pid);
        if sys.process(p).is_some() {
            return p;
        }
        eprintln!("leakwatch: no process with pid {pid}");
        std::process::exit(1);
    }
    if let Some(m) = &args.r#match {
        let me = std::process::id();
        for (pid, proc) in sys.processes() {
            if pid.as_u32() == me {
                continue;
            }
            let cmd = proc.cmd().join(" ");
            if cmd.contains(m.as_str()) && !cmd.contains("leakwatch") {
                return *pid;
            }
        }
        eprintln!("leakwatch: no process matching {m:?}");
        std::process::exit(1);
    }
    eprintln!("leakwatch: pass --pid or --match");
    std::process::exit(1);
}

fn log_line(path: &PathBuf, msg: &str) {
    let line = format!("{}  {msg}", Utc::now().to_rfc3339());
    println!("{line}");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn main() {
    let args = Args::parse();
    fs::create_dir_all(&args.outdir).expect("mkdir outdir");
    let outdir = PathBuf::from(&args.outdir);
    let db_path = outdir.join("leakwatch.db");
    let html_path = outdir.join("dashboard.html");
    let log_path = outdir.join("leakwatch.log");

    let store = Store::new(&db_path);
    let gpu = GpuProbe::new();

    let refresh_kind =
        RefreshKind::new().with_processes(ProcessRefreshKind::new().with_memory());
    let mut sys = System::new_with_specifics(refresh_kind);
    sys.refresh_processes();

    let root = find_root(&sys, &args);
    let root_label = label_for(&sys, root);

    let window = args.window_min as f64 * 60.0;
    let win_label = if args.window_min < 120 {
        format!("{}m", args.window_min)
    } else {
        format!("{}h", args.window_min / 60)
    };

    log_line(
        &log_path,
        &format!(
            "leakwatch start: root pid={} ({}) interval={}s window={} gpus={}",
            root.as_u32(),
            root_label,
            args.interval,
            win_label,
            if gpu.ok() {
                format!("yes({})", gpu.count())
            } else {
                "none".to_string()
            }
        ),
    );

    let running = Arc::new(AtomicBool::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
            .expect("set ctrl-c handler");
    }

    let mut flagged: std::collections::HashSet<String> = Default::default();
    let mut next_prune = now_secs() + 3600.0;

    while running.load(Ordering::SeqCst) {
        let ts = now_secs();
        sys.refresh_processes();

        if sys.process(root).is_none() {
            log_line(&log_path, &format!("root pid {} exited — stopping", root.as_u32()));
            break;
        }

        let tree = collect_tree(&sys, root);
        let (vram_map, devices) = gpu.sample();

        let mut rows = Vec::new();
        for pid in &tree {
            if let Some(proc) = sys.process(*pid) {
                let label = label_for(&sys, *pid);
                rows.push(Row {
                    pid: pid.as_u32(),
                    label,
                    rss: proc.memory(), // bytes
                    vram: *vram_map.get(&pid.as_u32()).unwrap_or(&0),
                });
            }
        }

        store.write_samples(ts, &rows);
        store.write_gpus(ts, &devices);

        let series = store.series_by_label(ts - window);
        let verdicts = assess(&series, window, args.min_slope_mb_hr);

        for v in &verdicts {
            if v.leak && !flagged.contains(&v.label) {
                flagged.insert(v.label.clone());
                log_line(
                    &log_path,
                    &format!(
                        "LEAK FLAGGED  {}  floor RSS +{}/hr, VRAM +{}/hr  (now RSS {}, VRAM {})",
                        v.label,
                        human(v.rss_floor),
                        human(v.vram_floor),
                        human(v.rss_last),
                        human(v.vram_last)
                    ),
                );
            }
            if !v.leak {
                flagged.remove(&v.label);
            }
        }

        let meta = Meta {
            root_pid: root.as_u32(),
            root_label: root_label.clone(),
            tree_count: rows.len(),
            window_label: win_label.clone(),
            refresh: args.interval.max(10),
            updated: Local::now().format("%H:%M:%S").to_string(),
        };
        let html = render_html(verdicts, &series, &devices, meta, window);
        let tmp = html_path.with_extension("html.tmp");
        fs::write(&tmp, html).ok();
        fs::rename(&tmp, &html_path).ok(); // atomic swap

        if ts >= next_prune {
            store.prune(ts - args.retain_hours * 3600.0);
            next_prune = ts + 3600.0;
        }

        // sleep in small steps so Ctrl-C is responsive
        let wake = std::time::Instant::now() + Duration::from_secs(args.interval);
        while std::time::Instant::now() < wake && running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    log_line(&log_path, "stopped");
}

const HTML_TEMPLATE: &str = include_str!("dashboard.html");