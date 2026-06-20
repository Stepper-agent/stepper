//! The headless event-loop contract that `oneshot()` (the `stepper -p` path in
//! main.rs) relies on, driven through the real `spawn_core` with stub providers
//! — no production code is exercised differently than the real headless run.
//!
//! Covers the two behaviors the headless loop turns into stdout / exit code that
//! nothing else tests: (1) assistant tokens stream before `TurnComplete`, and
//! (2) in an asking mode a tool call raises `ApprovalRequested`, and replying
//! `Deny` (what the default headless path sends) makes the gate fail the tool.
//! The cap-error → non-zero-exit signal is already covered by tests/limits.rs.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stepper_core::{
    spawn_core, CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver,
    SessionRecord, StepDef,
};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{Action, AppEvent, ApprovalDecision};
use stepper_provider::{ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason};
use stepper_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Streams one text delta then ends the turn.
struct HelloProvider;
#[async_trait]
impl LlmProvider for HelloProvider {
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
            ChatEvent::TextDelta("hello".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// First step calls bash; once the tool result is threaded back, ends the turn
/// (so a denied tool terminates rather than looping to the step cap).
struct ScriptedBashProvider {
    calls: AtomicUsize,
}
#[async_trait]
impl LlmProvider for ScriptedBashProvider {
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
        let script = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "c".into(),
                    name: "bash".into(),
                    input: json!({ "command": "echo hi" }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![ChatEvent::Done(StopReason::EndTurn)]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// Resolves `solo/m` to a provider built by `make`.
struct StubResolver {
    make: Box<dyn Fn() -> Box<dyn LlmProvider> + Send + Sync>,
}
impl ProviderResolver for StubResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "solo/m" => Ok((self.make)()),
            other => Err(CoreError::NoModel(other.to_string())),
        }
    }
    fn model_info(&self, _model_ref: &str) -> ModelInfo {
        ModelInfo {
            context_window: 200_000,
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
        system_prompt: "you are solo".into(),
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

fn make_orch(resolver: Arc<dyn ProviderResolver>, root: std::path::PathBuf, mode: PermissionMode) -> Orchestrator {
    Orchestrator {
        agents: Default::default(),
        formatters: Default::default(),
        lsp: Default::default(),
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![step()],
        base_context: "ctx".into(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(std::sync::RwLock::new(RuleSet::default())),
        mode: Arc::new(std::sync::RwLock::new(mode)),
        hooks: Arc::new(HookHost::empty(root)),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,
        limits: stepper_core::SessionLimits::default(),
        fallback_model: None,
        resume_seed: Vec::new(),
        sandbox_writable_roots: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn assistant_tokens_stream_before_turn_complete() {
    let dir = tempfile::tempdir().unwrap();
    let resolver = Arc::new(StubResolver {
        make: Box::new(|| Box::new(HelloProvider)),
    });
    let orch = make_orch(resolver, dir.path().to_path_buf(), PermissionMode::AcceptEdits);
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(orch, SessionRecord::fresh(), action_rx, CancellationToken::new());

    action_tx.send(Action::SubmitInput("hi".into())).await.unwrap();

    let mut saw_delta = false;
    while let Some(ev) = events.recv().await {
        match ev {
            AppEvent::AssistantTokenDelta(s) if s == "hello" => saw_delta = true,
            AppEvent::TurnComplete { turn_id } => {
                assert_eq!(turn_id, 1);
                assert!(saw_delta, "the assistant token must stream before TurnComplete");
                break;
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn default_mode_tool_call_asks_then_deny_fails_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let resolver = Arc::new(StubResolver {
        make: Box::new(|| Box::new(ScriptedBashProvider { calls: AtomicUsize::new(0) })),
    });
    // Default mode (what `-p` resolves to is DontAsk, but the *asking* path is the
    // one that can reach ApprovalRequested + Deny — the reachable headless seam).
    let orch = make_orch(resolver, dir.path().to_path_buf(), PermissionMode::Default);
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(orch, SessionRecord::fresh(), action_rx, CancellationToken::new());

    action_tx.send(Action::SubmitInput("run it".into())).await.unwrap();

    let mut saw_approval = false;
    let mut tool_failed = false;
    while let Some(ev) = events.recv().await {
        match ev {
            AppEvent::ApprovalRequested(req) => {
                saw_approval = true;
                // What the headless path sends without --dangerously-auto-approve.
                let _ = req.reply.send(ApprovalDecision::Deny);
            }
            AppEvent::ToolCallFinished { ok, .. } if !ok => tool_failed = true,
            AppEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    assert!(saw_approval, "a bash tool call in Default mode raises ApprovalRequested");
    assert!(tool_failed, "replying Deny makes the gate fail the tool (ok=false)");
}
