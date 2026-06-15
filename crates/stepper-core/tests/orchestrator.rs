//! Orchestrator.run_turn across two layers backed by different providers: layer
//! one's free-text outcome is the only carrier between the separate context
//! windows, so layer two must receive layer one's summary inside its opening user
//! message (the handoff). Also covers failure propagation: a step-cap error in an
//! early layer aborts the turn.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use stepper_core::{
    CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver, StepDef,
};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason,
};
use stepper_tools::{Approval, Approver, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct ScriptedProvider {
    provider_name: String,
    model_name: String,
    reply: String,
    loops_forever: bool,
    seen_first_user: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn provider(&self) -> &str {
        &self.provider_name
    }
    fn model(&self) -> &str {
        &self.model_name
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        if self.seen_first_user.lock().unwrap().is_none()
            && let Some(first) = request.messages.first()
        {
            *self.seen_first_user.lock().unwrap() = Some(first.text());
        }
        let script = if self.loops_forever {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "c".into(),
                    name: "list_dir".into(),
                    input: serde_json::json!({ "path": "." }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta(self.reply.clone()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct RecordingResolver {
    planner_seen: Arc<Mutex<Option<String>>>,
    builder_seen: Arc<Mutex<Option<String>>>,
    fail_planner: bool,
}

impl ProviderResolver for RecordingResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "planner/model-a" => Ok(Box::new(ScriptedProvider {
                provider_name: "planner-provider".into(),
                model_name: "model-a".into(),
                reply: "PLAN: write the README first".into(),
                loops_forever: self.fail_planner,
                seen_first_user: self.planner_seen.clone(),
            })),
            "builder/model-b" => Ok(Box::new(ScriptedProvider {
                provider_name: "builder-provider".into(),
                model_name: "model-b".into(),
                reply: "BUILT it".into(),
                loops_forever: false,
                seen_first_user: self.builder_seen.clone(),
            })),
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

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Allow
    }
}

fn step(name: &str, model_ref: &str, cap: usize) -> StepDef {
    StepDef {
        name: name.into(),
        model_ref: model_ref.into(),
        system_prompt: format!("you are {name}"),
        tool_allow: Vec::new(),
        tool_deny: Vec::new(),
        mcp_allow: Vec::new(),
        step_cap: cap,
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

fn orchestrator(resolver: Arc<dyn ProviderResolver>, root: std::path::PathBuf) -> Orchestrator {
    Orchestrator {
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![
            step("plan", "planner/model-a", 5),
            step("build", "builder/model-b", 5),
        ],
        base_context: "shared base context".into(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(RuleSet::default()),
        mode: PermissionMode::AcceptEdits,
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

#[tokio::test]
async fn forwards_layer_one_summary_into_layer_two_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let planner_seen = Arc::new(Mutex::new(None));
    let builder_seen = Arc::new(Mutex::new(None));
    let resolver = Arc::new(RecordingResolver {
        planner_seen: planner_seen.clone(),
        builder_seen: builder_seen.clone(),
        fail_planner: false,
    });
    let orch = orchestrator(resolver, dir.path().to_path_buf());

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let summaries = orch
        .run_turn(
            "build me a project".into(),
            Vec::new(),
            &tx,
            Arc::new(AllowAll),
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .summaries;

    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0], ("plan".into(), "PLAN: write the README first".into()));
    assert_eq!(summaries[1], ("build".into(), "BUILT it".into()));

    let planner_input = planner_seen.lock().unwrap().clone().unwrap();
    assert!(
        planner_input.contains("build me a project"),
        "layer one sees the raw user turn: {planner_input}"
    );
    assert!(
        !planner_input.contains("Output from prior layers"),
        "the first layer has no prior handoff: {planner_input}"
    );

    let builder_input = builder_seen.lock().unwrap().clone().unwrap();
    assert!(
        builder_input.contains("PLAN: write the README first"),
        "layer two must receive layer one's summary: {builder_input}"
    );
    assert!(
        builder_input.contains("build me a project"),
        "layer two still carries the original task: {builder_input}"
    );
}

#[tokio::test]
async fn early_layer_step_cap_aborts_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let builder_seen = Arc::new(Mutex::new(None));
    let resolver = Arc::new(RecordingResolver {
        planner_seen: Arc::new(Mutex::new(None)),
        builder_seen: builder_seen.clone(),
        fail_planner: true,
    });
    let mut orch = orchestrator(resolver, dir.path().to_path_buf());
    orch.steps[0].step_cap = 2;

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let err = orch
        .run_turn(
            "do it".into(),
            Vec::new(),
            &tx,
            Arc::new(AllowAll),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();

    match err {
        CoreError::StepCapHit { layer, cap } => {
            assert_eq!(layer, "plan");
            assert_eq!(cap, 2);
        }
        other => panic!("expected StepCapHit, got {other:?}"),
    }
    assert!(
        builder_seen.lock().unwrap().is_none(),
        "layer two must never run once layer one fails"
    );
}

#[tokio::test]
async fn failed_layer_with_skip_policy_continues_to_next_layer() {
    let dir = tempfile::tempdir().unwrap();
    let builder_seen = Arc::new(Mutex::new(None));
    let resolver = Arc::new(RecordingResolver {
        planner_seen: Arc::new(Mutex::new(None)),
        builder_seen: builder_seen.clone(),
        fail_planner: true,
    });
    let mut orch = orchestrator(resolver, dir.path().to_path_buf());
    orch.steps[0].step_cap = 2;
    orch.steps[0].on_failure = FailurePolicy::Skip;

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let summaries = orch
        .run_turn("do it".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .expect("skip policy keeps the turn alive")
        .summaries;

    assert!(
        builder_seen.lock().unwrap().is_some(),
        "layer two must still run when layer one is skipped"
    );
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].0, "plan");
    assert!(
        summaries[0].1.contains("skipped"),
        "the skipped layer's outcome records the failure: {:?}",
        summaries[0]
    );
    assert_eq!(summaries[1], ("build".into(), "BUILT it".into()));

    let builder_input = builder_seen.lock().unwrap().clone().unwrap();
    assert!(
        builder_input.contains("skipped"),
        "layer two's handoff carries the skip note: {builder_input}"
    );
}

struct FlakyProvider {
    calls: Arc<AtomicUsize>,
    fail_until: usize,
}

#[async_trait]
impl LlmProvider for FlakyProvider {
    fn provider(&self) -> &str {
        "flaky"
    }
    fn model(&self) -> &str {
        "flaky-m"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let script = if n < self.fail_until {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "c".into(),
                    name: "list_dir".into(),
                    input: serde_json::json!({ "path": "." }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta("recovered".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct FlakyResolver {
    calls: Arc<AtomicUsize>,
    fail_until: usize,
}

impl ProviderResolver for FlakyResolver {
    fn resolve(&self, _model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        Ok(Box::new(FlakyProvider {
            calls: self.calls.clone(),
            fail_until: self.fail_until,
        }))
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

/// Fails every request with a non-retryable auth error.
struct AuthFailProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for AuthFailProvider {
    fn provider(&self) -> &str {
        "primary"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Auth("invalid api key".into()))
    }
}

struct FallbackResolver {
    primary_calls: Arc<AtomicUsize>,
    backup_seen: Arc<Mutex<Option<String>>>,
}

impl ProviderResolver for FallbackResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "primary/m" => Ok(Box::new(AuthFailProvider {
                calls: self.primary_calls.clone(),
            })),
            "backup/m" => Ok(Box::new(ScriptedProvider {
                provider_name: "backup".into(),
                model_name: "m".into(),
                reply: "rescued by fallback".into(),
                loops_forever: false,
                seen_first_user: self.backup_seen.clone(),
            })),
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

#[tokio::test]
async fn fallback_model_engages_after_a_non_retryable_failure_and_notices() {
    let dir = tempfile::tempdir().unwrap();
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let backup_seen = Arc::new(Mutex::new(None));
    let resolver = Arc::new(FallbackResolver {
        primary_calls: primary_calls.clone(),
        backup_seen: backup_seen.clone(),
    });
    let mut orch = orchestrator(resolver, dir.path().to_path_buf());
    orch.steps = vec![step("solo", "primary/m", 5)];
    orch.steps[0].retries = 2;
    orch.fallback_model = Some("backup/m".into());

    let (tx, mut rx) = mpsc::channel(256);
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            seen2.lock().unwrap().push(format!("{ev:?}"));
        }
    });

    let summaries = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .expect("the fallback model rescues the layer")
        .summaries;

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].1, "rescued by fallback");
    assert_eq!(
        primary_calls.load(Ordering::SeqCst),
        1,
        "a non-retryable auth failure must not consume layer retries or api retries"
    );
    assert!(
        backup_seen.lock().unwrap().is_some(),
        "the fallback provider must actually be driven"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let events = seen.lock().unwrap().join("\n");
    assert!(
        events.contains("fallback model 'backup/m'"),
        "the switch must surface as a Notice: {events}"
    );
}

#[tokio::test]
async fn non_retryable_failure_without_fallback_fails_without_retry() {
    let dir = tempfile::tempdir().unwrap();
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(FallbackResolver {
        primary_calls: primary_calls.clone(),
        backup_seen: Arc::new(Mutex::new(None)),
    });
    let mut orch = orchestrator(resolver, dir.path().to_path_buf());
    orch.steps = vec![step("solo", "primary/m", 5)];
    orch.steps[0].retries = 2;

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let err = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(err, CoreError::Provider(_)), "got {err:?}");
    assert_eq!(
        primary_calls.load(Ordering::SeqCst),
        1,
        "step.retries must only re-run retryable failures"
    );
}

#[tokio::test]
async fn a_failing_layer_is_retried_and_can_recover() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(FlakyResolver {
        calls: calls.clone(),
        fail_until: 1,
    });
    let mut orch = orchestrator(resolver, dir.path().to_path_buf());
    // One single-step layer that hits the cap on its first attempt, recovers on
    // the retry. (step_cap 1 + a tool call -> StepCapHit on attempt one.)
    orch.steps = vec![step("solo", "solo/m", 1)];
    orch.steps[0].retries = 1;

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let summaries = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .expect("the retry recovers the layer")
        .summaries;

    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].1, "recovered");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "exactly one failed attempt followed by one successful retry"
    );
}
