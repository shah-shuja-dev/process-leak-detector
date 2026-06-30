use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone)]
pub struct FuncSpan {
    pub name: String,
    pub indent: usize,
    pub start: usize, // 0-indexed line of `def`
    pub end: usize,   // exclusive
}

static DEF_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\s*)def\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap());
static CLASS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\s*)class\s+([A-Za-z_][A-Za-z0-9_]*)\s*[\(:]").unwrap());

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

/// Find all function definitions in a file's lines and compute their body span
/// using simple indentation tracking (good enough for heuristic analysis,
/// not a full parser).
pub fn parse_functions(lines: &[&str]) -> Vec<FuncSpan> {
    let mut spans = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = DEF_RE.captures(line) {
            let indent = caps[1].len();
            let name = caps[2].to_string();
            let mut end = lines.len();
            for (j, later) in lines.iter().enumerate().skip(i + 1) {
                if is_blank_or_comment(later) {
                    continue;
                }
                if indent_of(later) <= indent {
                    end = j;
                    break;
                }
            }
            spans.push(FuncSpan {
                name,
                indent,
                start: i,
                end,
            });
        }
    }
    spans
}

#[derive(Debug, Clone)]
pub struct ClassSpan {
    pub name: String,
    pub indent: usize,
    pub start: usize,
    pub end: usize,
}

pub fn parse_classes(lines: &[&str]) -> Vec<ClassSpan> {
    let mut spans = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = CLASS_RE.captures(line) {
            let indent = caps[1].len();
            let name = caps[2].to_string();
            let mut end = lines.len();
            for (j, later) in lines.iter().enumerate().skip(i + 1) {
                if is_blank_or_comment(later) {
                    continue;
                }
                if indent_of(later) <= indent {
                    end = j;
                    break;
                }
            }
            spans.push(ClassSpan {
                name,
                indent,
                start: i,
                end,
            });
        }
    }
    spans
}

/// Given a line index, return the innermost enclosing function name, if any.
pub fn enclosing_function(spans: &[FuncSpan], line: usize) -> Option<String> {
    spans
        .iter()
        .filter(|s| s.start < line && line < s.end)
        .max_by_key(|s| s.indent) // innermost = deepest indent
        .map(|s| s.name.clone())
}

pub fn enclosing_class(spans: &[ClassSpan], line: usize) -> Option<String> {
    spans
        .iter()
        .filter(|s| s.start < line && line < s.end)
        .max_by_key(|s| s.indent)
        .map(|s| s.name.clone())
}
