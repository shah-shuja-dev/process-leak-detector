use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Collect every .py file reachable under `root` (file or directory).
/// This is how we cover "the complete tree of the script": rather than
/// trying to perfectly resolve dynamic import/subprocess targets, we treat
/// the whole project directory as in-scope, since that's where spawned
/// helper scripts / modules realistically live.
pub fn collect_py_files(root: &Path) -> Result<Vec<PathBuf>> {
    let base = if root.is_file() {
        root.parent().unwrap_or_else(|| Path::new(".")).to_path_buf()
    } else {
        root.to_path_buf()
    };

    let mut files = Vec::new();
    for entry in WalkDir::new(&base)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.file_type().is_dir()
                && (name == ".venv"
                    || name == "venv"
                    || name == "__pycache__"
                    || name == ".git"
                    || name == "node_modules"))
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            if let Some(ext) = entry.path().extension() {
                if ext == "py" {
                    files.push(entry.path().to_path_buf());
                }
            }
        }
    }

    if root.is_file() && !files.contains(&root.to_path_buf()) {
        files.push(root.to_path_buf());
    }

    files.sort();
    files.dedup();
    Ok(files)
}

#[derive(Debug, Clone)]
pub struct SpawnPoint {
    pub line: usize, // 0-indexed
    pub kind: SpawnKind,
    pub raw: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    Subprocess,
    OsSystem,
    Multiprocessing,
    Import,
}

static SUBPROCESS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"subprocess\.(run|Popen|call|check_call|check_output)\s*\(").unwrap());
static OS_SYSTEM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"os\.system\s*\(").unwrap());
static MULTIPROC_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"multiprocessing\.Process\s*\(|^\s*Process\s*\(").unwrap());
static IMPORT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*(import\s+[\w\.]+|from\s+[\w\.]+\s+import\s+.+)").unwrap());

/// Detect points in a file where it hands off execution to other code:
/// subprocess calls, os.system, multiprocessing, and plain imports. These
/// are reported so the user can see what the tool considered part of the
/// "tree", even though actual scanning is done by walking the directory.
pub fn detect_spawns(lines: &[&str]) -> Vec<SpawnPoint> {
    let mut spawns = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if SUBPROCESS_RE.is_match(line) {
            spawns.push(SpawnPoint {
                line: i,
                kind: SpawnKind::Subprocess,
                raw: trimmed.to_string(),
            });
        } else if OS_SYSTEM_RE.is_match(line) {
            spawns.push(SpawnPoint {
                line: i,
                kind: SpawnKind::OsSystem,
                raw: trimmed.to_string(),
            });
        } else if MULTIPROC_RE.is_match(line) {
            spawns.push(SpawnPoint {
                line: i,
                kind: SpawnKind::Multiprocessing,
                raw: trimmed.to_string(),
            });
        } else if IMPORT_RE.is_match(line) {
            spawns.push(SpawnPoint {
                line: i,
                kind: SpawnKind::Import,
                raw: trimmed.to_string(),
            });
        }
    }
    spawns
}
