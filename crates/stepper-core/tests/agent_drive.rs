//! AgentLoop turn-termination and hook-gating behaviors driven by a scripted
//! FakeProvider: a turn with no tool call finishes immediately, an endless
//! tool-asking model hits the step cap, and a blocking PreToolUse hook denies the
//! tool so it never runs.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use stepper_config::HookEntry;
use stepper_core::{AgentLoop, CoreError, HookHost, ModelRegistry};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, Message, ProviderError, StopReason,
};
use stepper_tools::{Approval, Approver, ToolCx, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    calls: Mutex<usize>,
    scripts: Vec<Vec<ChatEvent>>,
    fallback: Vec<ChatEvent>,
}

#[async_trait]
impl LlmProvider for FakeProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let index = {
            let mut c = self.calls.lock().unwrap();
            let i = *c;
            *c += 1;
            i
        };
        let script = self
            .scripts
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.fallback.clone());
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Allow
    }
}

fn make_cx(dir: &std::path::Path) -> ToolCx {
    ToolCx {
        cwd: dir.to_path_buf(),
        project_root: dir.to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
    }
}

fn spawn_drain() -> mpsc::Sender<stepper_protocol::AppEvent> {
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    tx
}

#[tokio::test]
async fn finishes_turn_when_model_emits_no_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![vec![
            ChatEvent::TextDelta("all done, no tools".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ]],
        fallback: Vec::new(),
    };

    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "test".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: spawn_drain(),
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let outcome = agent
        .drive("system".into(), vec![Message::user("hi")])
        .await
        .unwrap();

    assert_eq!(outcome.summary, "all done, no tools");
    assert_eq!(*provider.calls.lock().unwrap(), 1);
}

#[tokio::test]
async fn terminates_at_step_cap_when_model_never_stops() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: Vec::new(),
        fallback: vec![
            ChatEvent::ToolCallCompleted {
                index: 0,
                id: "call".into(),
                name: "list_dir".into(),
                input: serde_json::json!({ "path": "." }),
            },
            ChatEvent::Done(StopReason::ToolUse),
        ],
    };

    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "looping".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: spawn_drain(),
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 3,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let result = agent
        .drive("system".into(), vec![Message::user("go forever")])
        .await;

    match result {
        Err(CoreError::StepCapHit { layer, cap }) => {
            assert_eq!(layer, "looping");
            assert_eq!(cap, 3);
        }
        Ok(outcome) => panic!("expected StepCapHit, got Ok({})", outcome.summary),
        Err(other) => panic!("expected StepCapHit, got {other:?}"),
    }
    assert_eq!(*provider.calls.lock().unwrap(), 3);
}

#[tokio::test]
async fn blocking_pretooluse_hook_denies_the_tool() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "call_1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({ "path": "blocked.txt", "content": "nope" }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ],
            vec![
                ChatEvent::TextDelta("acknowledged the block".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ],
        ],
        fallback: Vec::new(),
    };

    let mut hook_map = BTreeMap::new();
    hook_map.insert(
        "PreToolUse".to_string(),
        vec![HookEntry {
            matcher: Some("write_file".into()),
            command: "echo policy denied >&2; exit 2".into(),
        }],
    );
    let hooks = Arc::new(HookHost::new(hook_map, root.clone()));

    let (tx, mut rx) = mpsc::channel(256);
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            seen2.lock().unwrap().push(format!("{ev:?}"));
        }
    });

    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "guarded".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: tx,
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks,
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let outcome = agent
        .drive("system".into(), vec![Message::user("write blocked.txt")])
        .await
        .unwrap();

    assert_eq!(outcome.summary, "acknowledged the block");
    assert!(
        !root.join("blocked.txt").exists(),
        "blocked tool must not have written the file"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let events = seen.lock().unwrap().join("\n");
    assert!(events.contains("ToolCallFinished"));
    assert!(events.contains("ok: false"), "block surfaced as failure: {events}");
}

#[tokio::test]
async fn posttooluse_hook_runs_after_the_tool_without_blocking_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sentinel = root.join("post-hook-ran.txt");
    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "call_1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({ "path": "produced.txt", "content": "made it" }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ],
            vec![
                ChatEvent::TextDelta("finished after post hook".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ],
        ],
        fallback: Vec::new(),
    };

    let mut hook_map = BTreeMap::new();
    hook_map.insert(
        "PostToolUse".to_string(),
        vec![HookEntry {
            matcher: Some("write_file".into()),
            command: "touch post-hook-ran.txt; exit 0".into(),
        }],
    );
    let hooks = Arc::new(HookHost::new(hook_map, root.clone()));

    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "post".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: spawn_drain(),
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks,
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let outcome = agent
        .drive("system".into(), vec![Message::user("write produced.txt")])
        .await
        .unwrap();

    assert_eq!(
        outcome.summary, "finished after post hook",
        "a non-blocking PostToolUse hook must not stop the loop from finishing"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("produced.txt")).unwrap(),
        "made it",
        "the tool itself ran and produced its file"
    );
    assert!(
        sentinel.exists(),
        "the PostToolUse hook must have actually run (sentinel file created)"
    );
    assert_eq!(
        *provider.calls.lock().unwrap(),
        2,
        "loop proceeded to the second turn after the post hook (not blocked)"
    );
}

#[tokio::test]
async fn already_cancelled_token_returns_cancelled_before_any_step() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: Vec::new(),
        fallback: vec![
            ChatEvent::TextDelta("should never be produced".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ],
    };

    let mut cx = make_cx(dir.path());
    cx.cancel = CancellationToken::new();
    cx.cancel.cancel();

    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "cancelled".into(),
        provider: &provider,
        tools: &registry,
        cx,
        event_tx: spawn_drain(),
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let result = agent
        .drive("system".into(), vec![Message::user("go")])
        .await;

    match result {
        Err(CoreError::Cancelled) => {}
        Ok(outcome) => panic!("expected Cancelled, got Ok({})", outcome.summary),
        Err(other) => panic!("expected Cancelled, got {other:?}"),
    }
    assert_eq!(
        *provider.calls.lock().unwrap(),
        0,
        "a pre-cancelled token must short-circuit before the provider is ever called"
    );
}

#[tokio::test]
async fn pretooluse_matcher_is_exact_and_does_not_substring_match() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "call_1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({ "path": "written.txt", "content": "let me through" }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ],
            vec![
                ChatEvent::TextDelta("wrote it".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ],
        ],
        fallback: Vec::new(),
    };

    let mut hook_map = BTreeMap::new();
    hook_map.insert(
        "PreToolUse".to_string(),
        vec![HookEntry {
            matcher: Some("write".into()),
            command: "echo should not run >&2; exit 2".into(),
        }],
    );
    let hooks = Arc::new(HookHost::new(hook_map, root.clone()));

    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "guarded".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: spawn_drain(),
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks,
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let outcome = agent
        .drive("system".into(), vec![Message::user("write written.txt")])
        .await
        .unwrap();

    assert_eq!(outcome.summary, "wrote it");
    assert_eq!(
        std::fs::read_to_string(root.join("written.txt")).unwrap(),
        "let me through",
        "a `write` matcher must NOT block `write_file` (exact-match only)"
    );
}

/// Per-call scripted transport: each entry is either a stream of events or a
/// `chat_stream`-level error built on demand (ProviderError is not Clone).
enum Attempt {
    Stream(Vec<ChatEvent>),
    Fail(fn() -> ProviderError),
}

struct AttemptProvider {
    calls: Mutex<usize>,
    attempts: Vec<Attempt>,
}

#[async_trait]
impl LlmProvider for AttemptProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let index = {
            let mut c = self.calls.lock().unwrap();
            let i = *c;
            *c += 1;
            i
        };
        let attempt = self.attempts.get(index).unwrap_or_else(|| {
            self.attempts.last().expect("at least one scripted attempt")
        });
        match attempt {
            Attempt::Stream(events) => Ok(Box::pin(futures::stream::iter(
                events.clone().into_iter().map(Ok),
            ))),
            Attempt::Fail(make) => Err(make()),
        }
    }
}

fn agent<'a>(
    provider: &'a AttemptProvider,
    registry: &'a ToolRegistry,
    dir: &std::path::Path,
    tx: mpsc::Sender<stepper_protocol::AppEvent>,
) -> AgentLoop<'a> {
    AgentLoop {
        layer_name: "retry".into(),
        provider,
        tools: registry,
        cx: make_cx(dir),
        event_tx: tx,
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    }
}

#[tokio::test]
async fn stream_without_done_is_retried_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    // First stream closes cleanly with no terminal Done (a truncated turn);
    // the retry gets a complete one.
    let provider = AttemptProvider {
        calls: Mutex::new(0),
        attempts: vec![
            Attempt::Stream(vec![ChatEvent::TextDelta("partial answ".into())]),
            Attempt::Stream(vec![
                ChatEvent::TextDelta("complete".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]),
        ],
    };

    let (tx, mut rx) = mpsc::channel(256);
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            seen2.lock().unwrap().push(format!("{ev:?}"));
        }
    });

    let registry = ToolRegistry::builtins();
    let outcome = agent(&provider, &registry, dir.path(), tx)
        .drive("system".into(), vec![Message::user("go")])
        .await
        .unwrap();

    assert_eq!(outcome.summary, "complete");
    assert_eq!(
        *provider.calls.lock().unwrap(),
        2,
        "the truncated stream must be retried exactly once before succeeding"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let events = seen.lock().unwrap().join("\n");
    assert!(
        events.contains("api retry 1/3"),
        "the retry must surface as a Notice: {events}"
    );
}

#[tokio::test]
async fn retryable_api_error_exhausts_retries_then_fails() {
    let dir = tempfile::tempdir().unwrap();
    let provider = AttemptProvider {
        calls: Mutex::new(0),
        attempts: vec![Attempt::Fail(|| ProviderError::Api {
            status: 503,
            code: None,
            message: "overloaded".into(),
        })],
    };

    let registry = ToolRegistry::builtins();
    let err = match agent(&provider, &registry, dir.path(), spawn_drain())
        .drive("system".into(), vec![Message::user("go")])
        .await
    {
        Err(e) => e,
        Ok(outcome) => panic!("expected a provider error, got Ok({})", outcome.summary),
    };

    assert!(matches!(err, CoreError::Provider(_)), "got {err:?}");
    assert_eq!(
        *provider.calls.lock().unwrap(),
        4,
        "a retryable failure is attempted once plus three retries"
    );
}

#[tokio::test]
async fn non_retryable_error_fails_without_retry() {
    let dir = tempfile::tempdir().unwrap();
    let provider = AttemptProvider {
        calls: Mutex::new(0),
        attempts: vec![Attempt::Fail(|| {
            ProviderError::Auth("bad key".into())
        })],
    };

    let registry = ToolRegistry::builtins();
    let err = match agent(&provider, &registry, dir.path(), spawn_drain())
        .drive("system".into(), vec![Message::user("go")])
        .await
    {
        Err(e) => e,
        Ok(outcome) => panic!("expected a provider error, got Ok({})", outcome.summary),
    };

    assert!(matches!(err, CoreError::Provider(_)), "got {err:?}");
    assert_eq!(
        *provider.calls.lock().unwrap(),
        1,
        "auth failures must never be retried"
    );
}

type SeenSampling = Arc<Mutex<Option<(Option<f32>, Option<f32>)>>>;

struct TempRecordingProvider {
    seen: SeenSampling,
}

#[async_trait]
impl LlmProvider for TempRecordingProvider {
    fn provider(&self) -> &str {
        "temp"
    }
    fn model(&self) -> &str {
        "temp-model"
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        *self.seen.lock().unwrap() = Some((request.temperature, request.top_p));
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ChatEvent::TextDelta("ok".into())),
            Ok(ChatEvent::Done(StopReason::EndTurn)),
        ])))
    }
}

#[tokio::test]
async fn sampling_overrides_are_forwarded_to_the_provider_request() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let provider = TempRecordingProvider { seen: seen.clone() };
    let registry = ToolRegistry::builtins();
    let agent = AgentLoop {
        layer_name: "sampled".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: spawn_drain(),
        model_info: ModelRegistry::builtin().lookup("temp", "temp-model"),
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: Some(0.3),
        top_p: Some(0.9),
        worker: None,
    };

    agent.drive("system".into(), vec![Message::user("hi")]).await.unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        Some((Some(0.3), Some(0.9))),
        "the layer's temperature/top_p must reach the provider request"
    );
}

struct MaxTokensRecordingProvider {
    seen: Arc<Mutex<Vec<Option<u32>>>>,
}

#[async_trait]
impl LlmProvider for MaxTokensRecordingProvider {
    fn provider(&self) -> &str {
        "cap"
    }
    fn model(&self) -> &str {
        "cap-model"
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.seen.lock().unwrap().push(request.max_tokens);
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(ChatEvent::TextDelta("ok".into())),
            Ok(ChatEvent::Done(StopReason::EndTurn)),
        ])))
    }
}

#[tokio::test]
async fn max_tokens_comes_from_the_model_info_output_cap() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = MaxTokensRecordingProvider { seen: seen.clone() };
    let registry = ToolRegistry::builtins();
    let mut model_info = ModelRegistry::builtin().lookup("anthropic", "claude-sonnet-4-6");
    assert_eq!(model_info.max_output_tokens, 64_000);
    let mut agent = AgentLoop {
        layer_name: "capped".into(),
        provider: &provider,
        tools: &registry,
        cx: make_cx(dir.path()),
        event_tx: spawn_drain(),
        model_info,
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    agent.drive("system".into(), vec![Message::user("hi")]).await.unwrap();
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[Some(64_000)],
        "the registry's max_output_tokens must reach the request"
    );

    // A zero cap leaves the provider default in place (max_tokens: None).
    model_info.max_output_tokens = 0;
    agent.model_info = model_info;
    agent.drive("system".into(), vec![Message::user("hi again")]).await.unwrap();
    assert_eq!(seen.lock().unwrap().as_slice(), &[Some(64_000), None]);
}
