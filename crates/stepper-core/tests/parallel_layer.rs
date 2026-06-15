//! Structural parallel layer: a sequential `plan` layer declares a task list via
//! `assign_tasks`; the next layer (marked `parallel`) fans out one worker per
//! task — each its own context window, running concurrently — and their
//! summaries converge into one handoff that the following `test` layer receives.
//! Also covers the fallback (a parallel layer with no prior task list runs once)
//! and that per-worker progress events reach the channel.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use stepper_core::{
    CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver, StepDef,
};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_protocol::AppEvent;
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason, Usage,
};
use stepper_tools::{Approval, Approver, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A planner that (optionally) calls `assign_tasks` on its first turn, then
/// finishes. With `tasks` empty it just finishes — no fan-out list.
struct PlanProvider {
    calls: Mutex<usize>,
    tasks: serde_json::Value,
}
#[async_trait]
impl LlmProvider for PlanProvider {
    fn provider(&self) -> &str {
        "plan"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        _r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let n = {
            let mut c = self.calls.lock().unwrap();
            let i = *c;
            *c += 1;
            i
        };
        let script = if n == 0 && self.tasks.as_array().is_some_and(|a| !a.is_empty()) {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "assign".into(),
                    name: "assign_tasks".into(),
                    input: serde_json::json!({ "tasks": self.tasks }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta("planned".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

/// One parallel worker: records its incoming subtask and finishes.
struct WorkerProvider {
    runs: Arc<AtomicUsize>,
    seen_subtasks: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl LlmProvider for WorkerProvider {
    fn provider(&self) -> &str {
        "impl"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if let Some(first) = r.messages.first() {
            self.seen_subtasks.lock().unwrap().push(first.text());
        }
        Ok(Box::pin(futures::stream::iter(
            vec![
                ChatEvent::TextDelta("built".into()),
                ChatEvent::Usage(Usage {
                    input: 40,
                    output: 12,
                    cache_read: 0,
                    cache_write: 0,
                }),
                ChatEvent::Done(StopReason::EndTurn),
            ]
            .into_iter()
            .map(Ok),
        )))
    }
}

struct TestProvider {
    seen: Arc<Mutex<Option<String>>>,
}
#[async_trait]
impl LlmProvider for TestProvider {
    fn provider(&self) -> &str {
        "test"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        r: ChatRequest,
        _c: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        if self.seen.lock().unwrap().is_none()
            && let Some(first) = r.messages.first()
        {
            *self.seen.lock().unwrap() = Some(first.text());
        }
        Ok(Box::pin(futures::stream::iter(
            vec![
                ChatEvent::TextDelta("tested".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
            .into_iter()
            .map(Ok),
        )))
    }
}

struct PipelineResolver {
    plan_tasks: serde_json::Value,
    worker_runs: Arc<AtomicUsize>,
    worker_subtasks: Arc<Mutex<Vec<String>>>,
    test_seen: Arc<Mutex<Option<String>>>,
}
impl ProviderResolver for PipelineResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "plan/m" => Ok(Box::new(PlanProvider {
                calls: Mutex::new(0),
                tasks: self.plan_tasks.clone(),
            })),
            "impl/m" => Ok(Box::new(WorkerProvider {
                runs: self.worker_runs.clone(),
                seen_subtasks: self.worker_subtasks.clone(),
            })),
            "test/m" => Ok(Box::new(TestProvider {
                seen: self.test_seen.clone(),
            })),
            other => Err(CoreError::NoModel(other.to_string())),
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

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _a: Approval) -> Decision {
        Decision::Allow
    }
}

fn step(name: &str, model_ref: &str, parallel: bool) -> StepDef {
    StepDef {
        name: name.into(),
        model_ref: model_ref.into(),
        system_prompt: format!("you are {name}"),
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
        parallel,
        parallel_max: 8,
        skills: Vec::new(),
    }
}

fn pipeline(resolver: Arc<dyn ProviderResolver>, root: std::path::PathBuf) -> Orchestrator {
    Orchestrator {
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![
            step("plan", "plan/m", false),
            step("implement", "impl/m", true),
            step("test", "test/m", false),
        ],
        base_context: "ctx".into(),
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

/// Collect every event into a shared log so the test can assert on worker events.
fn collecting_channel() -> (mpsc::Sender<AppEvent>, Arc<Mutex<Vec<String>>>) {
    let (tx, mut rx) = mpsc::channel(512);
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            log2.lock().unwrap().push(format!("{ev:?}"));
        }
    });
    (tx, log)
}

#[tokio::test(flavor = "multi_thread")]
async fn fans_out_parallel_layer_from_prior_task_list() {
    let dir = tempfile::tempdir().unwrap();
    let worker_runs = Arc::new(AtomicUsize::new(0));
    let worker_subtasks = Arc::new(Mutex::new(Vec::new()));
    let test_seen = Arc::new(Mutex::new(None));
    let resolver = Arc::new(PipelineResolver {
        plan_tasks: serde_json::json!([
            { "label": "api", "prompt": "build the api" },
            { "label": "db", "prompt": "build the db" }
        ]),
        worker_runs: worker_runs.clone(),
        worker_subtasks: worker_subtasks.clone(),
        test_seen: test_seen.clone(),
    });
    let orch = pipeline(resolver, dir.path().to_path_buf());

    let (tx, log) = collecting_channel();
    let summaries = orch
        .run_turn("ship it".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap()
        .summaries;

    // One worker ran per declared subtask.
    assert_eq!(worker_runs.load(Ordering::SeqCst), 2, "two workers must run");

    // Each worker received its own assigned subtask (fresh context windows).
    let subtasks = worker_subtasks.lock().unwrap().clone();
    assert_eq!(subtasks.len(), 2);
    assert!(subtasks.iter().any(|s| s.contains("build the api")), "{subtasks:?}");
    assert!(subtasks.iter().any(|s| s.contains("build the db")), "{subtasks:?}");

    // The implement layer's converged summary carries both workers, by label.
    let implement = &summaries[1];
    assert_eq!(implement.0, "implement");
    assert!(implement.1.contains("## api") && implement.1.contains("## db"), "{implement:?}");

    // The following sequential layer receives the converged summary as handoff.
    let test_input = test_seen.lock().unwrap().clone().unwrap();
    assert!(test_input.contains("## api") && test_input.contains("## db"), "{test_input}");

    // Per-worker progress events were emitted (panel data).
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let events = log.lock().unwrap().join("\n");
    assert_eq!(events.matches("WorkerStarted").count(), 2, "two WorkerStarted: {events}");
    assert_eq!(events.matches("WorkerFinished").count(), 2, "two WorkerFinished");
    assert!(
        events.contains("WorkerActivity") && events.contains("tokens: Some(52)"),
        "per-worker token activity (40+12) emitted: {events}"
    );

    // Worker-mode suppression: the workers' "built" text and their usage must NOT
    // leak onto the global stream (which would garble the shared buffer/footer) —
    // they surface only via the per-worker panel. The lone converged-summary
    // AssistantTokenDelta carries the merged "## api … ## db" text, never bare "built".
    assert!(
        !events.contains("AssistantTokenDelta(\"built\")"),
        "a worker's token stream leaked globally: {events}"
    );
    assert!(
        !events.contains("UsageUpdated"),
        "a worker's usage leaked as a global footer update: {events}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_assigned_task_runs_even_beyond_parallel_max() {
    let dir = tempfile::tempdir().unwrap();
    let worker_runs = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(PipelineResolver {
        plan_tasks: serde_json::json!([
            { "label": "a", "prompt": "task a" },
            { "label": "b", "prompt": "task b" },
            { "label": "c", "prompt": "task c" },
            { "label": "d", "prompt": "task d" },
            { "label": "e", "prompt": "task e" }
        ]),
        worker_runs: worker_runs.clone(),
        worker_subtasks: Arc::new(Mutex::new(Vec::new())),
        test_seen: Arc::new(Mutex::new(None)),
    });
    let mut orch = pipeline(resolver, dir.path().to_path_buf());
    // parallel_max caps how many run AT ONCE, not how many exist — no task is dropped.
    orch.steps[1].parallel_max = 2;

    let (tx, _log) = collecting_channel();
    let summaries = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap()
        .summaries;

    assert_eq!(
        worker_runs.load(Ordering::SeqCst),
        5,
        "all 5 assigned tasks must run even though parallel_max is 2 (cap is concurrency, not work)"
    );
    let merged = &summaries[1].1;
    for label in ["## a", "## b", "## c", "## d", "## e"] {
        assert!(merged.contains(label), "converged summary must include {label}: {merged}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_over_large_fan_out_is_rejected_not_materialized() {
    let dir = tempfile::tempdir().unwrap();
    let worker_runs = Arc::new(AtomicUsize::new(0));
    // 33 tasks > the 32-worker hard ceiling — must abort, not spawn 33 workers.
    let tasks: Vec<serde_json::Value> = (0..33)
        .map(|i| serde_json::json!({ "label": format!("t{i}"), "prompt": format!("do {i}") }))
        .collect();
    let resolver = Arc::new(PipelineResolver {
        plan_tasks: serde_json::Value::Array(tasks),
        worker_runs: worker_runs.clone(),
        worker_subtasks: Arc::new(Mutex::new(Vec::new())),
        test_seen: Arc::new(Mutex::new(None)),
    });
    let orch = pipeline(resolver, dir.path().to_path_buf());

    let (tx, _log) = collecting_channel();
    let result = orch
        .run_turn("go".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await;

    assert!(result.is_err(), "an over-limit fan-out fails the layer (on_failure=Stop)");
    assert_eq!(
        worker_runs.load(Ordering::SeqCst),
        0,
        "no workers are materialized when the list exceeds the ceiling"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_layer_without_a_task_list_runs_as_a_single_layer() {
    let dir = tempfile::tempdir().unwrap();
    let worker_runs = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(PipelineResolver {
        // Planner declares NO tasks → the parallel layer has nothing to fan out.
        plan_tasks: serde_json::json!([]),
        worker_runs: worker_runs.clone(),
        worker_subtasks: Arc::new(Mutex::new(Vec::new())),
        test_seen: Arc::new(Mutex::new(None)),
    });
    let orch = pipeline(resolver, dir.path().to_path_buf());

    let (tx, log) = collecting_channel();
    let summaries = orch
        .run_turn("ship it".into(), Vec::new(), &tx, Arc::new(AllowAll), CancellationToken::new())
        .await
        .unwrap()
        .summaries;

    assert_eq!(summaries.len(), 3, "all three layers still run");
    assert_eq!(
        worker_runs.load(Ordering::SeqCst),
        1,
        "with no task list the parallel layer runs once, sequentially"
    );
    assert_eq!(summaries[1].0, "implement");
    assert_eq!(summaries[1].1, "built", "single-run summary is the layer's own text");

    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let events = log.lock().unwrap().join("\n");
    assert!(!events.contains("WorkerStarted"), "no fan-out, so no worker events: {events}");
}
