//! The model-callable `dispatch` tool (parallel sub-agents): the tool forwards
//! tasks to a `Dispatcher` and folds results; the production
//! `OrchestratorDispatcher` runs real sub-agents; and an end-to-end turn shows a
//! model calling `dispatch` when the orchestrator enables it.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use stepper_core::{
    CoreError, DispatchRequest, DispatchResult, DispatchTool, Dispatcher, FailurePolicy, HookHost,
    ModelInfo, Orchestrator, OrchestratorDispatcher, ProviderResolver, StepDef,
};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason, ToolResult, ToolSpec,
};
use stepper_tools::{Approval, Approver, Tool, ToolCx, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _a: Approval) -> Decision {
        Decision::Allow
    }
}

fn cx(dir: &std::path::Path) -> ToolCx {
    ToolCx {
        cwd: dir.to_path_buf(),
        project_root: dir.to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    }
}

struct RecordingDispatcher {
    seen: Arc<Mutex<Vec<DispatchRequest>>>,
}
#[async_trait]
impl Dispatcher for RecordingDispatcher {
    async fn dispatch(&self, requests: Vec<DispatchRequest>) -> Vec<DispatchResult> {
        self.seen.lock().unwrap().extend(requests.iter().cloned());
        requests
            .into_iter()
            .map(|r| DispatchResult {
                ok: r.label != "boom",
                summary: format!("did {}", r.prompt),
                label: r.label,
            })
            .collect()
    }
}

#[tokio::test]
async fn dispatch_tool_forwards_tasks_and_folds_results() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool = DispatchTool::new(Arc::new(RecordingDispatcher { seen: seen.clone() }));

    let result = tool
        .call(
            serde_json::json!({
                "tasks": [
                    { "label": "a", "prompt": "do A" },
                    { "prompt": "do B" }
                ]
            }),
            &cx(dir.path()),
        )
        .await
        .unwrap();

    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].label, "a");
    assert_eq!(reqs[0].prompt, "do A");
    assert_eq!(reqs[1].label, "task-1", "a missing label is auto-named by index");
    assert!(!result.is_error);
    let text = result.content_text();
    assert!(text.contains("did do A") && text.contains("did do B"), "{text}");
}

#[tokio::test]
async fn dispatch_tool_marks_error_when_a_subagent_fails() {
    let dir = tempfile::tempdir().unwrap();
    let tool = DispatchTool::new(Arc::new(RecordingDispatcher {
        seen: Arc::new(Mutex::new(Vec::new())),
    }));
    let result = tool
        .call(
            serde_json::json!({ "tasks": [{ "label": "boom", "prompt": "x" }] }),
            &cx(dir.path()),
        )
        .await
        .unwrap();
    assert!(result.is_error, "a failed sub-agent makes the tool result an error");
}

#[tokio::test]
async fn dispatch_tool_rejects_missing_or_empty_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let tool = DispatchTool::new(Arc::new(RecordingDispatcher {
        seen: Arc::new(Mutex::new(Vec::new())),
    }));
    assert!(tool.call(serde_json::json!({}), &cx(dir.path())).await.is_err());
    assert!(
        tool.call(serde_json::json!({ "tasks": [] }), &cx(dir.path()))
            .await
            .is_err()
    );
    assert!(
        tool.call(serde_json::json!({ "tasks": [{ "label": "x" }] }), &cx(dir.path()))
            .await
            .is_err(),
        "a task without a prompt is invalid"
    );
}

struct EchoProvider;
#[async_trait]
impl LlmProvider for EchoProvider {
    fn provider(&self) -> &str {
        "echo"
    }
    fn model(&self) -> &str {
        "echo-m"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ChatEvent::TextDelta("sub-agent done".into())),
            Ok(ChatEvent::Done(StopReason::EndTurn)),
        ])))
    }
}

struct EchoResolver;
impl ProviderResolver for EchoResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        if model_ref == "bad/model" {
            Err(CoreError::NoModel(model_ref.into()))
        } else {
            Ok(Box::new(EchoProvider))
        }
    }
    fn model_info(&self, _m: &str) -> ModelInfo {
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

fn orchestrator_dispatcher(
    dir: &std::path::Path,
    event_tx: mpsc::Sender<stepper_protocol::AppEvent>,
) -> OrchestratorDispatcher {
    OrchestratorDispatcher {
        formatters: Default::default(),
        lsp: Default::default(),
        resolver: Arc::new(EchoResolver),
        base_tools: ToolRegistry::builtins(),
        hooks: Arc::new(HookHost::empty(dir.to_path_buf())),
        cwd: dir.to_path_buf(),
        project_root: dir.to_path_buf(),
        home: None,
        rules: Arc::new(RuleSet::default()),
        mode: PermissionMode::AcceptEdits,
        default_model: "default/model".into(),
        event_tx,
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
        compaction_provider: None,
        concurrency: 4,
        step_cap: 4,
        sandbox_writable_roots: None,
        budget: None,
        base_context: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn orchestrator_dispatcher_runs_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let d = orchestrator_dispatcher(dir.path(), tx);

    let results = d
        .dispatch(vec![
            DispatchRequest {
                label: "one".into(),
                prompt: "p1".into(),
                model_ref: None,
                ..Default::default()
            },
            DispatchRequest {
                label: "two".into(),
                prompt: "p2".into(),
                model_ref: None,
                ..Default::default()
            },
        ])
        .await;

    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(|r| r.ok && r.summary == "sub-agent done"),
        "{results:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn orchestrator_dispatcher_reports_unresolvable_models_as_failures() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let d = orchestrator_dispatcher(dir.path(), tx);

    let results = d
        .dispatch(vec![
            DispatchRequest {
                label: "ok".into(),
                prompt: "p".into(),
                model_ref: None,
                ..Default::default()
            },
            DispatchRequest {
                label: "bad".into(),
                prompt: "p".into(),
                model_ref: Some("bad/model".into()),
                ..Default::default()
            },
        ])
        .await;

    assert_eq!(results.len(), 2);
    let bad = results.iter().find(|r| r.label == "bad").unwrap();
    assert!(!bad.ok && bad.summary.contains("could not resolve"), "{bad:?}");
    assert!(results.iter().find(|r| r.label == "ok").unwrap().ok);
}

// ---- end-to-end: a model calls `dispatch` when the orchestrator enables it ----

struct DispatchCaller {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl LlmProvider for DispatchCaller {
    fn provider(&self) -> &str {
        "caller"
    }
    fn model(&self) -> &str {
        "caller-m"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let script = if n == 0 {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "d1".into(),
                    name: "dispatch".into(),
                    input: serde_json::json!({
                        "tasks": [{ "label": "worker", "prompt": "do the sub work", "model": "sub/model" }]
                    }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta("orchestrated".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct SubMarker {
    sub_ran: Arc<AtomicUsize>,
}
#[async_trait]
impl LlmProvider for SubMarker {
    fn provider(&self) -> &str {
        "sub"
    }
    fn model(&self) -> &str {
        "sub-m"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.sub_ran.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ChatEvent::TextDelta("worker finished".into())),
            Ok(ChatEvent::Done(StopReason::EndTurn)),
        ])))
    }
}

struct CallerResolver {
    calls: Arc<AtomicUsize>,
    sub_ran: Arc<AtomicUsize>,
}
impl ProviderResolver for CallerResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "sub/model" => Ok(Box::new(SubMarker {
                sub_ran: self.sub_ran.clone(),
            })),
            _ => Ok(Box::new(DispatchCaller {
                calls: self.calls.clone(),
            })),
        }
    }
    fn model_info(&self, _m: &str) -> ModelInfo {
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

// ---- security: dispatched sub-agents inherit the calling layer's tool scope ----

struct CountingTool {
    spec: ToolSpec,
    ran: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for CountingTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> Result<ToolResult, stepper_provider::ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::text("ran"))
    }
}

/// Sub-agent that tries the `probe` tool on its first turn, then finishes.
struct ProbeSub;
#[async_trait]
impl LlmProvider for ProbeSub {
    fn provider(&self) -> &str {
        "sub"
    }
    fn model(&self) -> &str {
        "sub-m"
    }
    async fn chat_stream(
        &self,
        r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        // first call (no Tool messages yet) → ask for `probe`; later → finish.
        let first = !r.messages.iter().any(|m| matches!(m.role, stepper_provider::Role::Tool));
        let script = if first {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "p1".into(),
                    name: "probe".into(),
                    input: serde_json::json!({}),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta("sub done".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct ProbeResolver {
    calls: Arc<AtomicUsize>,
}
impl ProviderResolver for ProbeResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "sub/model" => Ok(Box::new(ProbeSub)),
            _ => Ok(Box::new(DispatchCaller { calls: self.calls.clone() })),
        }
    }
    fn model_info(&self, _m: &str) -> ModelInfo {
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

#[tokio::test(flavor = "multi_thread")]
async fn dispatched_subagents_cannot_use_a_tool_the_calling_layer_denied() {
    let dir = tempfile::tempdir().unwrap();
    let probe_ran = Arc::new(AtomicUsize::new(0));

    // base tools include a `probe` tool, but the calling layer denies it.
    let mut base = ToolRegistry::builtins();
    base.register(Arc::new(CountingTool {
        spec: ToolSpec {
            name: "probe".into(),
            description: String::new(),
            input_schema: serde_json::json!({ "type": "object" }),
            read_only: false,
            parallel_safe: false,
        },
        ran: probe_ran.clone(),
    }));

    let main = StepDef {
        name: "main".into(),
        model_ref: "main/model".into(),
        system_prompt: "you orchestrate".into(),
        tool_allow: Vec::new(),
        tool_deny: vec!["probe".into()], // the layer removes `probe` from its surface
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
    };

    let orch = Orchestrator {
        agents: Default::default(),
        formatters: Default::default(),
        lsp: Default::default(),
        resolver: Arc::new(ProbeResolver { calls: Arc::new(AtomicUsize::new(0)) }),
        base_tools: base,
        steps: vec![main],
        base_context: "ctx".into(),
        project_root: dir.path().to_path_buf(),
        cwd: dir.path().to_path_buf(),
        home: None,
        rules: Arc::new(std::sync::RwLock::new(RuleSet::default())),
        mode: Arc::new(std::sync::RwLock::new(PermissionMode::AcceptEdits)),
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: true,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,        limits: stepper_core::SessionLimits::default(),
        fallback_model: None,
        resume_seed: Vec::new(),
        sandbox_writable_roots: None,
    };

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let _ = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        probe_ran.load(Ordering::SeqCst),
        0,
        "a dispatched sub-agent must NOT be able to run a tool the calling layer denied (tool scope is inherited)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn enabling_dispatch_lets_the_model_fan_out_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let sub_ran = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(CallerResolver {
        calls: Arc::new(AtomicUsize::new(0)),
        sub_ran: sub_ran.clone(),
    });
    let orch = Orchestrator {
        agents: Default::default(),
        formatters: Default::default(),
        lsp: Default::default(),
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![StepDef {
            name: "main".into(),
            model_ref: "main/model".into(),
            system_prompt: "you orchestrate".into(),
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
        }],
        base_context: "ctx".into(),
        project_root: dir.path().to_path_buf(),
        cwd: dir.path().to_path_buf(),
        home: None,
        rules: Arc::new(std::sync::RwLock::new(RuleSet::default())),
        mode: Arc::new(std::sync::RwLock::new(PermissionMode::AcceptEdits)),
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: true,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,        limits: stepper_core::SessionLimits::default(),
        fallback_model: None,
        resume_seed: Vec::new(),
        sandbox_writable_roots: None,
    };

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let summaries = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap()
        .summaries;

    assert_eq!(summaries[0].1, "orchestrated");
    assert_eq!(
        sub_ran.load(Ordering::SeqCst),
        1,
        "the dispatched sub-agent must have actually run"
    );
}

// ---- P0-5: the `task` tool (named sub-agents) ----

fn cx_with(dir: &std::path::Path, mode: PermissionMode, approver: Arc<dyn Approver>, rules: RuleSet) -> ToolCx {
    ToolCx {
        cwd: dir.to_path_buf(),
        project_root: dir.to_path_buf(),
        home: None,
        mode,
        live_mode: None,
        rules: Arc::new(rules),
        approver,
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    }
}

struct DenyAll;
#[async_trait]
impl Approver for DenyAll {
    async fn request(&self, _a: Approval) -> Decision {
        Decision::Deny
    }
}

fn agent(name: &str, model: Option<&str>) -> stepper_core::AgentDef {
    stepper_core::AgentDef {
        name: name.into(),
        description: "a test agent".into(),
        model_ref: model.map(str::to_string),
        tool_allow: Vec::new(),
        tool_deny: Vec::new(),
        role_prompt: format!("You are {name}."),
    }
}

#[tokio::test]
async fn task_tool_delegates_to_the_named_agent_with_its_config() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool = stepper_core::TaskTool::new(
        Arc::new(vec![agent("reviewer", Some("solo/big"))]),
        Arc::new(RecordingDispatcher { seen: seen.clone() }),
    );
    let result = tool
        .call(
            serde_json::json!({ "subagent_type": "reviewer", "prompt": "review X" }),
            &cx(dir.path()),
        )
        .await
        .unwrap();
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].prompt, "review X");
    assert_eq!(reqs[0].model_ref.as_deref(), Some("solo/big"));
    assert_eq!(reqs[0].role.as_deref(), Some("You are reviewer."));
    assert!(!result.is_error);
}

#[tokio::test]
async fn task_tool_rejects_an_unknown_agent() {
    let dir = tempfile::tempdir().unwrap();
    let tool = stepper_core::TaskTool::new(
        Arc::new(vec![agent("reviewer", None)]),
        Arc::new(RecordingDispatcher { seen: Arc::new(Mutex::new(Vec::new())) }),
    );
    let err = tool
        .call(serde_json::json!({ "subagent_type": "nope", "prompt": "x" }), &cx(dir.path()))
        .await
        .unwrap_err();
    match err {
        stepper_tools::ToolError::InvalidArgs(msg) => assert!(msg.contains("reviewer"), "lists available: {msg}"),
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

#[tokio::test]
async fn task_tool_is_denied_when_permission_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool = stepper_core::TaskTool::new(
        Arc::new(vec![agent("reviewer", None)]),
        Arc::new(RecordingDispatcher { seen: seen.clone() }),
    );
    // Default mode → Task asks; DenyAll refuses, so the agent never dispatches.
    let cx = cx_with(dir.path(), PermissionMode::Default, Arc::new(DenyAll), RuleSet::default());
    let err = tool
        .call(serde_json::json!({ "subagent_type": "reviewer", "prompt": "x" }), &cx)
        .await
        .unwrap_err();
    assert!(matches!(err, stepper_tools::ToolError::Denied(_)), "got {err:?}");
    assert!(seen.lock().unwrap().is_empty(), "a denied task never dispatches");
}

#[tokio::test]
async fn task_tool_allowed_by_a_task_rule_even_when_mode_would_ask() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool = stepper_core::TaskTool::new(
        Arc::new(vec![agent("reviewer", None)]),
        Arc::new(RecordingDispatcher { seen: seen.clone() }),
    );
    // An explicit `Task(reviewer)` allow rule lets it run in Default mode without
    // asking; DenyAll would refuse an Ask, so reaching dispatch proves the allow.
    let rules = RuleSet::from_lists(&["Task(reviewer)".into()], &[], &[]);
    let cx = cx_with(dir.path(), PermissionMode::Default, Arc::new(DenyAll), rules);
    tool.call(serde_json::json!({ "subagent_type": "reviewer", "prompt": "x" }), &cx)
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1, "the allow rule let the task dispatch");
}
