//! LSP diagnostics and their human/agent-facing report (mirrors opencode's
//! `lsp/diagnostic.ts`).

use serde::Deserialize;

/// A single LSP diagnostic, deserialized from `textDocument/publishDiagnostics`.
/// Only the fields stepper renders are kept; the rest are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default)]
    pub severity: Option<u8>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Range {
    pub start: Position,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

fn severity_label(severity: Option<u8>) -> &'static str {
    match severity.unwrap_or(1) {
        1 => "ERROR",
        2 => "WARN",
        3 => "INFO",
        _ => "HINT",
    }
}

/// `ERROR [line:col] message` (1-based line/col, matching opencode).
pub fn pretty(d: &Diagnostic) -> String {
    format!(
        "{} [{}:{}] {}",
        severity_label(d.severity),
        d.range.start.line + 1,
        d.range.start.character + 1,
        d.message
    )
}

const MAX_PER_FILE: usize = 20;

/// Render the *error* diagnostics for a file as an XML-tagged block (empty string
/// when there are no errors — warnings/info/hints are not surfaced, matching
/// opencode). Capped at [`MAX_PER_FILE`] with an "... and N more" suffix.
pub fn report(file: &str, issues: &[Diagnostic]) -> String {
    let errors: Vec<&Diagnostic> = issues.iter().filter(|d| d.severity == Some(1)).collect();
    if errors.is_empty() {
        return String::new();
    }
    let shown = errors.len().min(MAX_PER_FILE);
    let body = errors[..shown]
        .iter()
        .map(|d| pretty(d))
        .collect::<Vec<_>>()
        .join("\n");
    let suffix = if errors.len() > MAX_PER_FILE {
        format!("\n... and {} more", errors.len() - MAX_PER_FILE)
    } else {
        String::new()
    };
    format!("<diagnostics file=\"{file}\">\n{body}{suffix}\n</diagnostics>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: u8, line: u32, col: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: col,
                },
            },
            severity: Some(severity),
            message: msg.into(),
            source: None,
        }
    }

    #[test]
    fn pretty_is_one_based_with_severity_label() {
        assert_eq!(pretty(&diag(1, 0, 0, "boom")), "ERROR [1:1] boom");
        assert_eq!(pretty(&diag(2, 4, 2, "careful")), "WARN [5:3] careful");
    }

    #[test]
    fn report_only_includes_errors() {
        let issues = vec![diag(1, 0, 0, "err"), diag(2, 1, 0, "warn")];
        let r = report("a.rs", &issues);
        assert!(r.contains("ERROR [1:1] err"));
        assert!(!r.contains("warn"), "warnings are not reported: {r}");
        assert!(r.starts_with("<diagnostics file=\"a.rs\">"));
    }

    #[test]
    fn report_is_empty_without_errors() {
        assert_eq!(report("a.rs", &[diag(2, 0, 0, "just a warning")]), "");
        assert_eq!(report("a.rs", &[]), "");
    }

    #[test]
    fn report_caps_at_twenty_with_a_more_suffix() {
        let issues: Vec<Diagnostic> = (0..25).map(|i| diag(1, i, 0, "e")).collect();
        let r = report("a.rs", &issues);
        assert!(r.contains("... and 5 more"), "got: {r}");
        assert_eq!(r.matches("ERROR").count(), 20);
    }
}
