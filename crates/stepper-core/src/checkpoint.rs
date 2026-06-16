use crate::error::CoreError;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// How many `turn-<N>` snapshots to keep. Every turn full-copies the working
/// tree, so without a cap the store grows unbounded for the life of the project.
pub const RETAIN: usize = 20;

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
        // Create the checkpoint dir up front so an *empty* working tree still
        // produces a real (restorable) snapshot. Otherwise the dir would only
        // appear as a side effect of copying a file, and a first turn in a fresh
        // project (no files yet) would leave no `turn-N` to `/rewind` to.
        std::fs::create_dir_all(&dest).map_err(io)?;
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

    /// Keep at most the `retain` most-recent `turn-<N>` snapshots, removing the
    /// oldest. Non-`turn-<N>` entries and a missing store are left untouched.
    pub fn prune(&self, retain: usize) -> Result<(), CoreError> {
        let Ok(entries) = std::fs::read_dir(&self.store) else {
            return Ok(());
        };
        let mut turns: Vec<(u64, PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(n) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("turn-"))
                .and_then(|s| s.parse::<u64>().ok())
            {
                turns.push((n, path));
            }
        }
        if turns.len() <= retain {
            return Ok(());
        }
        turns.sort_by_key(|(n, _)| *n);
        let remove = turns.len() - retain;
        for (_, path) in turns.into_iter().take(remove) {
            std::fs::remove_dir_all(&path).map_err(io)?;
        }
        Ok(())
    }

    /// Drop the entire checkpoint store (used by `/clear`, which begins a fresh
    /// session). A missing store is a no-op.
    pub fn clear(&self) -> Result<(), CoreError> {
        match std::fs::remove_dir_all(&self.store) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io(e)),
        }
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

    #[test]
    fn snapshot_of_an_empty_tree_is_restorable() {
        // A fresh project's first turn checkpoints an empty working tree. The
        // snapshot must still exist so `/rewind` to that pre-turn point works and
        // prunes whatever the turn created (regression: the empty-tree snapshot
        // used to create no directory, so restore failed with "no checkpoint").
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let snap = Snapshotter::new(root.clone());

        snap.snapshot("turn-1").unwrap();
        assert!(
            root.join(".stepper/checkpoints/turn-1").is_dir(),
            "an empty-tree snapshot must still create its checkpoint dir"
        );

        // The turn then creates a file; rewinding to turn-1 must remove it.
        std::fs::write(root.join("created_during_turn.txt"), "stepper-e2e-ok").unwrap();
        snap.restore("turn-1").unwrap();
        assert!(
            !root.join("created_during_turn.txt").exists(),
            "rewind to the empty pre-turn checkpoint prunes the file the turn created"
        );
    }

    #[test]
    fn prune_keeps_only_the_newest_retained_turns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        for n in 1..=25 {
            snap.snapshot(&format!("turn-{n}")).unwrap();
        }
        // a non-turn dir must be left alone by prune
        let store = root.join(".stepper/checkpoints");
        std::fs::create_dir_all(store.join("scratch")).unwrap();

        snap.prune(20).unwrap();

        for n in 1..=5 {
            assert!(!store.join(format!("turn-{n}")).exists(), "turn-{n} pruned");
        }
        for n in 6..=25 {
            assert!(store.join(format!("turn-{n}")).exists(), "turn-{n} kept");
        }
        assert!(store.join("scratch").exists(), "non-turn dir untouched");
        // the newest kept snapshot still restores
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        snap.restore("turn-25").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "x");
    }

    #[test]
    fn prune_is_noop_below_cap_and_without_store() {
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshotter::new(dir.path().to_path_buf());
        snap.prune(20).unwrap(); // no store yet
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        snap.snapshot("turn-1").unwrap();
        snap.prune(20).unwrap();
        assert!(dir.path().join(".stepper/checkpoints/turn-1").exists());
    }

    #[test]
    fn clear_removes_the_whole_store_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("turn-1").unwrap();
        assert!(root.join(".stepper/checkpoints/turn-1").exists());

        snap.clear().unwrap();
        assert!(!root.join(".stepper/checkpoints").exists());
        snap.clear().unwrap(); // missing store is a no-op
    }
}
