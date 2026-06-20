//! Session persistence and working-tree checkpoints over a real temp tree:
//! atomic save/load, resume-context seeding, snapshot/restore pruning of files
//! created after a checkpoint, and the truncate-and-resave step the `/rewind`
//! path uses to drop turns at or after the rewound checkpoint.

use stepper_core::{SessionRecord, SessionStore, Snapshotter, TurnRecord};

fn turn(user: &str, layer: &str, summary: &str) -> TurnRecord {
    TurnRecord {
        user: user.into(),
        summaries: vec![(layer.into(), summary.into())],
        messages: Vec::new(),
        ..Default::default()
    }
}

#[test]
fn save_then_load_round_trips_atomically_and_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());

    let mut record = SessionRecord {
        id: "sess-1".into(),
        name: None,
        turns: vec![turn("first request", "plan", "planned it")],
    };
    store.save(&record).unwrap();

    let loaded = store.load("sess-1").unwrap();
    assert_eq!(loaded.id, "sess-1");
    assert_eq!(loaded.turns.len(), 1);
    assert_eq!(loaded.turns[0].user, "first request");
    assert_eq!(loaded.turns[0].summaries[0], ("plan".into(), "planned it".into()));

    let sessions_dir = dir.path().join(".stepper").join("sessions");
    let tmp_left = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!tmp_left, "atomic write must not leave a .tmp file behind");

    record.turns.push(turn("second request", "build", "built it"));
    store.save(&record).unwrap();
    let reloaded = store.load("sess-1").unwrap();
    assert_eq!(reloaded.turns.len(), 2, "save overwrites in place");
    assert_eq!(reloaded.turns[1].user, "second request");
}

#[test]
fn load_missing_session_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    assert!(store.load("does-not-exist").is_none());
}

#[test]
fn resume_context_seeds_prior_turns() {
    let empty = SessionRecord::fresh();
    assert!(empty.resume_context().is_empty(), "no turns => no resume seed");

    let record = SessionRecord {
        id: "r".into(),
        name: None,
        turns: vec![
            turn("add a CLI flag", "plan", "decided on --verbose"),
            turn("ship it", "build", "wired the flag"),
        ],
    };
    let ctx = record.resume_context();
    assert!(ctx.contains("# Earlier in this session"));
    assert!(ctx.contains("## Turn 1"));
    assert!(ctx.contains("Request: add a CLI flag"));
    assert!(ctx.contains("- plan: decided on --verbose"));
    assert!(ctx.contains("## Turn 2"));
    assert!(ctx.contains("- build: wired the flag"));
}

#[test]
fn fresh_sessions_have_distinct_ids() {
    let a = SessionRecord::fresh();
    let b = SessionRecord::fresh();
    assert_ne!(a.id, b.id);
    assert!(a.turns.is_empty());
}

#[test]
fn snapshot_restore_prunes_files_created_after_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("keep.txt"), "v1").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn one() {}").unwrap();

    let snap = Snapshotter::new(root.clone());
    snap.snapshot("turn-3").unwrap();

    std::fs::write(root.join("keep.txt"), "v2").unwrap();
    std::fs::write(root.join("src/lib.rs"), "fn two() {}").unwrap();
    std::fs::write(root.join("scratch.txt"), "created after snapshot").unwrap();
    std::fs::create_dir_all(root.join("gen")).unwrap();
    std::fs::write(root.join("gen/out.rs"), "generated later").unwrap();

    snap.restore("turn-3").unwrap();

    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "v1");
    assert_eq!(
        std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "fn one() {}"
    );
    assert!(
        !root.join("scratch.txt").exists(),
        "a file created after the snapshot is pruned on rewind"
    );
    assert!(
        !root.join("gen/out.rs").exists(),
        "a nested file created after the snapshot is pruned on rewind"
    );
}

#[test]
fn restore_unknown_checkpoint_errors() {
    let dir = tempfile::tempdir().unwrap();
    let snap = Snapshotter::new(dir.path().to_path_buf());
    let err = snap.restore("turn-999").unwrap_err();
    assert!(err.to_string().contains("turn-999"), "error names the missing id: {err}");
}

#[test]
fn rewind_truncates_session_turns_at_the_checkpoint_then_persists() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path());
    let mut record = SessionRecord {
        id: "rewindable".into(),
        name: None,
        turns: vec![
            turn("turn one", "plan", "a"),
            turn("turn two", "plan", "b"),
            turn("turn three", "plan", "c"),
        ],
    };
    store.save(&record).unwrap();

    let checkpoint_id = "turn-2";
    let n: usize = checkpoint_id.strip_prefix("turn-").unwrap().parse().unwrap();
    let keep = n.saturating_sub(1);
    record.turns.truncate(keep);
    store.save(&record).unwrap();

    let reloaded = store.load("rewindable").unwrap();
    assert_eq!(
        reloaded.turns.len(),
        1,
        "rewinding to turn-2 keeps only the turn(s) before it"
    );
    assert_eq!(reloaded.turns[0].user, "turn one");
}
