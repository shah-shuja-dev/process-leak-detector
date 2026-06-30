use crate::analyzer::{Finding, Severity};
use crate::discovery::{SpawnKind, SpawnPoint};
use colored::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn print_text_report(
    findings: &[Finding],
    spawns: &BTreeMap<PathBuf, Vec<SpawnPoint>>,
    files_scanned: usize,
    root: &Path,
) {
    println!(
        "\n{}",
        format!("pyleak — static memory-leak report for {}", root.display())
            .bold()
            .underline()
    );
    println!("{} python file(s) scanned\n", files_scanned);

    let spawn_count: usize = spawns.values().map(|v| v.len()).sum();
    if spawn_count > 0 {
        println!("{}", "Execution tree discovered:".bold());
        for (file, points) in spawns {
            for p in points {
                let tag = match p.kind {
                    SpawnKind::Subprocess => "subprocess".cyan(),
                    SpawnKind::OsSystem => "os.system".cyan(),
                    SpawnKind::Multiprocessing => "multiprocessing".cyan(),
                    SpawnKind::Import => "import".dimmed(),
                };
                println!(
                    "  {}:{}  [{}]  {}",
                    file.display(),
                    p.line + 1,
                    tag,
                    p.raw
                );
            }
        }
        println!();
    }

    if findings.is_empty() {
        println!("{}", "No suspicious patterns found. 🎉".green().bold());
        println!(
            "{}",
            "(this is a static heuristic scan, not a guarantee — see notes below)".dimmed()
        );
        return;
    }

    println!(
        "{}",
        format!("{} potential issue(s) found:\n", findings.len())
            .bold()
            .red()
    );

    let mut by_file: BTreeMap<&PathBuf, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        by_file.entry(&f.file).or_default().push(f);
    }

    for (file, items) in by_file {
        println!("{}", file.display().to_string().bold().yellow());
        for f in items {
            let sev = match f.severity {
                Severity::High => f.severity.to_string().red().bold(),
                Severity::Medium => f.severity.to_string().yellow().bold(),
                Severity::Low => f.severity.to_string().normal(),
            };
            let func_label = f
                .function
                .clone()
                .map(|n| format!("fn {n}()"))
                .unwrap_or_else(|| "module level".to_string());
            println!(
                "  [{}] line {:<5} {:<28} {}",
                sev,
                f.line,
                func_label.magenta(),
                f.category
            );
            println!("        {}", f.message);
        }
        println!();
    }

    println!("{}", "Summary by function:".bold());
    let mut by_func: BTreeMap<String, usize> = BTreeMap::new();
    for f in findings {
        let key = format!(
            "{} :: {}",
            f.file.display(),
            f.function.clone().unwrap_or_else(|| "<module>".to_string())
        );
        *by_func.entry(key).or_insert(0) += 1;
    }
    for (k, count) in by_func {
        println!("  - {k}  ({count} issue(s))");
    }

    println!(
        "\n{}",
        "Note: this is static pattern-matching, not execution — it flags suspects for review, not proven leaks."
            .dimmed()
    );
}

#[derive(serde::Serialize)]
struct JsonReport<'a> {
    root: String,
    files_scanned: usize,
    findings: &'a [Finding],
}

pub fn print_json_report(findings: &[Finding], files_scanned: usize, root: &Path) {
    let report = JsonReport {
        root: root.display().to_string(),
        files_scanned,
        findings,
    };
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

/// Build a timestamped, target-named filename so repeated runs land as
/// separate files on the Desktop instead of clobbering each other, e.g.
/// "pyleak_report_myscript_20260630_141533.html".
pub fn report_filename(target: &Path) -> String {
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let stem: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    format!("pyleak_report_{stem}_{timestamp}.html")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn severity_class(s: &Severity) -> &'static str {
    match s {
        Severity::High => "sev-high",
        Severity::Medium => "sev-med",
        Severity::Low => "sev-low",
    }
}

fn spawn_tag_class(kind: &SpawnKind) -> &'static str {
    match kind {
        SpawnKind::Subprocess | SpawnKind::OsSystem | SpawnKind::Multiprocessing => "tag-spawn",
        SpawnKind::Import => "tag-import",
    }
}

fn spawn_tag_label(kind: &SpawnKind) -> &'static str {
    match kind {
        SpawnKind::Subprocess => "subprocess",
        SpawnKind::OsSystem => "os.system",
        SpawnKind::Multiprocessing => "multiprocessing",
        SpawnKind::Import => "import",
    }
}

/// Render a single self-contained HTML file (inline CSS, no external
/// assets, no JS dependency) summarizing the scan.
pub fn write_html_report(
    findings: &[Finding],
    spawns: &BTreeMap<PathBuf, Vec<SpawnPoint>>,
    files_scanned: usize,
    root: &Path,
    out_path: &Path,
) -> std::io::Result<()> {
    let high = findings.iter().filter(|f| f.severity == Severity::High).count();
    let med = findings.iter().filter(|f| f.severity == Severity::Medium).count();
    let low = findings.iter().filter(|f| f.severity == Severity::Low).count();

    let mut html = String::new();
    html.push_str("<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\">");
    html.push_str("<title>pyleak report</title><style>");
    html.push_str(CSS);
    html.push_str("</style></head><body>");

    html.push_str(&format!(
        "<header><h1>pyleak</h1><p class=\"subtitle\">static memory-leak report for <code>{}</code></p></header>",
        esc(&root.display().to_string())
    ));

    html.push_str("<section class=\"stats\">");
    html.push_str(&format!(
        "<div class=\"stat\"><span class=\"stat-num\">{files_scanned}</span><span class=\"stat-label\">files scanned</span></div>"
    ));
    html.push_str(&format!(
        "<div class=\"stat\"><span class=\"stat-num\">{}</span><span class=\"stat-label\">total issues</span></div>",
        findings.len()
    ));
    html.push_str(&format!(
        "<div class=\"stat sev-high-bg\"><span class=\"stat-num\">{high}</span><span class=\"stat-label\">high</span></div>"
    ));
    html.push_str(&format!(
        "<div class=\"stat sev-med-bg\"><span class=\"stat-num\">{med}</span><span class=\"stat-label\">medium</span></div>"
    ));
    html.push_str(&format!(
        "<div class=\"stat sev-low-bg\"><span class=\"stat-num\">{low}</span><span class=\"stat-label\">low</span></div>"
    ));
    html.push_str("</section>");

    let spawn_count: usize = spawns.values().map(|v| v.len()).sum();
    if spawn_count > 0 {
        html.push_str("<section><h2>Execution tree discovered</h2><table class=\"spawns\">");
        html.push_str("<tr><th>File</th><th>Line</th><th>Kind</th><th>Code</th></tr>");
        for (file, points) in spawns {
            for p in points {
                html.push_str(&format!(
                    "<tr><td class=\"mono\">{}</td><td>{}</td><td><span class=\"tag {}\">{}</span></td><td class=\"mono\">{}</td></tr>",
                    esc(&file.display().to_string()),
                    p.line + 1,
                    spawn_tag_class(&p.kind),
                    spawn_tag_label(&p.kind),
                    esc(&p.raw)
                ));
            }
        }
        html.push_str("</table></section>");
    }

    html.push_str("<section><h2>Findings</h2>");
    if findings.is_empty() {
        html.push_str("<p class=\"empty\">No suspicious patterns found. 🎉</p>");
    } else {
        let mut by_file: BTreeMap<&PathBuf, Vec<&Finding>> = BTreeMap::new();
        for f in findings {
            by_file.entry(&f.file).or_default().push(f);
        }
        for (file, items) in by_file {
            html.push_str(&format!(
                "<h3 class=\"filename mono\">{}</h3><div class=\"findings\">",
                esc(&file.display().to_string())
            ));
            for f in items {
                let func_label = f
                    .function
                    .clone()
                    .map(|n| format!("fn {n}()"))
                    .unwrap_or_else(|| "module level".to_string());
                html.push_str(&format!(
                    "<div class=\"finding {}\">\
                        <div class=\"finding-head\">\
                            <span class=\"badge {}\">{}</span>\
                            <span class=\"loc\">line {}</span>\
                            <span class=\"func mono\">{}</span>\
                            <span class=\"category\">{}</span>\
                        </div>\
                        <p class=\"message\">{}</p>\
                    </div>",
                    severity_class(&f.severity),
                    severity_class(&f.severity),
                    f.severity,
                    f.line,
                    esc(&func_label),
                    esc(f.category),
                    esc(&f.message)
                ));
            }
            html.push_str("</div>");
        }
    }
    html.push_str("</section>");

    if !findings.is_empty() {
        let mut by_func: BTreeMap<String, usize> = BTreeMap::new();
        for f in findings {
            let key = format!(
                "{} :: {}",
                f.file.display(),
                f.function.clone().unwrap_or_else(|| "<module>".to_string())
            );
            *by_func.entry(key).or_insert(0) += 1;
        }
        html.push_str("<section><h2>Summary by function</h2><table class=\"summary\">");
        html.push_str("<tr><th>File :: Function</th><th>Issues</th></tr>");
        for (k, count) in by_func {
            html.push_str(&format!(
                "<tr><td class=\"mono\">{}</td><td>{}</td></tr>",
                esc(&k),
                count
            ));
        }
        html.push_str("</table></section>");
    }

    html.push_str(
        "<footer><p>Static pattern-matching, not execution — findings are suspects for review, not proven leaks.</p></footer>",
    );
    html.push_str("</body></html>");

    std::fs::write(out_path, html)
}

const CSS: &str = r#"
:root {
    --bg: #0f1115; --panel: #161922; --border: #262b38;
    --text: #e6e8ee; --muted: #8a90a3;
    --high: #ef4444; --med: #f59e0b; --low: #64748b;
    --accent: #60a5fa;
}
* { box-sizing: border-box; }
body {
    background: var(--bg); color: var(--text);
    font-family: -apple-system, Segoe UI, Roboto, Helvetica, Arial, sans-serif;
    margin: 0; padding: 2rem; line-height: 1.5;
}
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.9em; }
header h1 { margin: 0; font-size: 2rem; letter-spacing: -0.02em; }
header .subtitle { color: var(--muted); margin-top: 0.25rem; }
header code { color: var(--accent); }
section { margin-top: 2rem; }
h2 { font-size: 1.1rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }
h3.filename { color: var(--accent); margin-top: 1.5rem; margin-bottom: 0.5rem; font-size: 1rem; }
.stats { display: flex; gap: 1rem; flex-wrap: wrap; }
.stat { background: var(--panel); border: 1px solid var(--border); border-radius: 10px; padding: 1rem 1.5rem; min-width: 110px; display: flex; flex-direction: column; }
.stat-num { font-size: 1.8rem; font-weight: 700; }
.stat-label { color: var(--muted); font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.04em; }
.sev-high-bg { border-color: rgba(239,68,68,0.4); }
.sev-high-bg .stat-num { color: var(--high); }
.sev-med-bg { border-color: rgba(245,158,11,0.4); }
.sev-med-bg .stat-num { color: var(--med); }
.sev-low-bg { border-color: rgba(100,116,139,0.4); }
.sev-low-bg .stat-num { color: var(--low); }
table { width: 100%; border-collapse: collapse; background: var(--panel); border: 1px solid var(--border); border-radius: 10px; overflow: hidden; }
table th, table td { text-align: left; padding: 0.6rem 0.8rem; border-bottom: 1px solid var(--border); font-size: 0.9rem; }
table th { color: var(--muted); font-weight: 600; font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.04em; }
table tr:last-child td { border-bottom: none; }
.tag { padding: 0.15rem 0.5rem; border-radius: 6px; font-size: 0.78rem; }
.tag-spawn { background: rgba(96,165,250,0.15); color: var(--accent); }
.tag-import { background: rgba(138,144,163,0.15); color: var(--muted); }
.findings { display: flex; flex-direction: column; gap: 0.6rem; }
.finding { background: var(--panel); border: 1px solid var(--border); border-left-width: 4px; border-radius: 8px; padding: 0.8rem 1rem; }
.finding.sev-high { border-left-color: var(--high); }
.finding.sev-med { border-left-color: var(--med); }
.finding.sev-low { border-left-color: var(--low); }
.finding-head { display: flex; align-items: center; gap: 0.7rem; flex-wrap: wrap; }
.badge { font-size: 0.72rem; font-weight: 700; padding: 0.15rem 0.5rem; border-radius: 5px; }
.badge.sev-high { background: rgba(239,68,68,0.18); color: var(--high); }
.badge.sev-med { background: rgba(245,158,11,0.18); color: var(--med); }
.badge.sev-low { background: rgba(100,116,139,0.18); color: var(--low); }
.loc { color: var(--muted); font-size: 0.85rem; }
.func { color: var(--accent); }
.category { color: var(--muted); font-size: 0.8rem; margin-left: auto; }
.message { margin: 0.5rem 0 0; color: var(--text); font-size: 0.92rem; }
.empty { color: #4ade80; font-weight: 600; }
footer { margin-top: 2.5rem; color: var(--muted); font-size: 0.8rem; border-top: 1px solid var(--border); padding-top: 1rem; }
"#;
