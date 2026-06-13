use std::path::{Path, PathBuf};

/// The located `.stepper/` directories for a working directory.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// `<root>/.stepper` found by walking up from cwd.
    pub project_dir: Option<PathBuf>,
    /// The parent of `project_dir` — the `/` path anchor.
    pub project_root: Option<PathBuf>,
    /// `~/.stepper` if it exists.
    pub user_dir: Option<PathBuf>,
}

/// Walk up from `cwd` to find a `.stepper/` directory; the project root is its
/// parent. Also locate the user-level `~/.stepper`.
pub fn discover(cwd: &Path) -> Discovery {
    let project_dir = find_up(cwd, ".stepper");
    let project_root = project_dir
        .as_ref()
        .and_then(|d| d.parent().map(Path::to_path_buf));
    let user_dir = home_dir()
        .map(|h| h.join(".stepper"))
        .filter(|p| p.is_dir());

    Discovery {
        project_dir,
        project_root,
        user_dir,
    }
}

fn find_up(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
