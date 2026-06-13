use crate::error::CoreError;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Copy-based working-tree checkpoints. A snapshot copies every non-ignored file
/// (gitignore-aware, so `target/`, `node_modules/` etc. are skipped) into
/// `.stepper/checkpoints/<id>/`; `/rewind` restores them. This is the
/// git-free fallback (a gix-backed snapshotter can replace it later without
/// changing callers).
pub struct Snapshotter {
    project_root: PathBuf,
    store: PathBuf,
}

impl Snapshotter {
    pub fn new(project_root: PathBuf) -> Self {
        let store = project_root.join(".stepper").join("checkpoints");
        Snapshotter {
            project_root,
            store,
        }
    }

    pub fn snapshot(&self, id: &str) -> Result<(), CoreError> {
        let dest = self.store.join(id);
        for rel in self.tracked_files()? {
            let from = self.project_root.join(&rel);
            let to = dest.join(&rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::copy(&from, &to).map_err(io)?;
        }
        Ok(())
    }

    pub fn restore(&self, id: &str) -> Result<(), CoreError> {
        let src = self.store.join(id);
        if !src.is_dir() {
            return Err(CoreError::Session(format!("no checkpoint '{id}'")));
        }

        // Files captured in the snapshot.
        let mut snapshot: HashSet<PathBuf> = HashSet::new();
        for entry in WalkBuilder::new(&src).hidden(false).build().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let rel = path.strip_prefix(&src).unwrap_or(path).to_path_buf();
            let to = self.project_root.join(&rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::copy(path, &to).map_err(io)?;
            snapshot.insert(rel);
        }

        // Prune files created after the snapshot so the tree matches the
        // checkpoint exactly (rewind == that point in time).
        for rel in self.tracked_files()? {
            if !snapshot.contains(&rel) {
                let _ = std::fs::remove_file(self.project_root.join(&rel));
            }
        }
        Ok(())
    }

    fn tracked_files(&self) -> Result<Vec<PathBuf>, CoreError> {
        let mut files = Vec::new();
        for entry in WalkBuilder::new(&self.project_root).build().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(&self.project_root)
                .map_err(|e| CoreError::Io(e.to_string()))?;
            // Never snapshot our own runtime state (checkpoints, session
            // history) — only project files + authored `.stepper/` config.
            if rel.starts_with(Path::new(".stepper/checkpoints"))
                || rel.starts_with(Path::new(".stepper/sessions"))
            {
                continue;
            }
            files.push(rel.to_path_buf());
        }
        Ok(files)
    }
}

fn io(e: std::io::Error) -> CoreError {
    CoreError::Io(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_then_restore_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "original").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();

        let snap = Snapshotter::new(root.clone());
        snap.snapshot("turn-1").unwrap();

        // mutate + create a new file after snapshot
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        std::fs::write(root.join("src/main.rs"), "broken").unwrap();
        std::fs::write(root.join("new_after.txt"), "should be pruned").unwrap();

        snap.restore("turn-1").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "original");
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "fn main() {}"
        );
        // a file created after the snapshot is pruned on rewind
        assert!(!root.join("new_after.txt").exists(), "post-snapshot file pruned");
    }
}
