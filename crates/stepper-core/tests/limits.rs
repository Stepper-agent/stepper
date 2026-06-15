//! `SessionLimits` enforcement: a FakeProvider that never stops asking for
//! tools must be cut off by `--max-turns` (total ReAct steps for the turn) and
//! by `--max-budget-usd` (accumulated session cost), each surfacing as a clear
//! error instead of a silent cancellation. The caps ride the real
//! `Orchestrator::run_turn`/`spawn_core` paths.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stepper_core::{
    CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver, SessionLimits,
    StepDef,
};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason, Usage,
};
use stepper_tools::{Approval, Approver, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct LoopingProvider {
    calls: Arc<AtomicUsize>,
    finish_after: Option<usize>,
    usage_input: u64,
}

#[async_trait]
impl LlmProvider for LoopingProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-m"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let usage = ChatEvent::Usage(Usage {
            input: self.usage_input,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        });
        let script = if self.finish_after.is_some_and(|cap| n + 1 >= cap) {
            vec![
                usage,
                ChatEvent::TextDelta("done".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        } else {
            vec![
                usage,
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: format!("c{n}"),
                    name: "list_dir".into(),
                    input: serde_json::json!({ "path": "." }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct LoopingResolver {
    calls: Arc<AtomicUsize>,
    finish_after: Option<usize>,
    usage_input: u64,
    input_per_mtok: f64,
}

impl ProviderResolver for LoopingResolver {
    fn resolve(&self, _model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        Ok(Box::new(LoopingProvider {
            calls: self.calls.clone(),
            finish_after: self.finish_after,
            usage_input: self.usage_input,
        }))
    }
    fn model_info(&self, _model_ref: &str) -> ModelInfo {
        ModelInfo {
            context_window: 200_000,
            max_output_tokens: 0,
            input_per_mtok: self.input_per_mtok,
            output_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
            cache_write_per_mtok: 0.0,
            estimated: false,
        }
    }
}

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Allow
    }
}

fn step(cap: usize) -> StepDef {
    StepDef {
        name: "solo".into(),
        model_ref: "fake/fake-m".into(),
        system_prompt: "work".into(),
        tool_allow: Vec::new(),
        tool_deny: Vec::new(),
        mcp_allow: Vec::new(),
        step_cap: cap,
        color: None,
        on_failure: FailurePolicy::Stop,
        retries: 0,
        temperature: None,
        top_p: None,
        permission: Vec::new(),
        parallel: false,
        parallel_max: 8,
        skills: Vec::new(),
    }
}

fn orchestrator(
    resolver: Arc<dyn ProviderResolver>,
    root: std::path::PathBuf,
    limits: SessionLimits,
) -> Orchestrator {
    Orchestrator {
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![step(40)],
        base_context: String::new(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(RuleSet::default()),
        mode: PermissionMode::AcceptEdits,
        hooks: Arc::new(HookHost::empty(root)),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        limits,
        fallback_model: None,
        resume_seed: Vec::new(),
    }
}

fn drain_events() -> (
    mpsc::Sender<stepper_protocol::AppEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::channel(256);
    let handle = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    (tx, handle)
}

#[tokio::test]
async fn max_turns_cap_aborts_a_runaway_turn() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(LoopingResolver {
        calls: calls.clone(),
        finish_after: None,
        usage_input: 1_000,
        input_per_mtok: 0.0,
    });
    let orch = orchestrator(
        resolver,
        dir.path().to_path_buf(),
        SessionLimits::new(Some(3), None),
    );
    let (tx, _drain) = drain_events();

    let err = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap_err();

    match err {
        CoreError::MaxTurnsExceeded { cap } => assert_eq!(cap, 3),
        other => panic!("expected MaxTurnsExceeded, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "exactly max-turns provider requests were admitted"
    );
}

#[tokio::test]
async fn budget_cap_aborts_once_session_cost_reaches_it() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    // $3 per step (1M input tokens at $3/MTok); cap $4 admits two steps.
    let resolver = Arc::new(LoopingResolver {
        calls: calls.clone(),
        finish_after: None,
        usage_input: 1_000_000,
        input_per_mtok: 3.0,
    });
    let orch = orchestrator(
        resolver,
        dir.path().to_path_buf(),
        SessionLimits::new(None, Some(4.0)),
    );
    let (tx, _drain) = drain_events();

    let err = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap_err();

    match err {
        CoreError::BudgetExceeded { cap, spent } => {
            assert_eq!(cap, 4.0);
            assert!(spent >= cap, "spent {spent} must have reached the cap");
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn budget_accumulates_across_turns_in_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    // Each turn is a single $3 request; the $5 session budget survives turn one
    // and trips on turn two's first step.
    let resolver = Arc::new(LoopingResolver {
        calls: calls.clone(),
        finish_after: Some(1),
        usage_input: 1_000_000,
        input_per_mtok: 3.0,
    });
    let limits = SessionLimits::new(None, Some(5.0));
    let orch = orchestrator(resolver, dir.path().to_path_buf(), limits);
    let (tx, _drain) = drain_events();

    orch.run_turn("one".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .expect("the first turn fits the budget");

    // Admission happens before each request: $3 spent < $5 lets turn two run
    // (total $6); turn three must then be refused outright.
    orch.run_turn("two".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .expect("the second turn still fits");

    let err = orch
        .run_turn("three".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::BudgetExceeded { .. }),
        "expected BudgetExceeded, got {err:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2, "turn three admitted no request");
}

#[tokio::test]
async fn caps_leave_a_turn_within_limits_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(LoopingResolver {
        calls: calls.clone(),
        finish_after: Some(2),
        usage_input: 1_000,
        input_per_mtok: 3.0,
    });
    let orch = orchestrator(
        resolver,
        dir.path().to_path_buf(),
        SessionLimits::new(Some(10), Some(50.0)),
    );
    let (tx, _drain) = drain_events();

    let summaries = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .expect("a turn within the caps completes normally")
        .summaries;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].1, "done");
}

#[tokio::test]
async fn cap_error_surfaces_as_an_error_event_through_spawn_core() {
    use stepper_core::{spawn_core, SessionRecord};
    use stepper_protocol::{Action, AppEvent};

    let dir = tempfile::tempdir().unwrap();
    let resolver = Arc::new(LoopingResolver {
        calls: Arc::new(AtomicUsize::new(0)),
        finish_after: None,
        usage_input: 1_000,
        input_per_mtok: 0.0,
    });
    let orch = orchestrator(
        resolver,
        dir.path().to_path_buf(),
        SessionLimits::new(Some(2), None),
    );
    let (action_tx, action_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let mut event_rx = spawn_core(orch, SessionRecord::fresh(), action_rx, cancel);

    action_tx.send(Action::SubmitInput("go".into())).await.unwrap();

    let mut saw_cap_error = false;
    while let Some(event) = event_rx.recv().await {
        match event {
            AppEvent::Error(text) => {
                assert!(
                    text.contains("--max-turns"),
                    "the error names the tripped cap: {text}"
                );
                saw_cap_error = true;
            }
            AppEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(saw_cap_error, "the cap abort must surface as an Error event");
    let _ = action_tx.send(Action::Quit).await;
}
