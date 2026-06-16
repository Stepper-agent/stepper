//! Built-in slash commands (`/help`, `/clear`, `/compact`, `/context`, `/cost`,
//! `/model`, `/permissions`, `/resume`, `/rewind`) through the real `spawn_core`
//! loop: they emit their own events and never run an agent turn.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use stepper_core::{
    spawn_core, CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver,
    SessionRecord, SessionStore, StepDef, TurnRecord,
};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{Action, AppEvent, EventRx};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, Message, ProviderError, StopReason, Usage,
};
use stepper_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct DummyProvider {
    model: String,
}

#[async_trait]
impl LlmProvider for DummyProvider {
    fn provider(&self) -> &str {
        "solo"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

struct Resolver;

impl ProviderResolver for Resolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        // Two valid models; anything else is unknown (so /model rejects it).
        match model_ref {
            "solo/m" => Ok(Box::new(DummyProvider { model: "m".into() })),
            "solo/big" => Ok(Box::new(DummyProvider {
                model: "big".into(),
            })),
            other => Err(CoreError::NoModel(other.to_string())),
        }
    }
    fn model_info(&self, _model_ref: &str) -> ModelInfo {
        ModelInfo {
            context_window: 123_456,
            max_output_tokens: 0,
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
            cache_write_per_mtok: 0.0,
            estimated: false,
        }
    }
}

fn step() -> StepDef {
    StepDef {
        name: "solo".into(),
        model_ref: "solo/m".into(),
        system_prompt: "s".into(),
        tool_allow: Vec::new(),
        tool_deny: Vec::new(),
        mcp_allow: Vec::new(),
        step_cap: 5,
        color: None,
        on_failure: FailurePolicy::Stop,
        retries: 0,
        temperature: None,
        top_p: None,
        reasoning_effort: None,
        thinking_budget: None,
        permission: Vec::new(),
        parallel: false,
        parallel_max: 8,
        skills: Vec::new(),
    }
}

fn orchestrator(root: std::path::PathBuf) -> Orchestrator {
    Orchestrator {
        resolver: Arc::new(Resolver),
        base_tools: ToolRegistry::builtins(),
        steps: vec![step()],
        base_context: "ctx".into(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(std::sync::RwLock::new(RuleSet::default())),
        mode: Arc::new(std::sync::RwLock::new(PermissionMode::AcceptEdits)),
        hooks: Arc::new(HookHost::empty(root)),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,        limits: stepper_core::SessionLimits::default(),
        fallback_model: None,
        resume_seed: Vec::new(),
    }
}

fn slash(name: &str, args: &str) -> Action {
    Action::SlashCommand {
        name: name.into(),
        args: args.into(),
    }
}

async fn next_context_breakdown(rx: &mut EventRx) -> stepper_protocol::ContextBreakdownView {
    while let Some(ev) = rx.recv().await {
        if let AppEvent::ContextBreakdown(view) = ev {
            return view;
        }
    }
    panic!("event stream ended before a ContextBreakdown");
}

async fn next_permissions(rx: &mut EventRx) -> stepper_protocol::PermissionsSnapshotView {
    while let Some(ev) = rx.recv().await {
        if let AppEvent::PermissionsSnapshot(v) = ev {
            return v;
        }
    }
    panic!("event stream ended before a PermissionsSnapshot");
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_allow_adds_persists_and_rejects_malformed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".stepper")).unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    // /allow folds the rule live and persists it; the snapshot reflects it.
    action_tx.send(slash("allow", "Bash(cargo *)")).await.unwrap();
    let snap = next_permissions(&mut events).await;
    assert!(
        snap.rules.iter().any(|r| r.verdict == "allow" && r.rule == "Bash(cargo *)"),
        "snapshot shows the new allow rule: {:?}",
        snap.rules
    );
    let written = std::fs::read_to_string(dir.path().join(".stepper/setting.json")).unwrap();
    assert!(written.contains("Bash(cargo *)"), "persisted: {written}");

    // A malformed spec is rejected with a warning and not written.
    action_tx.send(slash("deny", "Bash(rm")).await.unwrap();
    let warn = next_notice(&mut events).await;
    assert!(warn.contains("malformed"), "got: {warn}");
}

async fn next_notice(rx: &mut EventRx) -> String {
    while let Some(ev) = rx.recv().await {
        match ev {
            AppEvent::Notice { text, .. } => return text,
            AppEvent::ModelChanged(_) => continue,
            _ => continue,
        }
    }
    panic!("event stream ended before a Notice");
}

#[tokio::test(flavor = "multi_thread")]
async fn builtins_emit_events_without_running_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    // /help → a Notice; crucially no TurnStarted (built-ins don't run a turn).
    action_tx.send(slash("help", "")).await.unwrap();
    let help = next_notice(&mut events).await;
    assert!(help.contains("/help"), "help lists commands: {help}");

    // /context → a structured breakdown whose limit is the model's window.
    action_tx.send(slash("context", "")).await.unwrap();
    let breakdown = next_context_breakdown(&mut events).await;
    assert_eq!(breakdown.context_limit, 123456, "got: {breakdown:?}");

    // /model with a valid ref → ModelChanged then a confirming Notice.
    action_tx.send(slash("model", "solo/big")).await.unwrap();
    let switched = wait_model_then_notice(&mut events).await;
    assert!(switched.contains("solo/big"), "got: {switched}");

    // /model with an invalid ref → a warning Notice (no switch).
    action_tx.send(slash("model", "bad/nope")).await.unwrap();
    let warn = next_notice(&mut events).await;
    assert!(warn.contains("cannot switch"), "got: {warn}");
}

#[tokio::test(flavor = "multi_thread")]
async fn interactive_bang_shell_runs_permissionless_without_approval() {
    // The orchestrator helper runs in AcceptEdits with no allow rules, so the
    // model's bash tool WOULD gate `echo` (Ask). A hand-typed `!echo` is
    // user-initiated and must run permissionless — no ApprovalRequested, no
    // "shell: … not permitted" error.
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx
        .send(Action::RunShell("echo perm_marker_42".into()))
        .await
        .unwrap();

    let mut saw_output = false;
    while let Some(ev) = events.recv().await {
        match ev {
            AppEvent::ApprovalRequested(_) => {
                panic!("interactive `!` must not prompt for approval")
            }
            AppEvent::AssistantTokenDelta(t) if t.contains("perm_marker_42") => saw_output = true,
            AppEvent::Notice { text, .. } if text.starts_with("shell:") => {
                panic!("`!echo` should run, not error: {text}")
            }
            AppEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(saw_output, "the `!echo` output should stream back as assistant text");
}

#[tokio::test(flavor = "multi_thread")]
async fn interactive_bang_shell_still_refuses_secret_files() {
    // Permissionless does NOT mean unguarded: the bash secret-file screen runs
    // before execution regardless of mode, so `!cat ~/.ssh/id_rsa` is refused.
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx
        .send(Action::RunShell("cat ~/.ssh/id_rsa".into()))
        .await
        .unwrap();

    let mut refused = false;
    while let Some(ev) = events.recv().await {
        match ev {
            AppEvent::Notice { text, .. } if text.contains("secret") => refused = true,
            AppEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(refused, "a `!` touching a secret file must be refused even permissionless");
}

#[tokio::test(flavor = "multi_thread")]
async fn scaffold_layer_command_writes_the_default_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(root.clone()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx.send(slash("scaffold-layer", "")).await.unwrap();
    let note = next_notice(&mut events).await;
    assert!(note.contains("plan"), "scaffold notice names the pipeline: {note}");
    for name in ["plan", "implement", "review"] {
        assert!(
            root.join(".stepper/layer").join(name).join("index.md").exists(),
            "{name} layer file written"
        );
    }
    let setting = std::fs::read_to_string(root.join(".stepper/setting.json")).unwrap_or_default();
    assert!(setting.contains("plan"), "step pipeline set in setting.json: {setting}");
}

#[tokio::test(flavor = "multi_thread")]
async fn new_layer_command_rejects_unsafe_names() {
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx.send(slash("layer", "../escape")).await.unwrap();
    let note = next_notice(&mut events).await;
    assert!(note.contains("usage"), "an unsafe name is refused: {note}");
    assert!(
        !dir.path().join(".stepper/layer/../escape").exists(),
        "nothing is written for an unsafe name"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn background_bang_shell_streams_output_and_exits_without_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    // `!echo … &` runs detached: no turn, but ProcessStarted/Output/Exited stream.
    action_tx
        .send(Action::RunShell("echo bg_marker_77 &".into()))
        .await
        .unwrap();

    let (mut started, mut output, mut exited) = (false, false, false);
    let collect = async {
        while let Some(ev) = events.recv().await {
            match ev {
                AppEvent::ProcessStarted { command, .. } => {
                    assert!(command.contains("echo bg_marker_77"), "got: {command}");
                    started = true;
                }
                AppEvent::ProcessOutput { line, .. } if line.contains("bg_marker_77") => {
                    output = true;
                }
                AppEvent::ProcessExited { code, .. } => {
                    assert_eq!(code, Some(0));
                    exited = true;
                }
                AppEvent::TurnStarted { .. } => panic!("a background `!cmd &` must not open a turn"),
                _ => {}
            }
            if started && output && exited {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), collect).await;
    assert!(
        started && output && exited,
        "started={started} output={output} exited={exited}"
    );
}

async fn wait_model_then_notice(rx: &mut EventRx) -> String {
    let mut saw_model = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AppEvent::ModelChanged(m) => {
                assert_eq!(m.model, "big");
                saw_model = true;
            }
            AppEvent::Notice { text, .. } => {
                assert!(saw_model, "ModelChanged must precede the confirmation notice");
                return text;
            }
            AppEvent::TurnStarted { .. } => panic!("a built-in must not start a turn"),
            _ => {}
        }
    }
    panic!("event stream ended before the model-switch notice");
}

// ── /cost · /compact · /permissions · /rewind · /resume ──

/// Streams one text reply plus a fixed usage frame — a successful, billable turn.
struct UsageProvider {
    usage: Usage,
}

#[async_trait]
impl LlmProvider for UsageProvider {
    fn provider(&self) -> &str {
        "solo"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let script = vec![
            ChatEvent::TextDelta("ok".into()),
            ChatEvent::Usage(self.usage),
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// Records the summarize request's system prompt and answers with a fixed
/// summary — the `/compact` summarizer.
struct SummaryProvider {
    seen_systems: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmProvider for SummaryProvider {
    fn provider(&self) -> &str {
        "solo"
    }
    fn model(&self) -> &str {
        "sum"
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.seen_systems
            .lock()
            .unwrap()
            .push(request.system.unwrap_or_default());
        let script = vec![
            ChatEvent::TextDelta("FOCUSED SUMMARY".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// `solo/m` → priced UsageProvider, `solo/sum` → the recording summarizer.
struct PricedResolver {
    usage: Usage,
    seen_systems: Arc<Mutex<Vec<String>>>,
}

impl ProviderResolver for PricedResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "solo/m" => Ok(Box::new(UsageProvider { usage: self.usage })),
            "solo/sum" => Ok(Box::new(SummaryProvider {
                seen_systems: self.seen_systems.clone(),
            })),
            other => Err(CoreError::NoModel(other.to_string())),
        }
    }
    fn model_info(&self, _model_ref: &str) -> ModelInfo {
        ModelInfo {
            context_window: 200_000,
            max_output_tokens: 0,
            input_per_mtok: 2.0,
            output_per_mtok: 4.0,
            cache_read_per_mtok: 1.0,
            cache_write_per_mtok: 8.0,
            estimated: false,
        }
    }
}

fn priced_orchestrator(
    root: std::path::PathBuf,
    usage: Usage,
    seen_systems: Arc<Mutex<Vec<String>>>,
) -> Orchestrator {
    let mut orch = orchestrator(root);
    orch.resolver = Arc::new(PricedResolver { usage, seen_systems });
    orch.compaction_model = Some("solo/sum".into());
    orch
}

async fn wait_turn_complete(rx: &mut EventRx) {
    while let Some(ev) = rx.recv().await {
        if matches!(ev, AppEvent::TurnComplete { .. }) {
            return;
        }
    }
    panic!("event stream ended before TurnComplete");
}

#[tokio::test(flavor = "multi_thread")]
async fn cost_reports_session_and_last_turn_usage_with_usd() {
    let dir = tempfile::tempdir().unwrap();
    let usage = Usage {
        input: 1000,
        output: 500,
        cache_read: 200,
        cache_write: 100,
    };
    let orch = priced_orchestrator(dir.path().to_path_buf(), usage, Arc::default());
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orch,
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    // Before any turn: everything zero.
    action_tx.send(slash("cost", "")).await.unwrap();
    let zero = next_notice(&mut events).await;
    assert!(zero.contains("session 0 in · 0 out"), "got: {zero}");
    assert!(zero.contains("$0.0000"), "got: {zero}");

    // Two turns, each (1000 in × $2 + 500 out × $4 + 200 cr × $1 + 100 cw × $8)/Mtok = $0.0050.
    action_tx.send(Action::SubmitInput("one".into())).await.unwrap();
    wait_turn_complete(&mut events).await;
    action_tx.send(Action::SubmitInput("two".into())).await.unwrap();
    wait_turn_complete(&mut events).await;

    action_tx.send(slash("cost", "")).await.unwrap();
    let cost = next_notice(&mut events).await;
    assert!(
        cost.contains("session 2000 in · 1000 out · 400 cache-read · 200 cache-write"),
        "session usage sums across turns: {cost}"
    );
    assert!(cost.contains("$0.0100"), "session cost: {cost}");
    assert!(
        cost.contains("last turn 1000 in · 500 out · 200 cache-read · 100 cache-write"),
        "current-turn usage: {cost}"
    );
    assert!(cost.contains("$0.0050"), "last-turn cost: {cost}");
}

fn long_session(id: &str, messages: usize) -> SessionRecord {
    let msgs: Vec<Message> = (0..messages)
        .map(|i| {
            if i % 2 == 0 {
                Message::user(format!("user message {i} {}", "x".repeat(80)))
            } else {
                Message::assistant(format!("assistant message {i} {}", "y".repeat(80)))
            }
        })
        .collect();
    SessionRecord {
        id: id.into(),
        name: None,
        turns: vec![TurnRecord {
            user: "the long opening ask".into(),
            summaries: vec![("solo".into(), "did things".into())],
            messages: msgs,
        }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_folds_the_persisted_conversation_with_focus_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = SessionStore::new(&root);
    let session = long_session("to-compact", 12);
    let original = session.seed_messages();
    let before = stepper_core::compaction::estimate_tokens(&original);
    store.save(&session).unwrap();

    let seen_systems: Arc<Mutex<Vec<String>>> = Arc::default();
    let orch = priced_orchestrator(root.clone(), Usage::default(), seen_systems.clone());
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orch,
        store.load("to-compact").unwrap(),
        action_rx,
        CancellationToken::new(),
    );

    action_tx
        .send(slash("compact", "focus on file paths"))
        .await
        .unwrap();

    let mut saw_started = false;
    let freed = loop {
        match events.recv().await.expect("event stream stays open") {
            AppEvent::CompactionStarted => saw_started = true,
            AppEvent::CompactionDone { freed_tokens } => break freed_tokens,
            AppEvent::TurnStarted { .. } => panic!("/compact must not start a turn"),
            _ => {}
        }
    };
    assert!(saw_started, "CompactionStarted precedes CompactionDone");

    // The persisted session collapsed to one synthetic /compact turn whose
    // messages are the marker (with the model's summary) + the recent tail.
    let reloaded = store.load("to-compact").unwrap();
    assert_eq!(reloaded.turns.len(), 1);
    assert_eq!(reloaded.turns[0].user, "/compact");
    let compacted = &reloaded.turns[0].messages;
    assert_eq!(compacted.len(), 1 + 6, "marker + keep_recent tail: {compacted:#?}");
    assert!(compacted[0].text().contains("FOCUSED SUMMARY"), "got: {:?}", compacted[0]);
    assert!(compacted.last().unwrap().text().contains("assistant message 11"));

    // The freed figure is the honest before/after estimate delta.
    let after = stepper_core::compaction::estimate_tokens(compacted);
    assert_eq!(freed, before - after, "freed must be the real estimate delta");
    assert!(freed > 0);

    // The focus instructions ride into the summarizer's system prompt.
    {
        let systems = seen_systems.lock().unwrap();
        assert_eq!(systems.len(), 1, "the compaction provider summarized once");
        assert!(
            systems[0].contains("Focus on: focus on file paths"),
            "got: {}",
            systems[0]
        );
    }

    // The next turn is seeded with the compacted history, not the original.
    action_tx.send(slash("context", "")).await.unwrap();
    let breakdown = next_context_breakdown(&mut events).await;
    assert_eq!(breakdown.messages, after, "context sees the compacted size");
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_with_nothing_to_fold_is_an_honest_notice() {
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx.send(slash("compact", "")).await.unwrap();
    let text = next_notice(&mut events).await;
    assert!(text.contains("nothing to compact"), "got: {text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_snapshot_reports_mode_rules_with_sources_and_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let stepper_dir = root.join(".stepper");
    std::fs::create_dir_all(&stepper_dir).unwrap();
    std::fs::write(
        stepper_dir.join("setting.json"),
        r#"{
            "permissions": {
                "allow": ["Read(/**)", "Bash(pnpm *)"],
                "ask": ["Bash(git push:*)"],
                "deny": ["Bash(rm -rf *)"]
            },
            "approvals": [
                { "rule": "Bash(git status)", "scope": "git status", "grantedAt": "2026-06-01" }
            ]
        }"#,
    )
    .unwrap();

    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(root),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx.send(slash("permissions", "")).await.unwrap();

    let snapshot = loop {
        match events.recv().await.expect("event stream stays open") {
            AppEvent::PermissionsSnapshot(s) => break s,
            AppEvent::TurnStarted { .. } => panic!("/permissions must not start a turn"),
            _ => {}
        }
    };
    assert_eq!(snapshot.mode, "accept-edits");
    let find = |verdict: &str, rule: &str| {
        snapshot
            .rules
            .iter()
            .find(|r| r.verdict == verdict && r.rule == rule)
            .unwrap_or_else(|| panic!("missing {verdict} {rule}: {:?}", snapshot.rules))
    };
    assert_eq!(find("allow", "Read(/**)").source, "scaffold");
    assert_eq!(find("allow", "Bash(pnpm *)").source, "project");
    assert_eq!(find("ask", "Bash(git push:*)").source, "scaffold");
    assert_eq!(find("deny", "Bash(rm -rf *)").source, "scaffold");
    assert_eq!(snapshot.approvals.len(), 1);
    assert_eq!(snapshot.approvals[0].rule, "Bash(git status)");
    assert_eq!(snapshot.approvals[0].scope.as_deref(), Some("git status"));
    assert_eq!(snapshot.approvals[0].granted_at.as_deref(), Some("2026-06-01"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rewind_lists_checkpoints_newest_first_and_warns_when_none() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("a.txt"), "tracked").unwrap();
    let orch = priced_orchestrator(root, Usage::default(), Arc::default());
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orch,
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    // No turns yet → no checkpoints → an honest warning, not an empty picker.
    action_tx.send(slash("rewind", "")).await.unwrap();
    let none = next_notice(&mut events).await;
    assert!(none.contains("no checkpoints"), "got: {none}");

    action_tx.send(Action::SubmitInput("one".into())).await.unwrap();
    wait_turn_complete(&mut events).await;
    action_tx.send(Action::SubmitInput("two".into())).await.unwrap();
    wait_turn_complete(&mut events).await;

    action_tx.send(slash("rewind", "")).await.unwrap();
    let checkpoints = loop {
        match events.recv().await.expect("event stream stays open") {
            AppEvent::CheckpointList(list) => break list,
            AppEvent::TurnStarted { .. } => panic!("/rewind must not start a turn"),
            _ => {}
        }
    };
    let ids: Vec<&str> = checkpoints.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["turn-2", "turn-1"], "newest first");
    assert_eq!(checkpoints[0].turn, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_lists_sessions_and_action_resume_reseeds_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = SessionStore::new(&root);
    let mut prior = long_session("prior-session", 4);
    prior.name = Some("earlier work".into());
    store.save(&prior).unwrap();

    let orch = priced_orchestrator(root, Usage::default(), Arc::default());
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orch,
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    action_tx.send(slash("resume", "")).await.unwrap();
    let sessions = loop {
        match events.recv().await.expect("event stream stays open") {
            AppEvent::SessionList(list) => break list,
            AppEvent::TurnStarted { .. } => panic!("/resume must not start a turn"),
            _ => {}
        }
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "prior-session");
    assert_eq!(sessions[0].name.as_deref(), Some("earlier work"));
    assert_eq!(sessions[0].digest, "the long opening ask");
    assert_eq!(sessions[0].turns, 1);
    assert!(!sessions[0].age.is_empty());

    // Selecting it (the picker's Action::Resume) reseeds in-session.
    action_tx
        .send(Action::Resume {
            session_id: "prior-session".into(),
        })
        .await
        .unwrap();
    let (id, name, turns) = loop {
        if let AppEvent::SessionResumed { id, name, turns } =
            events.recv().await.expect("event stream stays open")
        {
            break (id, name, turns);
        }
    };
    assert_eq!(id, "prior-session");
    assert_eq!(name.as_deref(), Some("earlier work"));
    assert_eq!(turns, 1);

    // The resumed conversation is now the live seed: /context counts it.
    action_tx.send(slash("context", "")).await.unwrap();
    let breakdown = next_context_breakdown(&mut events).await;
    let expected = stepper_core::compaction::estimate_tokens(&prior.seed_messages());
    assert_eq!(breakdown.messages, expected);

    // An unknown id warns instead of clobbering the session.
    action_tx
        .send(Action::Resume {
            session_id: "missing".into(),
        })
        .await
        .unwrap();
    let warn = next_notice(&mut events).await;
    assert!(warn.contains("no session 'missing'"), "got: {warn}");
}

#[tokio::test]
async fn login_emits_an_api_key_prompt_for_the_named_provider() {
    let dir = tempfile::tempdir().unwrap();
    let orch = orchestrator(dir.path().to_path_buf());
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orch,
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    // `provider/model` is accepted; only the provider segment is used.
    action_tx.send(slash("login", "anthropic/claude-x")).await.unwrap();
    let provider = loop {
        match events.recv().await.expect("event stream stays open") {
            AppEvent::ApiKeyPrompt { provider } => break provider,
            AppEvent::TurnStarted { .. } => panic!("/login must not start a turn"),
            _ => {}
        }
    };
    assert_eq!(provider, "anthropic");
}
