//! `@import` resolution for `stepper.md` (and any base-context document): a line
//! whose only content is `@<path>` is replaced inline with that file's contents,
//! recursively. Mirrors Claude Code's `@path` import so a migrated `CLAUDE.md`
//! that references shared rule files keeps working. Each file is inlined at most
//! once (dedup + cycle guard); a missing file leaves its directive untouched.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;

/// Resolve `@import` directives in `content`. Relative paths resolve against
/// `base_dir` (the importing file's directory), `~/` against `home`.
pub fn resolve_imports(content: &str, base_dir: &Path, home: Option<&Path>) -> String {
    let mut seen = HashSet::new();
    resolve(content, base_dir, home, &mut seen, 0)
}

fn resolve(
    content: &str,
    base_dir: &Path,
    home: Option<&Path>,
    seen: &mut HashSet<PathBuf>,
    depth: usize,
) -> String {
    let mut out = String::new();
    for line in content.lines() {
        match import_target(line) {
            Some(target) if depth < MAX_DEPTH => {
                let path = resolve_path(target, base_dir, home);
                let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !seen.insert(key) {
                    // Already inlined (or a cycle) — drop the duplicate directive.
                    continue;
                }
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        let inner_dir = path.parent().unwrap_or(base_dir);
                        let inlined = resolve(&text, inner_dir, home, seen, depth + 1);
                        out.push_str(inlined.trim_end_matches('\n'));
                        out.push('\n');
                    }
                    // Missing/unreadable — keep the directive so the gap is visible.
                    Err(_) => {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// The import path if `line` is purely an `@<path>` directive (no surrounding
/// text, no whitespace in the path), else `None`.
fn import_target(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix('@')?;
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

fn resolve_path(target: &str, base_dir: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = target.strip_prefix("~/")
        && let Some(home) = home
    {
        return home.join(rest);
    }
    let path = Path::new(target);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inlines_relative_and_home_imports_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("rules")).unwrap();
        // a.md sits in rules/ and imports its sibling b.md (relative to a.md's dir).
        std::fs::write(home.join("rules/a.md"), "Rule A\n@b.md\n").unwrap();
        std::fs::write(home.join("rules/b.md"), "Rule B").unwrap();

        let out = resolve_imports("Top\n@~/rules/a.md\nBottom\n", home, Some(home));
        assert!(out.contains("Top"));
        assert!(out.contains("Rule A"));
        assert!(out.contains("Rule B"), "nested @import inlined relative to a.md: {out}");
        assert!(out.contains("Bottom"));
        assert!(!out.contains("@b.md"), "directives are replaced: {out}");
    }

    #[test]
    fn dedups_and_breaks_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        std::fs::write(base.join("x.md"), "X\n@y.md\n").unwrap();
        std::fs::write(base.join("y.md"), "Y\n@x.md\n").unwrap(); // cycle back to x
        let out = resolve_imports("@x.md\n@x.md\n", base, None);
        assert_eq!(out.matches("X").count(), 1, "x inlined once: {out}");
        assert_eq!(out.matches("Y").count(), 1, "y inlined once: {out}");
    }

    #[test]
    fn missing_import_keeps_the_directive() {
        let out = resolve_imports("a\n@/nope/missing.md\nb\n", Path::new("/tmp"), None);
        assert!(out.contains("@/nope/missing.md"), "missing import is left visible: {out}");
    }

    #[test]
    fn non_directive_at_signs_are_untouched() {
        let out = resolve_imports("email me @ a@b.com\n@path with space\n", Path::new("/tmp"), None);
        assert!(out.contains("a@b.com"));
        assert!(out.contains("@path with space"));
    }
}
