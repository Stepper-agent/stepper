//! Built-in tool behavior against a real temp project, with an always-approve
//! approver. Exercises the file lifecycle, bash, grep, and the secret-file and
//! permission guards.

use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_tools::{Approval, Approver, ToolCx, ToolError, ToolRegistry};
use tokio_util::sync::CancellationToken;

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Allow
    }
}

struct DenyAll;
#[async_trait]
impl Approver for DenyAll {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Deny
    }
}

fn approval_kind(approval: &Approval) -> &'static str {
    match approval {
        Approval::Command { .. } => "command",
        Approval::FileEdit { .. } => "file_edit",
        Approval::OutsideProject { .. } => "outside_project",
        Approval::Mcp { .. } => "mcp",
    }
}

struct Recording {
    decision: Decision,
    seen: Mutex<Vec<Approval>>,
}

impl Recording {
    fn new(decision: Decision) -> Arc<Self> {
        Arc::new(Recording {
            decision,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.seen.lock().unwrap().iter().map(approval_kind).collect()
    }

    fn last(&self) -> Approval {
        self.seen.lock().unwrap().last().cloned().unwrap()
    }
}

#[async_trait]
impl Approver for Recording {
    async fn request(&self, approval: Approval) -> Decision {
        self.seen.lock().unwrap().push(approval);
        self.decision
    }
}

fn cx_with(root: &std::path::Path, mode: PermissionMode, approver: Arc<dyn Approver>) -> ToolCx {
    ToolCx {
        cwd: root.to_path_buf(),
        project_root: root.to_path_buf(),
        home: None,
        mode,
        live_mode: None,
        rules: Arc::new(RuleSet::from_lists(&["Bash(*)".into()], &[], &[])),
        approver,
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    }
}

#[tokio::test]
async fn write_read_edit_grep_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    reg.get("write_file")
        .unwrap()
        .call(json!({"path": "a.txt", "content": "hello\nworld\n"}), &cx)
        .await
        .unwrap();

    let read = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "a.txt"}), &cx)
        .await
        .unwrap();
    assert!(read.content_text().contains("hello"));

    reg.get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "world", "new_string": "stepper"}),
            &cx,
        )
        .await
        .unwrap();

    let grep = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "stepper"}), &cx)
        .await
        .unwrap();
    assert!(grep.content_text().contains("a.txt"));
}

#[tokio::test]
async fn search_sees_dotfiles_but_prunes_the_git_dir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    std::fs::write(root.join(".github/workflows/ci.yml"), "name: ci\n").unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/config"), "secret = 1\n").unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();

    let reg = ToolRegistry::builtins();
    let cx = cx_with(root, PermissionMode::AcceptEdits, Arc::new(AllowAll));

    // glob descends into dotfile dirs like `.github/`...
    let glob = reg
        .get("glob")
        .unwrap()
        .call(json!({"pattern": "**/*.yml"}), &cx)
        .await
        .unwrap();
    assert!(
        glob.content_text().contains("ci.yml"),
        "glob sees files under .github/: {}",
        glob.content_text()
    );

    // ...list_dir shows dotfiles at the root but never the `.git` dir...
    let list = reg.get("list_dir").unwrap().call(json!({}), &cx).await.unwrap();
    let text = list.content_text();
    assert!(text.contains(".gitignore"), "dotfiles are listed: {text}");
    assert!(text.contains(".github"), "dot-dirs are listed: {text}");
    assert!(!text.contains(".git/"), ".git dir is pruned from listings: {text}");

    // ...and grep can search dotfile dirs but not the pruned `.git` internals.
    let grep = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "secret"}), &cx)
        .await
        .unwrap();
    assert!(
        !grep.content_text().contains(".git/config"),
        "grep does not descend into .git: {}",
        grep.content_text()
    );
}

#[tokio::test]
async fn read_with_offset_limit_slices_a_large_file_instead_of_rejecting() {
    let dir = tempfile::tempdir().unwrap();
    let big: String = (0..40_000).map(|i| format!("line {i}\n")).collect();
    assert!(big.len() > 256 * 1024, "the fixture must exceed the whole-file cap");
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();

    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));
    let out = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "big.txt", "offset": 1, "limit": 3}), &cx)
        .await
        .unwrap();
    let text = out.content_text();
    assert!(
        text.contains("line 0") && text.contains("line 2"),
        "offset/limit reads a slice of a file too big to read whole: {text}"
    );
    assert!(!text.contains("line 100"), "the limit is honored");
}

#[tokio::test]
async fn read_slice_larger_than_the_cap_is_truncated_on_a_char_boundary() {
    // A slice that itself exceeds 256KB (and straddles a multibyte char at the cap)
    // must be truncated safely (no panic) with a marker, not returned whole.
    let dir = tempfile::tempdir().unwrap();
    let line = format!("{}\n", "é".repeat(300_000)); // multibyte, one huge line > 256KB
    assert!(line.len() > 256 * 1024);
    std::fs::write(dir.path().join("huge.txt"), &line).unwrap();

    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));
    let out = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "huge.txt", "offset": 1}), &cx)
        .await
        .unwrap();
    let text = out.content_text();
    assert!(text.len() < line.len(), "the oversized slice is capped");
    assert!(text.contains("truncated at the read-size limit"), "marker present: end={:?}", &text[text.len().saturating_sub(80)..]);
}

#[tokio::test]
async fn enumerators_skip_deny_listed_subpaths() {
    // A `deny Read(/secret/**)` rule must fence grep/glob/list_dir out of that
    // subtree even though the search root (the project) is allowed.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("secret")).unwrap();
    std::fs::write(root.join("public.txt"), "needle here\n").unwrap();
    std::fs::write(root.join("secret/private.txt"), "needle here\n").unwrap();

    let cx = ToolCx {
        cwd: root.to_path_buf(),
        project_root: root.to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        live_mode: None,
        rules: Arc::new(RuleSet::from_lists(&[], &[], &["Read(/secret/**)".into()])),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    };
    let reg = ToolRegistry::builtins();

    let grep = reg.get("grep").unwrap().call(json!({"pattern": "needle"}), &cx).await.unwrap();
    let grep = grep.content_text();
    assert!(grep.contains("public.txt"), "allowed match kept: {grep}");
    assert!(!grep.contains("private.txt"), "denied subpath excluded from grep: {grep}");

    let glob = reg.get("glob").unwrap().call(json!({"pattern": "**/*.txt"}), &cx).await.unwrap();
    let glob = glob.content_text();
    assert!(glob.contains("public.txt"), "allowed file listed: {glob}");
    assert!(!glob.contains("private.txt"), "denied subpath excluded from glob: {glob}");

    // Listing the denied directory itself yields nothing (its child is denied).
    let ls = reg.get("list_dir").unwrap().call(json!({"path": "secret"}), &cx).await.unwrap();
    assert_eq!(ls.content_text(), "empty", "denied dir contents are not enumerated");
}

#[tokio::test]
async fn bash_runs_and_reports_exit() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let ok = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "echo hi"}), &cx)
        .await
        .unwrap();
    assert!(ok.content_text().contains("hi"));
    assert!(!ok.is_error);

    let fail = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "exit 3"}), &cx)
        .await
        .unwrap();
    assert!(fail.is_error);
}

#[tokio::test]
async fn secret_file_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "SECRET=1").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let err = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": ".env"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, stepper_tools::ToolError::Denied(_)));
}

#[tokio::test]
async fn outside_project_write_denied_when_approver_denies() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    // Auto mode: writing outside the project asks; the approver says no.
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(DenyAll));

    let target = outside.path().join("x.txt");
    let err = reg
        .get("write_file")
        .unwrap()
        .call(json!({"path": target.to_string_lossy(), "content": "nope"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, stepper_tools::ToolError::Denied(_)));
}

#[tokio::test]
async fn layer_filter_restricts_tools() {
    let reg = ToolRegistry::builtins();
    let view = reg.filtered(&["read_file".into(), "grep".into()], &[]);
    assert!(view.get("read_file").is_some());
    assert!(view.get("bash").is_none());

    let no_bash = reg.filtered(&[], &["bash".into()]);
    assert!(no_bash.get("bash").is_none());
    assert!(no_bash.get("read_file").is_some());
}

#[tokio::test]
async fn edit_returns_unified_diff_and_persists_change() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let result = reg
        .get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "two", "new_string": "TWO"}),
            &cx,
        )
        .await
        .unwrap();

    let text = result.content_text();
    assert!(text.contains("1 replacement(s)"));
    assert!(text.contains("--- before"));
    assert!(text.contains("+++ after"));
    assert!(text.contains("-two"));
    assert!(text.contains("+TWO"));

    let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
    assert_eq!(on_disk, "one\nTWO\nthree\n");
}

#[tokio::test]
async fn edit_rejects_non_unique_old_string_without_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\nx\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let err = reg
        .get("edit_file")
        .unwrap()
        .call(json!({"path": "a.txt", "old_string": "x", "new_string": "y"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "x\nx\n");

    let ok = reg
        .get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "x", "new_string": "y", "replace_all": true}),
            &cx,
        )
        .await
        .unwrap();
    assert!(ok.content_text().contains("2 replacement(s)"));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "y\ny\n");
}

#[tokio::test]
async fn edit_missing_old_string_is_invalid_args() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let err = reg
        .get("edit_file")
        .unwrap()
        .call(json!({"path": "a.txt", "old_string": "absent", "new_string": "x"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

/// An approver whose `ask` never returns — stands in for a user staring at the
/// question overlay while the turn is cancelled (timeout / Interrupt) out from
/// under them. `request` is unused here.
struct NeverAsk;
#[async_trait]
impl Approver for NeverAsk {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Allow
    }
    async fn ask(&self, _question: &str, _options: &[String]) -> Option<usize> {
        std::future::pending::<()>().await;
        None
    }
}

#[tokio::test]
async fn ask_user_question_unwinds_when_the_turn_is_cancelled() {
    // A parked question must be interruptible: with the cancel token already
    // fired, the tool returns the "interrupted" result promptly instead of
    // hanging on the never-answering approver (which would hang the whole turn).
    let dir = tempfile::tempdir().unwrap();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(NeverAsk));
    cx.cancel.cancel();
    let reg = ToolRegistry::builtins();

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reg.get("ask_user_question").unwrap().call(
            json!({"question": "pick", "options": ["a", "b"]}),
            &cx,
        ),
    )
    .await
    .expect("ask must not hang when cancelled")
    .unwrap();
    let text = res.content_text();
    assert!(text.contains("interrupted"), "expected interrupted result, got: {text}");
}

#[tokio::test]
async fn edit_empty_old_string_is_rejected_not_a_whole_file_splice() {
    // An empty `old_string` matches at every char boundary: `replace` would
    // splice `new_string` between every character (file corruption). It must be
    // rejected before any read/gate/write, with and without replace_all.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    for replace_all in [false, true] {
        let err = reg
            .get("edit_file")
            .unwrap()
            .call(
                json!({"path": "a.txt", "old_string": "", "new_string": "X", "replace_all": replace_all}),
                &cx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs(_)), "replace_all={replace_all}");
    }
    // The file is untouched.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello\n");
}

#[tokio::test]
async fn secret_files_are_denied_for_write_and_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("id_rsa"), "PRIVATE").unwrap();
    std::fs::write(dir.path().join("credentials"), "k=v").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let write_err = reg
        .get("write_file")
        .unwrap()
        .call(json!({"path": ".env.local", "content": "X=1"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(write_err, ToolError::Denied(_)));
    assert!(!dir.path().join(".env.local").exists());

    let edit_err = reg
        .get("edit_file")
        .unwrap()
        .call(json!({"path": "id_rsa", "old_string": "PRIVATE", "new_string": "x"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(edit_err, ToolError::Denied(_)));
    assert_eq!(std::fs::read_to_string(dir.path().join("id_rsa")).unwrap(), "PRIVATE");

    let read_err = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "credentials"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(read_err, ToolError::Denied(_)));
}

#[tokio::test]
async fn grep_skips_secret_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "TOKEN=needle").unwrap();
    std::fs::write(dir.path().join("src.txt"), "needle here").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let grep = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "needle"}), &cx)
        .await
        .unwrap();
    let text = grep.content_text();
    assert!(text.contains("src.txt"));
    assert!(!text.contains(".env"));
}

#[tokio::test]
async fn grep_invalid_regex_is_invalid_args() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let err = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "("}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn grep_and_glob_honor_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "needle").unwrap();
    std::fs::write(dir.path().join("kept.txt"), "needle").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let grep = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "needle"}), &cx)
        .await
        .unwrap();
    let grep_text = grep.content_text();
    assert!(grep_text.contains("kept.txt"));
    assert!(!grep_text.contains("ignored.txt"));

    let glob = reg
        .get("glob")
        .unwrap()
        .call(json!({"pattern": "*.txt"}), &cx)
        .await
        .unwrap();
    let glob_text = glob.content_text();
    assert!(glob_text.contains("kept.txt"));
    assert!(!glob_text.contains("ignored.txt"));
}

#[tokio::test]
async fn glob_and_list_skip_secret_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "X=1").unwrap();
    std::fs::write(dir.path().join("server.pem"), "CERT").unwrap();
    std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let glob = reg
        .get("glob")
        .unwrap()
        .call(json!({"pattern": "*"}), &cx)
        .await
        .unwrap();
    let glob_text = glob.content_text();
    assert!(glob_text.contains("main.rs"));
    assert!(!glob_text.contains(".env"));
    assert!(!glob_text.contains("server.pem"));

    let list = reg
        .get("list_dir")
        .unwrap()
        .call(json!({}), &cx)
        .await
        .unwrap();
    let list_text = list.content_text();
    assert!(list_text.contains("main.rs"));
    assert!(!list_text.contains(".env"));
    assert!(!list_text.contains("server.pem"));
}

#[tokio::test]
async fn glob_no_match_reports_empty_result() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.rs"), "x").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let glob = reg
        .get("glob")
        .unwrap()
        .call(json!({"pattern": "*.py"}), &cx)
        .await
        .unwrap();
    assert_eq!(glob.content_text(), "no files matched");
}

#[tokio::test]
async fn write_outside_project_asks_and_records_file_edit_approval() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Allow);
    let cx = cx_with(dir.path(), PermissionMode::Auto, approver.clone());

    let target = outside.path().join("note.txt");
    reg.get("write_file")
        .unwrap()
        .call(json!({"path": target.to_string_lossy(), "content": "hi"}), &cx)
        .await
        .unwrap();

    assert_eq!(approver.kinds(), vec!["file_edit"]);
    match approver.last() {
        Approval::FileEdit { path, old, new } => {
            assert_eq!(path, target);
            assert_eq!(old, "");
            assert_eq!(new, "hi");
        }
        other => panic!("unexpected approval {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
}

#[tokio::test]
async fn read_outside_project_in_auto_allows_but_accept_edits_asks() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("data.txt");
    std::fs::write(&target, "payload").unwrap();
    let reg = ToolRegistry::builtins();

    // Policy A: in Auto, an out-of-project READ is auto-approved — the approver is
    // never consulted (a read can't damage the tree; secrets are screened here).
    let auto_approver = Recording::new(Decision::Allow);
    let auto_cx = cx_with(dir.path(), PermissionMode::Auto, auto_approver.clone());
    let read = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": target.to_string_lossy()}), &auto_cx)
        .await
        .unwrap();
    assert_eq!(read.content_text(), "payload");
    assert!(auto_approver.kinds().is_empty(), "Auto must not prompt for an out-of-project read");

    // AcceptEdits still gates out-of-project reads, recording an OutsideProject ask.
    let ae_approver = Recording::new(Decision::Allow);
    let ae_cx = cx_with(dir.path(), PermissionMode::AcceptEdits, ae_approver.clone());
    let read = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": target.to_string_lossy()}), &ae_cx)
        .await
        .unwrap();
    assert_eq!(read.content_text(), "payload");
    assert_eq!(ae_approver.kinds(), vec!["outside_project"]);
    match ae_approver.last() {
        Approval::OutsideProject { path, action } => {
            assert_eq!(path, target);
            assert_eq!(action, "read");
        }
        other => panic!("unexpected approval {other:?}"),
    }
}

#[tokio::test]
async fn in_project_write_does_not_ask_the_approver() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Deny);
    let cx = cx_with(dir.path(), PermissionMode::Auto, approver.clone());

    reg.get("write_file")
        .unwrap()
        .call(json!({"path": "in.txt", "content": "v"}), &cx)
        .await
        .unwrap();

    assert!(approver.kinds().is_empty());
    assert_eq!(std::fs::read_to_string(dir.path().join("in.txt")).unwrap(), "v");
}

#[tokio::test]
async fn bash_records_command_approval_in_gated_mode() {
    // A gated mode (Default) routes an un-ruled shell command through the
    // approver. (Auto auto-allows shell now, so it would not prompt — see the
    // permission crate's auto-mode tests.)
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Allow);
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::Default,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver: approver.clone(),
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    };

    let result = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "echo gated"}), &cx)
        .await
        .unwrap();
    assert!(result.content_text().contains("gated"));

    assert_eq!(approver.kinds(), vec!["command"]);
    match approver.last() {
        Approval::Command { command, outside_project } => {
            assert_eq!(command, "echo gated");
            assert!(!outside_project);
        }
        other => panic!("unexpected approval {other:?}"),
    }
}

#[tokio::test]
async fn bash_timeout_kills_long_process_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let start = Instant::now();
    let err = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "sleep 30", "timeout_ms": 200}), &cx)
        .await
        .unwrap_err();
    let elapsed = start.elapsed();

    match err {
        ToolError::Execution(msg) => assert!(msg.contains("timed out")),
        other => panic!("expected timeout, got {other:?}"),
    }
    assert!(elapsed.as_secs() < 5, "timeout did not bound runtime: {elapsed:?}");
}

#[tokio::test]
async fn bash_cancellation_token_aborts_run() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cancel = CancellationToken::new();
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::Auto,
        live_mode: None,
        rules: Arc::new(RuleSet::from_lists(&["Bash(*)".into()], &[], &[])),
        approver: Arc::new(AllowAll),
        cancel: cancel.clone(),
        sandbox_writable_roots: None,
    };

    let bash = reg.get("bash").unwrap();
    let start = Instant::now();
    let handle = tokio::spawn(async move {
        bash.call(json!({"command": "sleep 30"}), &cx).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    cancel.cancel();

    let result = handle.await.unwrap();
    let elapsed = start.elapsed();
    match result {
        Err(ToolError::Execution(msg)) => assert!(msg.contains("cancelled"), "got {msg}"),
        other => panic!("expected cancellation error, got {other:?}"),
    }
    assert!(
        elapsed.as_secs() < 5,
        "cancellation must interrupt the running sleep, not wait it out: {elapsed:?}"
    );
}

#[tokio::test]
async fn bash_denied_by_default_when_no_allow_rule() {
    // Gated mode: with no allow rule the shell prompts, and a denying approver
    // turns that into a Denied error. (Auto would auto-allow it.)
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::Default,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(DenyAll),
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    };

    let err = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "echo nope"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)));
}

#[tokio::test]
async fn todo_write_folds_text_and_json_content() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let result = reg
        .get("todo_write")
        .unwrap()
        .call(
            json!({"todos": [
                {"content": "first", "status": "completed"},
                {"content": "second", "status": "in_progress"}
            ]}),
            &cx,
        )
        .await
        .unwrap();

    assert_eq!(result.content.len(), 2);
    assert!(!result.is_error);
    let folded = result.content_text();
    assert!(folded.contains("updated 2 todo(s)"));
    assert!(folded.contains("\"content\":\"first\""));
    assert!(folded.contains("\"status\":\"in_progress\""));
}

#[tokio::test]
async fn todo_write_rejects_two_in_progress() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let err = reg
        .get("todo_write")
        .unwrap()
        .call(
            json!({"todos": [
                {"content": "a", "status": "in_progress"},
                {"content": "b", "status": "in_progress"}
            ]}),
            &cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn secret_directory_branch_denies_read_write_edit_under_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    std::fs::write(ssh.join("config"), "Host example\n  User me\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let read_err = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": ".ssh/config"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(read_err, ToolError::Denied(_)));

    let write_err = reg
        .get("write_file")
        .unwrap()
        .call(json!({"path": ".ssh/config", "content": "Host evil"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(write_err, ToolError::Denied(_)));
    assert_eq!(
        std::fs::read_to_string(ssh.join("config")).unwrap(),
        "Host example\n  User me\n"
    );

    let edit_err = reg
        .get("edit_file")
        .unwrap()
        .call(json!({"path": ".ssh/config", "old_string": "me", "new_string": "you"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(edit_err, ToolError::Denied(_)));
}

#[tokio::test]
async fn secret_directory_branch_denies_under_aws_and_gnupg() {
    let dir = tempfile::tempdir().unwrap();
    let aws = dir.path().join(".aws");
    let gnupg = dir.path().join(".gnupg");
    std::fs::create_dir(&aws).unwrap();
    std::fs::create_dir(&gnupg).unwrap();
    std::fs::write(aws.join("config"), "[default]\nregion=x\n").unwrap();
    std::fs::write(gnupg.join("gpg.conf"), "use-agent\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let aws_err = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": ".aws/config"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(aws_err, ToolError::Denied(_)));

    let gnupg_err = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": ".gnupg/gpg.conf"}), &cx)
        .await
        .unwrap_err();
    assert!(matches!(gnupg_err, ToolError::Denied(_)));
}

#[tokio::test]
async fn search_tools_skip_secrets_in_subdirectories() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("config")).unwrap();
    std::fs::create_dir(dir.path().join("certs")).unwrap();
    std::fs::write(dir.path().join("config/.env"), "TOKEN=needle").unwrap();
    std::fs::write(dir.path().join("certs/server.pem"), "needle CERT").unwrap();
    std::fs::write(dir.path().join("config/app.rs"), "let x = \"needle\";").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let grep = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "needle"}), &cx)
        .await
        .unwrap();
    let grep_text = grep.content_text();
    assert!(grep_text.contains("app.rs"));
    assert!(!grep_text.contains(".env"));
    assert!(!grep_text.contains("server.pem"));

    let glob = reg
        .get("glob")
        .unwrap()
        .call(json!({"pattern": "**/*"}), &cx)
        .await
        .unwrap();
    let glob_text = glob.content_text();
    assert!(glob_text.contains("app.rs"));
    assert!(!glob_text.contains(".env"));
    assert!(!glob_text.contains("server.pem"));

    let list = reg
        .get("list_dir")
        .unwrap()
        .call(json!({"path": "config"}), &cx)
        .await
        .unwrap();
    let list_text = list.content_text();
    assert!(list_text.contains("app.rs"));
    assert!(!list_text.contains(".env"));

    let list_certs = reg
        .get("list_dir")
        .unwrap()
        .call(json!({"path": "certs"}), &cx)
        .await
        .unwrap();
    assert_eq!(list_certs.content_text(), "empty");
}

#[tokio::test]
async fn compound_bash_resolves_to_most_restrictive_decision_at_tool_level() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Allow);
    let marker = dir.path().join("marker.txt");
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::Auto,
        live_mode: None,
        rules: Arc::new(RuleSet::from_lists(
            &["Bash(echo *)".into()],
            &[],
            &["Bash(rm *)".into()],
        )),
        approver: approver.clone(),
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    };

    let err = reg
        .get("bash")
        .unwrap()
        .call(
            json!({"command": format!("echo ok > {} && rm -rf /", marker.display())}),
            &cx,
        )
        .await
        .unwrap_err();

    assert!(matches!(err, ToolError::Denied(_)));
    assert!(approver.kinds().is_empty());
    assert!(!marker.exists());
}

#[tokio::test]
async fn edit_multi_line_replacement_diff_body() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let result = reg
        .get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "beta\ngamma", "new_string": "BETA\nDELTA\nEPSILON"}),
            &cx,
        )
        .await
        .unwrap();

    let text = result.content_text();
    assert!(text.contains("-beta"));
    assert!(text.contains("-gamma"));
    assert!(text.contains("+BETA"));
    assert!(text.contains("+DELTA"));
    assert!(text.contains("+EPSILON"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\nBETA\nDELTA\nEPSILON\n"
    );
}

#[tokio::test]
async fn edit_add_only_diff_body() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "head\ntail\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let result = reg
        .get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "head\n", "new_string": "head\ninserted\n"}),
            &cx,
        )
        .await
        .unwrap();

    let text = result.content_text();
    assert!(text.contains("+inserted"));
    assert!(!text.contains("-head"));
    assert!(!text.contains("-tail"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "head\ninserted\ntail\n"
    );
}

#[tokio::test]
async fn edit_delete_only_diff_body() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "keep\ndrop\nkeep2\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let result = reg
        .get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "drop\n", "new_string": ""}),
            &cx,
        )
        .await
        .unwrap();

    let text = result.content_text();
    assert!(text.contains("-drop"));
    assert!(!text.contains("+drop"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "keep\nkeep2\n"
    );
}

#[tokio::test]
async fn edit_replace_all_diff_body_shows_every_change() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "foo\nmid\nfoo\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let result = reg
        .get("edit_file")
        .unwrap()
        .call(
            json!({"path": "a.txt", "old_string": "foo", "new_string": "bar", "replace_all": true}),
            &cx,
        )
        .await
        .unwrap();

    let text = result.content_text();
    assert!(text.contains("2 replacement(s)"));
    assert_eq!(text.matches("-foo").count(), 2);
    assert_eq!(text.matches("+bar").count(), 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "bar\nmid\nbar\n"
    );
}

#[tokio::test]
async fn read_rejects_file_over_max_read_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let big = "x".repeat(256 * 1024 + 1);
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let err = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "big.txt"}), &cx)
        .await
        .unwrap_err();
    match err {
        ToolError::Execution(msg) => assert!(msg.contains("limit"), "got {msg}"),
        other => panic!("expected execution error, got {other:?}"),
    }
}

#[tokio::test]
async fn read_offset_and_limit_slice_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let offset_only = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "a.txt", "offset": 3}), &cx)
        .await
        .unwrap();
    assert_eq!(offset_only.content_text(), "l3\nl4\nl5");

    let offset_limit = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "a.txt", "offset": 2, "limit": 2}), &cx)
        .await
        .unwrap();
    assert_eq!(offset_limit.content_text(), "l2\nl3");

    let limit_only = reg
        .get("read_file")
        .unwrap()
        .call(json!({"path": "a.txt", "limit": 1}), &cx)
        .await
        .unwrap();
    assert_eq!(limit_only.content_text(), "l1");
}

#[tokio::test]
async fn glob_and_list_dir_gate_outside_project_reads() {
    // The project is `proj`; an out-of-project sibling dir must require approval
    // for both glob and list_dir (previously they walked it with no gate). Tested
    // in AcceptEdits, which gates out-of-project reads — Auto auto-approves reads
    // under policy A, so it would not exercise the gate path here.
    let proj = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret-topology.txt"), "x").unwrap();
    let reg = ToolRegistry::builtins();

    // DenyAll: the outside-project Read escalates to Ask, the approver denies.
    let denied = Recording::new(Decision::Deny);
    let cx = cx_with(proj.path(), PermissionMode::AcceptEdits, denied.clone());

    let glob = reg
        .get("glob")
        .unwrap()
        .call(
            json!({"pattern": "**/*", "path": outside.path().to_str().unwrap()}),
            &cx,
        )
        .await;
    assert!(matches!(glob, Err(ToolError::Denied(_))), "glob must be gated outside the project: {glob:?}");

    let list = reg
        .get("list_dir")
        .unwrap()
        .call(json!({"path": outside.path().to_str().unwrap()}), &cx)
        .await;
    assert!(matches!(list, Err(ToolError::Denied(_))), "list_dir must be gated outside the project: {list:?}");

    assert_eq!(denied.kinds(), vec!["outside_project", "outside_project"]);
}

#[tokio::test]
async fn bash_truncates_large_non_ascii_output_without_panic() {
    // >30000 bytes of a 3-byte codepoint guarantees the byte cap lands mid-char;
    // the char-boundary clamp must not panic the tool task.
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let out = reg
        .get("bash")
        .unwrap()
        // 20000 × "가" (3 bytes) = 60000 bytes of multibyte output.
        .call(json!({"command": "for i in $(seq 1 20000); do printf '가'; done"}), &cx)
        .await
        .expect("bash must not panic on a mid-codepoint truncation");
    assert!(out.truncated, "output should be marked truncated");
    assert!(out.content_text().contains('가'));
}

#[tokio::test]
async fn symlinked_secret_read_write_edit_are_refused() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("id_rsa");
    std::fs::write(&secret, "PRIVATE").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&secret, dir.path().join("notes.txt")).unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    for (tool, args) in [
        ("read_file", json!({"path": "notes.txt"})),
        ("write_file", json!({"path": "notes.txt", "content": "overwrite"})),
        ("edit_file", json!({"path": "notes.txt", "old_string": "PRIVATE", "new_string": "x"})),
    ] {
        let err = reg.get(tool).unwrap().call(args, &cx).await.unwrap_err();
        match err {
            ToolError::Denied(msg) => assert!(msg.contains("secret"), "{tool}: got {msg}"),
            other => panic!("{tool}: expected secret refusal, got {other:?}"),
        }
    }
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "PRIVATE");
}

#[tokio::test]
async fn write_through_symlinked_secret_dir_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    let ssh = outside.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&ssh, dir.path().join("keys")).unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let err = reg
        .get("write_file")
        .unwrap()
        .call(json!({"path": "keys/new_key_material", "content": "k"}), &cx)
        .await
        .unwrap_err();
    match err {
        ToolError::Denied(msg) => assert!(msg.contains("secret"), "got {msg}"),
        other => panic!("expected secret refusal, got {other:?}"),
    }
    assert!(!ssh.join("new_key_material").exists());
}

#[tokio::test]
async fn case_insensitive_secret_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    for path in [".ENV", ".Env.Production", "Id_Rsa", "Server.PEM"] {
        let err = reg
            .get("read_file")
            .unwrap()
            .call(json!({"path": path}), &cx)
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => assert!(msg.contains("secret"), "{path}: got {msg}"),
            other => panic!("{path}: expected secret refusal, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn expanded_denylist_entries_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    for path in [
        ".git-credentials",
        ".kube/config",
        ".pgpass",
        ".dockercfg",
        ".docker/config.json",
        ".terraformrc",
        ".pypirc",
        ".htpasswd",
        ".netrc",
        ".config/gh/hosts.yml",
    ] {
        let err = reg
            .get("read_file")
            .unwrap()
            .call(json!({"path": path}), &cx)
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => assert!(msg.contains("secret"), "{path}: got {msg}"),
            other => panic!("{path}: expected secret refusal, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn search_tools_skip_symlinked_secrets() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("id_rsa");
    std::fs::write(&secret, "needle SECRETDATA").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&secret, dir.path().join("benign.txt")).unwrap();
    std::fs::write(dir.path().join("normal.txt"), "needle here").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(AllowAll));

    let grep = reg
        .get("grep")
        .unwrap()
        .call(json!({"pattern": "needle"}), &cx)
        .await
        .unwrap();
    let grep_text = grep.content_text();
    assert!(grep_text.contains("normal.txt"));
    assert!(!grep_text.contains("benign.txt"), "grep must not read through the symlink: {grep_text}");
    assert!(!grep_text.contains("SECRETDATA"));

    let glob = reg
        .get("glob")
        .unwrap()
        .call(json!({"pattern": "*"}), &cx)
        .await
        .unwrap();
    let glob_text = glob.content_text();
    assert!(glob_text.contains("normal.txt"));
    assert!(!glob_text.contains("benign.txt"), "glob must hide the symlinked secret: {glob_text}");

    let list = reg
        .get("list_dir")
        .unwrap()
        .call(json!({}), &cx)
        .await
        .unwrap();
    let list_text = list.content_text();
    assert!(list_text.contains("normal.txt"));
    assert!(!list_text.contains("benign.txt"), "list_dir must hide the symlinked secret: {list_text}");
}

#[tokio::test]
async fn bash_runs_without_login_shell() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let out = reg
        .get("bash")
        .unwrap()
        .call(
            json!({"command": "shopt -q login_shell && echo yes-login || echo no-login"}),
            &cx,
        )
        .await
        .unwrap();
    let text = out.content_text();
    assert!(text.contains("no-login"), "bash must run non-login: {text}");
    assert!(!text.contains("yes-login"), "bash must not source the login profile: {text}");
}

#[tokio::test]
async fn bash_command_touching_tilde_secret_path_is_refused_without_running() {
    let home = tempfile::tempdir().unwrap();
    let ssh = home.path().join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    std::fs::write(ssh.join("id_rsa"), "PRIVATE").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let approver = Recording::new(Decision::Allow);
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: Some(home.path().to_path_buf()),
        mode: PermissionMode::Auto,
        live_mode: None,
        rules: Arc::new(RuleSet::from_lists(&["Bash(*)".into()], &[], &[])),
        approver: approver.clone(),
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    };

    let err = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "cp ~/.ssh/id_rsa stolen.txt"}), &cx)
        .await
        .unwrap_err();
    match err {
        ToolError::Denied(msg) => assert!(msg.contains("secret"), "got {msg}"),
        other => panic!("expected secret refusal, got {other:?}"),
    }
    assert!(!dir.path().join("stolen.txt").exists(), "the command must not have run");
    assert!(approver.kinds().is_empty(), "refusal must happen before any approval prompt");
}

#[tokio::test]
async fn bash_command_with_relative_and_absolute_secret_paths_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("id_rsa");
    std::fs::write(&secret, "PRIVATE").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    for command in [
        "cat ./.env".to_string(),
        format!("head {}", secret.display()),
        "grep key ../.aws/credentials".to_string(),
    ] {
        let err = reg
            .get("bash")
            .unwrap()
            .call(json!({"command": command}), &cx)
            .await
            .unwrap_err();
        match err {
            ToolError::Denied(msg) => assert!(msg.contains("secret"), "{command}: got {msg}"),
            other => panic!("{command}: expected secret refusal, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn bash_command_reading_secret_through_symlink_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("id_rsa");
    std::fs::write(&secret, "PRIVATE").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&secret, dir.path().join("notes.txt")).unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let err = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "cat ./notes.txt"}), &cx)
        .await
        .unwrap_err();
    match err {
        ToolError::Denied(msg) => assert!(msg.contains("secret"), "got {msg}"),
        other => panic!("expected secret refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_untokenizable_command_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let err = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "echo \"unterminated"}), &cx)
        .await
        .unwrap_err();
    match err {
        ToolError::Denied(msg) => assert!(msg.contains("tokenized"), "got {msg}"),
        other => panic!("expected fail-closed refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_benign_paths_still_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let out = reg
        .get("bash")
        .unwrap()
        .call(json!({"command": "cat src/main.rs"}), &cx)
        .await
        .unwrap();
    assert!(out.content_text().contains("fn main"));
    assert!(!out.is_error);
}

#[tokio::test]
async fn apply_patch_adds_updates_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("update.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(dir.path().join("gone.txt"), "obsolete\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    // Built line-by-line so leading-space context lines survive.
    let patch = [
        "*** Begin Patch",
        "*** Add File: new.txt",
        "+fresh content",
        "*** Update File: update.txt",
        "@@",
        " alpha",
        "-beta",
        "+BETA",
        " gamma",
        "*** Delete File: gone.txt",
        "*** End Patch",
    ]
    .join("\n");

    let out = reg
        .get("apply_patch")
        .unwrap()
        .call(json!({ "patch": patch }), &cx)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content_text());

    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "fresh content\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("update.txt")).unwrap(),
        "alpha\nBETA\ngamma\n"
    );
    assert!(!dir.path().join("gone.txt").exists(), "delete removed the file");
}

#[tokio::test]
async fn apply_patch_add_refuses_to_clobber_an_existing_file() {
    // `Add File` over an existing path would silently destroy its contents (the
    // approval diff would show an empty `old`). It must be rejected, leaving the
    // file untouched, so the model uses `Update File` instead.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("exists.txt"), "precious\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let patch = ["*** Begin Patch", "*** Add File: exists.txt", "+overwrite", "*** End Patch"].join("\n");
    let err = reg
        .get("apply_patch")
        .unwrap()
        .call(json!({ "patch": patch }), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArgs(_)), "Add over existing is rejected");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("exists.txt")).unwrap(),
        "precious\n",
        "the existing file is untouched"
    );
}

#[tokio::test]
async fn apply_patch_renames_with_move_to() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("old.rs"), "fn a() {}\n").unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let patch = [
        "*** Begin Patch",
        "*** Update File: old.rs",
        "*** Move to: new.rs",
        "@@",
        "-fn a() {}",
        "+fn b() {}",
        "*** End Patch",
    ]
    .join("\n");

    reg.get("apply_patch")
        .unwrap()
        .call(json!({ "patch": patch }), &cx)
        .await
        .unwrap();
    assert!(!dir.path().join("old.rs").exists(), "original removed");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.rs")).unwrap(),
        "fn b() {}\n"
    );
}

#[tokio::test]
async fn apply_patch_refuses_secret_files() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let patch = [
        "*** Begin Patch",
        "*** Add File: .env",
        "+SECRET=1",
        "*** End Patch",
    ]
    .join("\n");

    let err = reg
        .get("apply_patch")
        .unwrap()
        .call(json!({ "patch": patch }), &cx)
        .await
        .unwrap_err();
    match err {
        ToolError::Denied(msg) => assert!(msg.contains("secret"), "got {msg}"),
        other => panic!("expected secret refusal, got {other:?}"),
    }
    assert!(!dir.path().join(".env").exists(), "nothing written on refusal");
}

#[tokio::test]
async fn apply_patch_writes_nothing_when_a_change_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    // Default mode → writes ask; DenyAll refuses, so the patch is gated out
    // before phase 3 ever writes.
    let cx = cx_with(dir.path(), PermissionMode::Default, Arc::new(DenyAll));

    let patch = [
        "*** Begin Patch",
        "*** Add File: created.txt",
        "+nope",
        "*** End Patch",
    ]
    .join("\n");

    let err = reg
        .get("apply_patch")
        .unwrap()
        .call(json!({ "patch": patch }), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
    assert!(
        !dir.path().join("created.txt").exists(),
        "a denied patch writes nothing"
    );
}

#[tokio::test]
async fn memory_write_appends_to_project_memory_file() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let cx = cx_with(dir.path(), PermissionMode::Auto, Arc::new(AllowAll));

    let r = reg
        .get("memory_write")
        .unwrap()
        .call(json!({"note": "build with cargo test"}), &cx)
        .await
        .unwrap();
    assert_eq!(r.content_text(), "remembered");
    let path = dir.path().join(".stepper/memory/MEMORY.md");
    let mem = std::fs::read_to_string(&path).unwrap();
    assert!(mem.contains("# Project memory"), "header written: {mem}");
    assert!(mem.contains("- build with cargo test"), "note appended: {mem}");

    // A second note appends without duplicating the header.
    reg.get("memory_write")
        .unwrap()
        .call(json!({"note": "lint: clippy -D warnings"}), &cx)
        .await
        .unwrap();
    let mem2 = std::fs::read_to_string(&path).unwrap();
    assert_eq!(mem2.matches("# Project memory").count(), 1, "header once: {mem2}");
    assert!(mem2.contains("- lint: clippy -D warnings"));

    // An empty note is rejected.
    assert!(reg.get("memory_write").unwrap().call(json!({"note": "  "}), &cx).await.is_err());
}

/// An approver whose `ask` returns a fixed option index, for the ask_user_question test.
struct Picks(Option<usize>);
#[async_trait]
impl Approver for Picks {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Deny
    }
    async fn ask(&self, _question: &str, _options: &[String]) -> Option<usize> {
        self.0
    }
}

#[tokio::test]
async fn ask_user_question_returns_the_picked_option_or_no_answer() {
    let dir = tempfile::tempdir().unwrap();
    let reg = ToolRegistry::builtins();
    let args = json!({ "question": "Which?", "options": ["alpha", "beta"] });

    // A pick comes back as the chosen option text.
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(Picks(Some(1))));
    let out = reg.get("ask_user_question").unwrap().call(args.clone(), &cx).await.unwrap();
    assert!(out.content_text().contains("beta"), "selected option returned: {:?}", out.content_text());

    // No answer (default approver / dismissed) → a proceed-anyway message, not an error.
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(Picks(None)));
    let out = reg.get("ask_user_question").unwrap().call(args.clone(), &cx).await.unwrap();
    assert!(out.content_text().to_lowercase().contains("did not answer"));

    // Fewer than 2 options is rejected.
    let cx = cx_with(dir.path(), PermissionMode::AcceptEdits, Arc::new(Picks(Some(0))));
    assert!(reg.get("ask_user_question").unwrap().call(json!({"question":"q","options":["only"]}), &cx).await.is_err());
}
