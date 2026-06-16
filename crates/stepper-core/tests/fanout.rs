//! Fan-out: several sub-agents run concurrently under a concurrency cap.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stepper_core::{run_parallel, FanoutTask, HookHost, ModelRegistry};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{ChatEvent, ChatRequest, ChatStream, LlmProvider, Message, ProviderError, StopReason};
use stepper_tools::{Approval, Approver, ToolCx, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct SayProvider(String);
#[async_trait]
impl LlmProvider for SayProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let script = vec![
            ChatEvent::TextDelta(self.0.clone()),
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _a: Approval) -> Decision {
        Decision::Allow
    }
}

fn task(dir: &std::path::Path, label: &str, say: &str) -> FanoutTask {
    FanoutTask {
        label: label.into(),
        worker_index: 0,
        provider: Box::new(SayProvider(say.into())),
        tools: ToolRegistry::builtins(),
        cx: ToolCx {
            cwd: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            home: None,
            mode: PermissionMode::AcceptEdits,
            live_mode: None,
            rules: Arc::new(RuleSet::default()),
            approver: Arc::new(AllowAll),
            cancel: CancellationToken::new(),
            sandbox_writable_roots: None,
        },
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 3,
        hooks: Arc::new(HookHost::empty(dir.to_path_buf())),
        compaction_provider: None,
        system: "you are a worker".into(),
        messages: vec![Message::user("go")],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_subagents_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let tasks = vec![
        task(dir.path(), "a", "alpha"),
        task(dir.path(), "b", "bravo"),
        task(dir.path(), "c", "charlie"),
    ];
    let results = run_parallel(tasks, 2, tx).await;

    assert_eq!(results.len(), 3);
    let summaries: Vec<String> = results
        .into_iter()
        .map(|(label, outcome)| format!("{label}:{}", outcome.unwrap().summary))
        .collect();
    let joined = summaries.join(",");
    assert!(joined.contains("a:alpha"), "{joined}");
    assert!(joined.contains("b:bravo"), "{joined}");
    assert!(joined.contains("c:charlie"), "{joined}");
}

struct GatedProvider {
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for GatedProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let script = vec![
            ChatEvent::TextDelta("ok".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn gated_task(
    dir: &std::path::Path,
    label: &str,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
) -> FanoutTask {
    FanoutTask {
        label: label.into(),
        worker_index: 0,
        provider: Box::new(GatedProvider { in_flight, peak }),
        tools: ToolRegistry::builtins(),
        cx: ToolCx {
            cwd: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            home: None,
            mode: PermissionMode::AcceptEdits,
            live_mode: None,
            rules: Arc::new(RuleSet::default()),
            approver: Arc::new(AllowAll),
            cancel: CancellationToken::new(),
            sandbox_writable_roots: None,
        },
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 3,
        hooks: Arc::new(HookHost::empty(dir.to_path_buf())),
        compaction_provider: None,
        system: "you are a worker".into(),
        messages: vec![Message::user("go")],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn never_exceeds_the_concurrency_cap() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<FanoutTask> = (0..6)
        .map(|i| gated_task(dir.path(), &format!("t{i}"), in_flight.clone(), peak.clone()))
        .collect();

    let cap = 2;
    let results = run_parallel(tasks, cap, tx).await;

    assert_eq!(results.len(), 6);
    assert!(results.iter().all(|(_, outcome)| outcome.is_ok()));
    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak > 0 && observed_peak <= cap,
        "peak in-flight {observed_peak} must respect cap {cap}"
    );
    assert_eq!(
        in_flight.load(Ordering::SeqCst),
        0,
        "all permits released after completion"
    );
}

struct ErrorProvider;
#[async_trait]
impl LlmProvider for ErrorProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        Err(ProviderError::Auth("subagent provider exploded".into()))
    }
}

struct PanicProvider;
#[async_trait]
impl LlmProvider for PanicProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        panic!("subagent task panicked on purpose")
    }
}

fn task_with(dir: &std::path::Path, label: &str, provider: Box<dyn LlmProvider>) -> FanoutTask {
    FanoutTask {
        label: label.into(),
        worker_index: 0,
        provider,
        tools: ToolRegistry::builtins(),
        cx: ToolCx {
            cwd: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            home: None,
            mode: PermissionMode::AcceptEdits,
            live_mode: None,
            rules: Arc::new(RuleSet::default()),
            approver: Arc::new(AllowAll),
            cancel: CancellationToken::new(),
            sandbox_writable_roots: None,
        },
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 3,
        hooks: Arc::new(HookHost::empty(dir.to_path_buf())),
        compaction_provider: None,
        system: "you are a worker".into(),
        messages: vec![Message::user("go")],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn surfaces_subagent_error_and_panic_without_aborting_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let tasks = vec![
        task(dir.path(), "ok", "fine"),
        task_with(dir.path(), "boom", Box::new(ErrorProvider)),
        task_with(dir.path(), "panic", Box::new(PanicProvider)),
    ];
    let results = run_parallel(tasks, 3, tx).await;

    assert_eq!(results.len(), 3, "every task yields a result pair");

    let ok = results
        .iter()
        .find(|(label, _)| label == "ok")
        .expect("the healthy task is present");
    assert_eq!(
        ok.1.as_ref().unwrap().summary,
        "fine",
        "the healthy sub-agent still completes alongside a failing/panicking peer"
    );

    let errored = results
        .iter()
        .find(|(label, _)| label == "boom")
        .expect("the erroring task is present");
    match &errored.1 {
        Err(stepper_core::CoreError::Provider(ProviderError::Auth(msg))) => {
            assert!(msg.contains("exploded"), "error detail propagated: {msg}");
        }
        Err(other) => panic!("expected a Provider/Auth error, got {other:?}"),
        Ok(outcome) => panic!("expected the sub-agent Err to surface, got Ok({})", outcome.summary),
    }

    let panicked = results
        .iter()
        .find(|(label, _)| label == "<panicked>")
        .expect("a panicking task is relabeled '<panicked>' by the JoinError branch");
    match &panicked.1 {
        Err(stepper_core::CoreError::Io(_)) => {}
        Err(other) => panic!("expected the JoinError to become CoreError::Io, got {other:?}"),
        Ok(outcome) => panic!("expected a panic Err, got Ok({})", outcome.summary),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_worker_still_emits_a_terminal_worker_finished() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            log2.lock().unwrap().push(format!("{ev:?}"));
        }
    });

    let mut healthy = task(dir.path(), "ok", "fine");
    healthy.worker_index = 0;
    let mut boom = task_with(dir.path(), "boom", Box::new(PanicProvider));
    boom.worker_index = 1;
    let results = run_parallel(vec![healthy, boom], 2, tx).await;
    assert_eq!(results.len(), 2);

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let events = log.lock().unwrap().join("\n");
    assert_eq!(events.matches("WorkerStarted").count(), 2, "both workers announced: {events}");
    assert!(
        events.contains("WorkerFinished { index: 0"),
        "the healthy worker finishes: {events}"
    );
    assert!(
        events.contains("WorkerFinished { index: 1"),
        "the panicked worker still reaches a terminal status via the Drop guard (no stuck row): {events}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_cap_is_clamped_to_serial_execution() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<FanoutTask> = (0..3)
        .map(|i| gated_task(dir.path(), &format!("s{i}"), in_flight.clone(), peak.clone()))
        .collect();

    let results = run_parallel(tasks, 0, tx).await;

    assert_eq!(results.len(), 3);
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "a cap of 0 is clamped to 1 (serial), never unbounded"
    );
}
