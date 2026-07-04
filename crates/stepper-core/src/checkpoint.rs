use crate::error::CoreError;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::PathBuf;

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
        // Clear any prior occupant so a reused id (after `/compact` reset the turn
        // counter) never merges a stale tree into this checkpoint.
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(io)?;
        }
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

        // Copy the snapshot back verbatim. `standard_filters(false)` disables
        // every ignore rule (gitignore + hidden) so the checkpoint's own contents
        // restore exactly — a `.gitignore` added since the snapshot must never
        // hide a file we deliberately captured. Individual copy failures are
        // collected instead of aborting, so one bad path (a read-only occupant,
        // a permission quirk) can't strand the tree half-restored.
        let mut snapshot: HashSet<PathBuf> = HashSet::new();
        let mut errors: Vec<String> = Vec::new();
        for entry in WalkBuilder::new(&src).standard_filters(false).build().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let rel = path.strip_prefix(&src).unwrap_or(path).to_path_buf();
            let to = self.project_root.join(&rel);
            if let Err(e) = restore_one(path, &to) {
                errors.push(format!("{}: {e}", rel.display()));
            }
            // Even on failure the path belongs to the snapshot: leaving it out
            // would let the prune below DELETE the current copy — strictly worse
            // than keeping the un-restored version.
            snapshot.insert(rel);
        }

        // Prune files created after the snapshot so the tree matches the
        // checkpoint exactly (rewind == that point in time).
        for rel in self.tracked_files()? {
            if !snapshot.contains(&rel) {
                let _ = std::fs::remove_file(self.project_root.join(&rel));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::Io(format!(
                "restored with {} failure(s): {}",
                errors.len(),
                errors.join("; ")
            )))
        }
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
        for (n, path) in turns.into_iter().take(remove) {
            std::fs::remove_dir_all(&path).map_err(io)?;
            // Drop the turn-count sidecar alongside its snapshot dir.
            let _ = std::fs::remove_file(self.meta_path(&format!("turn-{n}")));
        }
        Ok(())
    }

    /// After a `/rewind` that keeps `keep` turns, drop every `turn-<N>` checkpoint
    /// taken when MORE than `keep` turns were complete — the abandoned "future"
    /// trees. Without this they linger in the rewind picker as stale entries that,
    /// if re-selected, restore a future tree while the conversation has fewer turns
    /// (a files↔conversation desync). Mirrors [`clear_redo_snapshots`]' rule that a
    /// forward tree is unreachable once you go back. A missing store is a no-op.
    pub fn prune_forward(&self, keep: usize) {
        let Ok(entries) = std::fs::read_dir(&self.store) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path.file_name().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let Some(n) = id.strip_prefix("turn-").and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            // Turns-completed for this checkpoint: the recorded count, else N-1.
            let completed = self.checkpoint_turns(&id).unwrap_or(n.saturating_sub(1) as usize);
            if completed > keep {
                let _ = std::fs::remove_dir_all(&path);
                let _ = std::fs::remove_file(self.meta_path(&id));
            }
        }
    }

    /// Record how many session turns were complete when `id` was taken, so a
    /// later rewind truncates the session exactly — the turn-id counter can drift
    /// past the real turn count (failed turns, `/compact`), so the count is stored
    /// rather than parsed back out of the id.
    pub fn record_turns(&self, id: &str, turns_completed: usize) {
        let _ = std::fs::write(self.meta_path(id), turns_completed.to_string());
    }

    /// The turn count recorded with `id`, if any.
    pub fn checkpoint_turns(&self, id: &str) -> Option<usize> {
        std::fs::read_to_string(self.meta_path(id)).ok()?.trim().parse().ok()
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.store.join(format!("{id}.meta"))
    }

    /// Delete a single checkpoint dir and its turn-count sidecar (best-effort) —
    /// used to drop a `/redo` forward snapshot once it is consumed or invalidated.
    /// A missing id is a no-op.
    pub fn remove(&self, id: &str) {
        let _ = std::fs::remove_dir_all(self.store.join(id));
        let _ = std::fs::remove_file(self.meta_path(id));
    }

    /// Garbage-collect every `redo-<n>` forward snapshot left on disk. The redo
    /// stack is in-memory only, so at process start no live `/redo` can reference
    /// any of them — a crash between `/undo` and `/redo` would otherwise orphan
    /// them forever (`prune` only bounds `turn-<N>`). Called once at startup.
    pub fn clear_redo_snapshots(&self) {
        let Ok(entries) = std::fs::read_dir(&self.store) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("redo-") {
                // Handles both the `redo-<n>` dir and its `redo-<n>.meta` sidecar,
                // whichever this entry is, regardless of enumeration order.
                let path = entry.path();
                let _ = std::fs::remove_dir_all(&path);
                let _ = std::fs::remove_file(&path);
            }
        }
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

    /// Record `session_id` as the store's owner (best-effort). Checkpoints are
    /// only meaningful for the session whose turns produced them, but the store
    /// itself is project-global — the stamp is what lets [`reconcile_owner`]
    /// detect a session switch.
    ///
    /// [`reconcile_owner`]: Snapshotter::reconcile_owner
    pub fn stamp_owner(&self, session_id: &str) {
        let _ = std::fs::create_dir_all(&self.store);
        let _ = std::fs::write(self.store.join("owner"), session_id);
    }

    /// Clear the store unless `session_id` already owns it, then stamp the new
    /// owner. Called wherever the live session's identity changes (process
    /// start, in-session `/resume`) so `/rewind`/`/undo` can never restore
    /// another session's tree over the current work. Returns `true` when the
    /// store was cleared. A pre-owner-stamp store (older stepper) reads as
    /// unowned and is cleared — checkpoints are a disposable safety net.
    pub fn reconcile_owner(&self, session_id: &str) -> Result<bool, CoreError> {
        let owner = std::fs::read_to_string(self.store.join("owner")).ok();
        if owner.as_deref().map(str::trim) == Some(session_id) {
            return Ok(false);
        }
        self.clear()?;
        self.stamp_owner(session_id);
        Ok(true)
    }

    fn tracked_files(&self) -> Result<Vec<PathBuf>, CoreError> {
        let root = self.project_root.clone();
        let mut files = Vec::new();
        // `hidden(false)` so dotfiles (`.github`, `.gitignore`, …) are captured;
        // `require_git(false)` so `.gitignore` is honored even in a non-git
        // project (otherwise `target/`, `node_modules/` get full-copied every
        // turn). `.git` and our own runtime state (the whole `.stepper/`:
        // checkpoints, sessions, setting.json, commands, exports, memory, …) are
        // pruned at traversal so they are never walked and never restored/pruned
        // by a `/rewind` or `/undo` of the user's working tree. `starts_with`
        // matches whole path components, so `.github` is unaffected.
        for entry in WalkBuilder::new(&self.project_root)
            .hidden(false)
            .require_git(false)
            .filter_entry(move |e| match e.path().strip_prefix(&root) {
                Ok(rel) => !(rel.starts_with(".git") || rel.starts_with(".stepper")),
                Err(_) => true,
            })
            .build()
            .flatten()
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(&self.project_root)
                .map_err(|e| CoreError::Io(e.to_string()))?;
            files.push(rel.to_path_buf());
        }
        Ok(files)
    }
}

fn io(e: std::io::Error) -> CoreError {
    CoreError::Io(e.to_string())
}

/// Copy one snapshot file over the working tree. An existing occupant is
/// removed first: `fs::copy` opens the destination for writing, so a read-only
/// file (EACCES) or a path whose type changed during the turn (file↔dir) would
/// otherwise abort the restore midway. Removal only needs write permission on
/// the parent, and `copy` re-creates the file with the snapshot's own mode.
fn restore_one(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        ensure_dir(parent)?;
    }
    match std::fs::symlink_metadata(to) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(to)?,
        Ok(_) => std::fs::remove_file(to)?,
        Err(_) => {}
    }
    std::fs::copy(from, to)?;
    Ok(())
}

/// `create_dir_all` that also clears any FILE occupying a directory component
/// (a `foo/` dir replaced by a `foo` file mid-turn would otherwise block the
/// restore of `foo/bar.txt`).
fn ensure_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if std::fs::symlink_metadata(dir).is_ok() {
        std::fs::remove_file(dir)?;
    } else if let Some(parent) = dir.parent() {
        ensure_dir(parent)?;
    }
    std::fs::create_dir_all(dir)
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
    fn stepper_runtime_state_is_excluded_from_checkpoints() {
        // The whole `.stepper/` dir (setting.json, commands, exports, memory, …)
        // is stepper's own runtime state, not the user's working tree: a `/rewind`
        // or `/undo` must never revert/prune it. Regression: only `.stepper/
        // checkpoints` and `.stepper/sessions` used to be excluded, so editing
        // settings / adding a command / exporting mid-session, then rewinding,
        // reverted or deleted those files.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("code.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(root.join(".stepper/commands")).unwrap();
        std::fs::write(root.join(".stepper/setting.json"), "{\"mode\":\"auto\"}").unwrap();

        let snap = Snapshotter::new(root.clone());
        snap.snapshot("turn-1").unwrap();

        // After the snapshot: the user edits code (to be reverted) AND stepper
        // writes its own runtime files (must survive the rewind untouched).
        std::fs::write(root.join("code.rs"), "fn main() { broken }").unwrap();
        std::fs::write(root.join(".stepper/setting.json"), "{\"mode\":\"plan\"}").unwrap();
        std::fs::write(root.join(".stepper/commands/greet.md"), "hi").unwrap();
        std::fs::create_dir_all(root.join(".stepper/exports")).unwrap();
        std::fs::write(root.join(".stepper/exports/t.md"), "transcript").unwrap();

        snap.restore("turn-1").unwrap();

        // The user's code is reverted...
        assert_eq!(std::fs::read_to_string(root.join("code.rs")).unwrap(), "fn main() {}");
        // ...but every `.stepper/` file written after the snapshot is preserved
        // (neither reverted to its pre-turn state nor pruned as "post-snapshot").
        assert_eq!(
            std::fs::read_to_string(root.join(".stepper/setting.json")).unwrap(),
            "{\"mode\":\"plan\"}",
            ".stepper/setting.json is not reverted"
        );
        assert!(root.join(".stepper/commands/greet.md").exists(), "a command added mid-session survives");
        assert!(root.join(".stepper/exports/t.md").exists(), "an export written mid-session survives");
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
    fn prune_forward_drops_abandoned_future_checkpoints() {
        // After a rewind that keeps `keep` turns, checkpoints taken when more than
        // `keep` turns were complete are abandoned futures and must be removed, so
        // they can't be re-selected later and restore a tree newer than the (now
        // shorter) conversation.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        for n in 1..=5 {
            snap.snapshot(&format!("turn-{n}")).unwrap();
            snap.record_turns(&format!("turn-{n}"), n - 1); // turn-N taken after N-1 turns
        }
        let store = root.join(".stepper/checkpoints");

        // Rewind kept 2 turns: turn-1 (0 done) and turn-2 (1 done) and turn-3 (2
        // done) stay; turn-4 (3 done) and turn-5 (4 done) are the abandoned future.
        snap.prune_forward(2);

        for n in 1..=3 {
            assert!(store.join(format!("turn-{n}")).exists(), "turn-{n} kept (<= keep)");
            assert!(store.join(format!("turn-{n}.meta")).exists(), "turn-{n}.meta kept");
        }
        for n in 4..=5 {
            assert!(!store.join(format!("turn-{n}")).exists(), "turn-{n} pruned (future)");
            assert!(!store.join(format!("turn-{n}.meta")).exists(), "turn-{n}.meta pruned");
        }
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
    fn records_and_reads_the_turn_count() {
        // The recorded turn-count is what /rewind truncates the session by, so it
        // must survive a drifted turn-id (the id counter can move past the real
        // turn count after failed turns or /compact).
        let dir = tempfile::tempdir().unwrap();
        let snap = Snapshotter::new(dir.path().to_path_buf());
        snap.snapshot("turn-3").unwrap();
        snap.record_turns("turn-3", 2);
        assert_eq!(snap.checkpoint_turns("turn-3"), Some(2));
        assert_eq!(snap.checkpoint_turns("turn-9"), None);
    }

    #[test]
    fn prune_drops_the_turn_count_sidecar_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        for n in 1..=25 {
            snap.snapshot(&format!("turn-{n}")).unwrap();
            snap.record_turns(&format!("turn-{n}"), n);
        }
        snap.prune(20).unwrap();
        let store = root.join(".stepper/checkpoints");
        // pruned turns lose both their dir and their .meta sidecar
        assert!(!store.join("turn-1.meta").exists(), "pruned turn-1 sidecar removed");
        assert_eq!(snap.checkpoint_turns("turn-1"), None);
        // kept turns retain their recorded count
        assert_eq!(snap.checkpoint_turns("turn-25"), Some(25));
    }

    #[test]
    fn remove_drops_one_checkpoint_dir_and_sidecar_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("redo-0").unwrap();
        snap.record_turns("redo-0", 3);
        snap.snapshot("turn-1").unwrap();
        let store = root.join(".stepper/checkpoints");
        assert!(store.join("redo-0").is_dir() && store.join("redo-0.meta").exists());
        snap.remove("redo-0");
        assert!(!store.join("redo-0").exists(), "redo-0 dir removed");
        assert!(!store.join("redo-0.meta").exists(), "redo-0 sidecar removed");
        assert!(store.join("turn-1").is_dir(), "an unrelated checkpoint is untouched");
        // A missing id is a no-op.
        snap.remove("redo-0");
    }

    #[test]
    fn redo_snapshots_survive_prune_and_turn_enumeration() {
        // `redo-<n>` forward snapshots must NOT be pruned by RETAIN (only `turn-<N>`
        // are) so a pending /redo keeps its tree even past 20 turns.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("redo-0").unwrap();
        for n in 1..=25 {
            snap.snapshot(&format!("turn-{n}")).unwrap();
        }
        snap.prune(20).unwrap();
        let store = root.join(".stepper/checkpoints");
        assert!(store.join("redo-0").is_dir(), "redo-0 survives prune");
        assert!(!store.join("turn-1").exists(), "oldest turn-1 is pruned");
        assert!(store.join("turn-25").is_dir(), "newest turn kept");
    }

    #[test]
    fn clear_redo_snapshots_gcs_every_redo_dir_and_sidecar_but_keeps_turns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("redo-0").unwrap();
        snap.record_turns("redo-0", 1);
        snap.snapshot("redo-3").unwrap();
        snap.snapshot("turn-1").unwrap();
        snap.record_turns("turn-1", 0);
        let store = root.join(".stepper/checkpoints");
        snap.clear_redo_snapshots();
        assert!(!store.join("redo-0").exists() && !store.join("redo-0.meta").exists());
        assert!(!store.join("redo-3").exists());
        assert!(store.join("turn-1").is_dir(), "turn checkpoints are untouched");
        assert_eq!(snap.checkpoint_turns("turn-1"), Some(0), "turn sidecar kept");
        // Idempotent / no store = no-op.
        snap.clear_redo_snapshots();
    }

    #[test]
    fn restore_overwrites_a_read_only_file_and_finishes_the_prune() {
        // A tracked file without the owner write bit used to abort restore with
        // EACCES midway (partial tree, prune never reached). Removal-then-copy
        // only needs parent-dir write permission.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "original").unwrap();
        std::fs::write(root.join("locked.txt"), "locked-v1").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("turn-1").unwrap();

        std::fs::write(root.join("a.txt"), "changed").unwrap();
        std::fs::write(root.join("new_after.txt"), "prune me").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("locked.txt"), std::fs::Permissions::from_mode(0o444))
                .unwrap();
        }

        snap.restore("turn-1").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(root.join("locked.txt")).unwrap(), "locked-v1");
        assert!(!root.join("new_after.txt").exists(), "prune still ran to completion");
    }

    #[test]
    fn restore_replaces_a_dir_swapped_in_for_a_file_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/mod.rs"), "mod v1").unwrap();
        std::fs::write(root.join("single.txt"), "file v1").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("turn-1").unwrap();

        // During the turn: the dir becomes a file, and the file becomes a dir.
        std::fs::remove_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg"), "now a file").unwrap();
        std::fs::remove_file(root.join("single.txt")).unwrap();
        std::fs::create_dir_all(root.join("single.txt")).unwrap();
        std::fs::write(root.join("single.txt/inner"), "x").unwrap();

        snap.restore("turn-1").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("pkg/mod.rs")).unwrap(), "mod v1");
        assert_eq!(std::fs::read_to_string(root.join("single.txt")).unwrap(), "file v1");
    }

    #[test]
    fn reconcile_owner_clears_for_a_different_or_missing_owner_and_keeps_for_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.snapshot("turn-1").unwrap();
        let store = root.join(".stepper/checkpoints");

        // Pre-owner-stamp store (older stepper) reads as unowned → cleared.
        assert!(snap.reconcile_owner("session-a").unwrap());
        assert!(!store.join("turn-1").exists(), "unowned checkpoints dropped");

        // Same session again → kept.
        snap.snapshot("turn-1").unwrap();
        assert!(!snap.reconcile_owner("session-a").unwrap());
        assert!(store.join("turn-1").is_dir(), "owning session keeps its checkpoints");

        // A different session (resume/fork/clear) → cleared and re-stamped.
        assert!(snap.reconcile_owner("session-b").unwrap());
        assert!(!store.join("turn-1").exists(), "another session's checkpoints dropped");
        assert!(!snap.reconcile_owner("session-b").unwrap(), "new owner stamped");
    }

    #[test]
    fn owner_stamp_is_invisible_to_prune_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let snap = Snapshotter::new(root.clone());
        snap.stamp_owner("session-a");
        for n in 1..=25 {
            snap.snapshot(&format!("turn-{n}")).unwrap();
        }
        snap.prune(20).unwrap();
        snap.prune_forward(30);
        snap.clear_redo_snapshots();
        let store = root.join(".stepper/checkpoints");
        assert_eq!(
            std::fs::read_to_string(store.join("owner")).unwrap(),
            "session-a",
            "the owner stamp survives every maintenance pass"
        );
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        snap.restore("turn-25").unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "x");
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
