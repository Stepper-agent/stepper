use globset::GlobBuilder;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Resolve a rule path pattern to an absolute glob string in *canonical* space
/// (so it lines up with canonicalized request paths). Anchors: `//abs` →
/// absolute, `~/` → home, `/rel` or bare `rel` → under project root. The
/// glob-free prefix is symlink-resolved; the glob tail is appended verbatim.
pub fn resolve_pattern(pattern: &str, project_root: &Path, home: Option<&Path>) -> Option<String> {
    let abs = if let Some(rest) = pattern.strip_prefix("//") {
        PathBuf::from("/").join(rest)
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        home?.join(rest)
    } else if pattern == "~" {
        home?.to_path_buf()
    } else if let Some(rest) = pattern.strip_prefix('/') {
        project_root.join(rest)
    } else {
        project_root.join(pattern)
    };
    Some(canonical_glob(&abs).to_string_lossy().into_owned())
}

/// Make a request path absolute by anchoring a relative path at `cwd` (the
/// command's effective working directory), so a bash redirect like `> out.txt`
/// run from a subdirectory is judged under that subdirectory, not project_root.
/// Absolute paths are returned unchanged. Downstream matching still anchors
/// *rule* patterns at project_root.
pub fn anchor_at_cwd(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Resolve a request path to an absolute, symlink-free path so a symlink that
/// points outside the project is judged by its *real* location.
pub fn resolve_request_path(path: &Path, project_root: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    canonicalize_lenient(&abs)
}

pub fn path_matches(
    pattern: &str,
    request_path: &Path,
    project_root: &Path,
    home: Option<&Path>,
) -> bool {
    let Some(glob_str) = resolve_pattern(pattern, project_root, home) else {
        return false;
    };
    // `literal_separator` keeps `*` within one path segment (gitignore
    // semantics), so `Read(/src/*)` grants direct children only; `**` stays
    // recursive.
    let Ok(matcher) = GlobBuilder::new(&glob_str)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
    else {
        return false;
    };
    matcher.is_match(resolve_request_path(request_path, project_root))
}

/// Whether the request path's real location is inside the project root.
pub fn is_in_project(request_path: &Path, project_root: &Path) -> bool {
    let target = resolve_request_path(request_path, project_root);
    target.starts_with(canonicalize_lenient(project_root))
}

/// Whether the path is inside the project's `.stepper/` config dir — a protected
/// area (commands, hooks, settings) that controls the agent's own security and so
/// must never be auto-approved for writes/edits, even in `accept-edits` mode.
pub fn is_protected(request_path: &Path, project_root: &Path) -> bool {
    let target = resolve_request_path(request_path, project_root);
    let stepper = canonicalize_lenient(&project_root.join(".stepper"));
    target.starts_with(&stepper)
}

/// Canonicalize as much of `abs` as exists on disk, appending any non-existent
/// trailing components — so new files under a symlinked dir still resolve into
/// the same canonical space as existing ones.
fn canonicalize_lenient(abs: &Path) -> PathBuf {
    let mut suffix: Vec<OsString> = Vec::new();
    let mut cur = abs;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cur) {
            let mut result = canonical;
            for name in suffix.iter().rev() {
                result.push(name);
            }
            return result;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                suffix.push(name.to_os_string());
                cur = parent;
            }
            _ => return lexical_normalize(abs),
        }
    }
}

/// Build a glob string whose glob-free prefix is canonicalized and whose glob
/// tail (anything from the first `* ? [` component onward) is kept literal.
fn canonical_glob(abs: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut tail: Vec<OsString> = Vec::new();
    let mut in_tail = false;
    for component in abs.components() {
        let is_glob = component
            .as_os_str()
            .to_string_lossy()
            .contains(['*', '?', '[']);
        if in_tail || is_glob {
            in_tail = true;
            tail.push(component.as_os_str().to_os_string());
        } else {
            prefix.push(component);
        }
    }
    let mut result = canonicalize_lenient(&prefix);
    for name in tail {
        result.push(name);
    }
    result
}

fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in p.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
