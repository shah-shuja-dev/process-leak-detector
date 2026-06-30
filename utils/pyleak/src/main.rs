mod analyzer;
mod discovery;
mod report;
mod scanner;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// pyleak — lightweight static memory-leak detector for Python projects.
///
/// By default this writes a self-contained HTML report to your Desktop.
#[derive(Parser, Debug)]
#[command(name = "pyleak", version, about)]
struct Args {
    /// Path to the entry Python script (or a directory) to check.
    /// The whole surrounding directory tree is scanned, so scripts it
    /// spawns via subprocess/multiprocessing/import are covered too.
    path: PathBuf,

    /// Directory to write the HTML report into. Created automatically if it
    /// doesn't exist. Defaults to your Desktop (cross-platform: handles
    /// Windows %USERPROFILE%\Desktop, including OneDrive-redirected
    /// Desktops, plus macOS/Linux ~/Desktop). Falls back to the current
    /// directory if a Desktop folder can't be found.
    #[arg(short = 'o', long = "output-dir", value_name = "DIR")]
    output_dir: Option<PathBuf>,

    /// Print raw JSON to stdout instead of writing an HTML report.
    #[arg(long)]
    json: bool,

    /// Also print the plain-text report to the terminal (in addition to
    /// writing the HTML report).
    #[arg(long)]
    also_print: bool,
}

fn default_output_dir() -> PathBuf {
    dirs::desktop_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn main() -> Result<()> {
    let args = Args::parse();

    if !args.path.exists() {
        anyhow::bail!("path does not exist: {}", args.path.display());
    }

    let files = discovery::collect_py_files(&args.path)
        .with_context(|| format!("failed to walk {}", args.path.display()))?;

    let mut all_findings = Vec::new();
    let mut all_spawns: BTreeMap<PathBuf, Vec<discovery::SpawnPoint>> = BTreeMap::new();

    for file in &files {
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let lines: Vec<&str> = content.lines().collect();

        let spawns = discovery::detect_spawns(&lines);
        if !spawns.is_empty() {
            all_spawns.insert(file.clone(), spawns);
        }

        let findings = analyzer::analyze_file(file, &content);
        all_findings.extend(findings);
    }

    if args.json {
        report::print_json_report(&all_findings, files.len(), &args.path);
        return Ok(());
    }

    if args.also_print {
        report::print_text_report(&all_findings, &all_spawns, files.len(), &args.path);
    }

    let out_dir = args.output_dir.unwrap_or_else(default_output_dir);
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create output directory {}", out_dir.display()))?;

    let filename = report::report_filename(&args.path);
    let out_path = out_dir.join(filename);

    report::write_html_report(&all_findings, &all_spawns, files.len(), &args.path, &out_path)
        .with_context(|| format!("failed to write {}", out_path.display()))?;

    println!(
        "{} issue(s) found across {} file(s).",
        all_findings.len(),
        files.len()
    );
    println!("HTML report written to: {}", out_path.display());

    Ok(())
}
