use crate::scanner::{enclosing_class, enclosing_function, parse_classes, parse_functions};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Low => write!(f, "LOW"),
            Severity::Medium => write!(f, "MED"),
            Severity::High => write!(f, "HIGH"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub file: PathBuf,
    pub function: Option<String>,
    pub line: usize, // 1-indexed for display
    pub severity: Severity,
    pub category: &'static str,
    pub message: String,
}

static MODULE_CONTAINER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^([A-Za-z_]\w*)\s*=\s*(\[\]|\{\}|set\(\))\s*$").unwrap());
static OPEN_ASSIGN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(\w+)\s*=\s*open\s*\(").unwrap());
static WITH_OPEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*with\s+open\s*\(").unwrap());
static LRU_CACHE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*@(functools\.)?lru_cache(\s*\(\s*\))?\s*$").unwrap());
static FUNC_CACHE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*@(functools\.)?cache\s*$").unwrap());
static LRU_CACHE_MAXSIZE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^\s*@(functools\.)?lru_cache\s*\(.*maxsize\s*=").unwrap());
static THREAD_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(\w+)\s*=\s*(threading\.)?(Thread|Timer)\s*\(").unwrap()
});
static DAEMON_TRUE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"daemon\s*=\s*True").unwrap());
static DEF_DEL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*def\s+__del__\s*\(").unwrap());
static LISTENER_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(listener|callback|handler|subscriber|observer)").unwrap());

pub fn analyze_file(path: &Path, content: &str) -> Vec<Finding> {
    let lines: Vec<&str> = content.lines().collect();
    let funcs = parse_functions(&lines);
    let classes = parse_classes(&lines);
    let mut findings = Vec::new();

    check_unbounded_module_containers(path, &lines, &funcs, &mut findings);
    check_unbounded_caches(path, &lines, &mut findings);
    check_unclosed_files(path, &lines, &funcs, &mut findings);
    check_threads_without_join(path, &lines, &funcs, &mut findings);
    check_listener_registration_without_removal(path, &lines, &funcs, &mut findings);
    check_del_with_cycles(path, &lines, &classes, &mut findings);

    findings.sort_by_key(|f| f.line);
    findings
}

/// 1. Module-level list/dict/set that functions keep appending to, with no
///    clear()/pop()/reassignment anywhere in the file -> classic unbounded
///    cache / "leaky global" pattern.
fn check_unbounded_module_containers(
    path: &Path,
    lines: &[&str],
    funcs: &[crate::scanner::FuncSpan],
    findings: &mut Vec<Finding>,
) {
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue; // module level only
        }
        let Some(caps) = MODULE_CONTAINER_RE.captures(line) else {
            continue;
        };
        let name = &caps[1];

        let append_re = Regex::new(&format!(
            r"\b{name}\s*\.\s*(append|add|update|setdefault)\s*\(|\b{name}\s*\[[^\]]*\]\s*="
        ))
        .unwrap();
        let clear_re = Regex::new(&format!(
            r"\b{name}\s*\.\s*(clear|pop|popitem|remove|discard)\s*\(|^\s*{name}\s*=\s*(\[\]|\{{\}}|set\(\))\s*$"
        ))
        .unwrap();

        let mut growth_sites: Vec<usize> = Vec::new();
        let mut has_shrink = false;
        for (j, l) in lines.iter().enumerate() {
            if j == i {
                continue;
            }
            if append_re.is_match(l) {
                growth_sites.push(j);
            }
            if clear_re.is_match(l) {
                has_shrink = true;
            }
        }

        if growth_sites.is_empty() || has_shrink {
            continue;
        }

        // group by enclosing function so the report names the culprit function(s)
        let mut by_func: HashMap<Option<String>, usize> = HashMap::new();
        for site in &growth_sites {
            *by_func.entry(enclosing_function(funcs, *site)).or_insert(0) += 1;
        }

        for (func, count) in by_func {
            findings.push(Finding {
                file: path.to_path_buf(),
                function: func,
                line: i + 1,
                severity: Severity::High,
                category: "unbounded-global-container",
                message: format!(
                    "module-level container '{name}' is grown ({count} site(s)) but never cleared, popped, or reassigned anywhere in this file — it will grow for the life of the process"
                ),
            });
        }
    }
}

/// 2. functools.lru_cache / functools.cache with no maxsize -> grows forever.
fn check_unbounded_caches(path: &Path, lines: &[&str], findings: &mut Vec<Finding>) {
    for (i, line) in lines.iter().enumerate() {
        let is_bare_lru = LRU_CACHE_RE.is_match(line) && !LRU_CACHE_MAXSIZE_RE.is_match(line);
        let is_cache = FUNC_CACHE_RE.is_match(line);
        if !is_bare_lru && !is_cache {
            continue;
        }
        // find the function this decorator applies to (next def line below)
        let mut func_name = None;
        for later in lines.iter().skip(i + 1) {
            let t = later.trim_start();
            if t.starts_with("def ") {
                func_name = t
                    .strip_prefix("def ")
                    .and_then(|s| s.split('(').next())
                    .map(|s| s.trim().to_string());
                break;
            }
            if !t.starts_with('@') {
                break;
            }
        }
        let decorator = if is_cache { "@cache" } else { "@lru_cache" };
        findings.push(Finding {
            file: path.to_path_buf(),
            function: func_name,
            line: i + 1,
            severity: Severity::Medium,
            category: "unbounded-cache",
            message: format!(
                "{decorator} has no maxsize, so cached results accumulate for every distinct argument forever; consider maxsize=N or a TTL-based cache"
            ),
        });
    }
}

/// 3. `x = open(...)` outside a `with` block, with no matching `x.close()`
///    anywhere in the same function.
fn check_unclosed_files(
    path: &Path,
    lines: &[&str],
    funcs: &[crate::scanner::FuncSpan],
    findings: &mut Vec<Finding>,
) {
    for (i, line) in lines.iter().enumerate() {
        if WITH_OPEN_RE.is_match(line) {
            continue; // safe pattern
        }
        let Some(caps) = OPEN_ASSIGN_RE.captures(line) else {
            continue;
        };
        let var = caps[1].to_string();
        let func = enclosing_function(funcs, i);

        let (scan_start, scan_end) = match funcs.iter().find(|f| {
            f.start < i && i < f.end && Some(f.name.clone()) == func
        }) {
            Some(span) => (span.start, span.end),
            None => (0, lines.len()), // module-level open()
        };

        let close_re = Regex::new(&format!(r"\b{var}\s*\.\s*close\s*\(")).unwrap();
        let closed = lines[scan_start..scan_end].iter().any(|l| close_re.is_match(l));

        if !closed {
            findings.push(Finding {
                file: path.to_path_buf(),
                function: func,
                line: i + 1,
                severity: Severity::Medium,
                category: "unclosed-file-handle",
                message: format!(
                    "'{var}' is assigned from open() outside a `with` block and never explicitly closed in this scope — prefer `with open(...) as {var}:`"
                ),
            });
        }
    }
}

/// 4. threading.Thread / threading.Timer created without .join() and without
///    daemon=True — can pile up live thread objects that never get reclaimed.
fn check_threads_without_join(
    path: &Path,
    lines: &[&str],
    funcs: &[crate::scanner::FuncSpan],
    findings: &mut Vec<Finding>,
) {
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = THREAD_RE.captures(line) else {
            continue;
        };
        let var = caps[1].to_string();
        let kind = caps[3].to_string();
        if DAEMON_TRUE_RE.is_match(line) {
            continue;
        }
        let func = enclosing_function(funcs, i);
        let (scan_start, scan_end) = match funcs
            .iter()
            .find(|f| f.start < i && i < f.end && Some(f.name.clone()) == func)
        {
            Some(span) => (span.start, span.end),
            None => (0, lines.len()),
        };
        let join_re = Regex::new(&format!(r"\b{var}\s*\.\s*join\s*\(")).unwrap();
        let joined = lines[scan_start..scan_end]
            .iter()
            .any(|l| join_re.is_match(l));
        if !joined {
            findings.push(Finding {
                file: path.to_path_buf(),
                function: func,
                line: i + 1,
                severity: Severity::Low,
                category: "dangling-thread",
                message: format!(
                    "{kind} '{var}' is created without daemon=True and never joined in this scope — if these accumulate across calls, the thread objects (and their frames) won't be collected"
                ),
            });
        }
    }
}

/// 5. A list/dict named like a listener/callback registry is appended to
///    somewhere, but nothing in the file ever calls .remove()/.pop() on it —
///    classic "subscribed but never unsubscribed" leak.
fn check_listener_registration_without_removal(
    path: &Path,
    lines: &[&str],
    funcs: &[crate::scanner::FuncSpan],
    findings: &mut Vec<Finding>,
) {
    let append_re = Regex::new(r"(\w+)\s*\.\s*append\s*\(").unwrap();
    let mut candidates: HashSet<String> = HashSet::new();
    for line in lines.iter() {
        if let Some(caps) = append_re.captures(line) {
            let name = &caps[1];
            if LISTENER_NAME_RE.is_match(name) {
                candidates.insert(name.to_string());
            }
        }
    }

    for name in candidates {
        let append_var_re = Regex::new(&format!(r"\b{name}\s*\.\s*append\s*\(")).unwrap();
        let remove_var_re =
            Regex::new(&format!(r"\b{name}\s*\.\s*(remove|pop|discard|clear)\s*\(")).unwrap();
        let has_removal = lines.iter().any(|l| remove_var_re.is_match(l));
        if has_removal {
            continue;
        }
        for (i, line) in lines.iter().enumerate() {
            if append_var_re.is_match(line) {
                findings.push(Finding {
                    file: path.to_path_buf(),
                    function: enclosing_function(funcs, i),
                    line: i + 1,
                    severity: Severity::Medium,
                    category: "unremoved-listener",
                    message: format!(
                        "'{name}' looks like a listener/callback registry — entries are appended here but nothing in this file ever removes from '{name}', so registered callbacks (and whatever they close over) accumulate"
                    ),
                });
            }
        }
    }
}

/// 6. Classes defining __del__ — informational. Pre-3.4 CPython couldn't
///    collect reference cycles containing a __del__ at all; modern CPython
///    can, but it's still a common source of subtle leaks/finalization bugs
///    when combined with cycles, so worth flagging as low severity.
fn check_del_with_cycles(
    path: &Path,
    lines: &[&str],
    classes: &[crate::scanner::ClassSpan],
    findings: &mut Vec<Finding>,
) {
    for (i, line) in lines.iter().enumerate() {
        if !DEF_DEL_RE.is_match(line) {
            continue;
        }
        let class = enclosing_class(classes, i);
        findings.push(Finding {
            file: path.to_path_buf(),
            function: class.map(|c| format!("{c}.__del__")),
            line: i + 1,
            severity: Severity::Low,
            category: "del-with-possible-cycle",
            message: "class defines __del__; if instances ever participate in a reference cycle (e.g. parent/child back-references), finalization can be delayed or skipped entirely — prefer weakref for back-references".to_string(),
        });
    }
}
