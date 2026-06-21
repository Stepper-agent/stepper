//! Live end-to-end pipeline: a two-layer turn where layer one (plan) runs on
//! ollama-cloud and layer two (implement) runs on a local oMLX server, driven
//! through the real `spawn_core` channel contract with the real
//! `ConfigProviderResolver`. Asserts the handoff carried layer one's summary,
//! the implement layer actually wrote a file via the `write_file` tool, the
//! session persisted, real usage came back, and `/rewind` prunes the work.
//!
//! `#[ignore]` + env-gated — the default `cargo test` reports it ignored. Run:
//!   STEPPER_E2E=1 \
//!   STEPPER_OLLAMA_CLOUD_API_KEY=... \
//!   STEPPER_OMLX_BASE_URL=http://localhost:8000/v1 \
//!   STEPPER_E2E_OLLAMA_MODEL=qwen3-coder STEPPER_E2E_OMLX_MODEL=deepseek-coder \
//!   cargo test -p stepper-core --test e2e_live -- --ignored --nocapture

use std::sync::Arc;
use std::time::Duration;

use stepper_config::{Config, ProviderConfig};
use stepper_core::{
    spawn_core, ConfigProviderResolver, FailurePolicy, HookHost, ModelRegistry, Orchestrator,
    SessionRecord, SessionStore, StepDef,
};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{Action, AppEvent, ApprovalDecision, EventRx};
use stepper_providers::ProviderFactory;
use stepper_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn e2e_on() -> bool {
    std::env::var("STEPPER_E2E").as_deref() == Ok("1")
}

fn compat_provider(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        kind: "openai-compat".into(),
        base_url: Some(base_url.into()),
        api_key: None,
        auth: None,
        default_model: None,
        context_window: None,
        models: Default::default(),
    }
}

fn step(name: &str, model_ref: &str, system: &str, cap: usize) -> StepDef {
    StepDef {
        name: name.into(),
        model_ref: model_ref.into(),
        system_prompt: system.into(),
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

struct TurnReport {
    usages: Vec<stepper_protocol::UsageView>,
    layers_finished: usize,
    last_turn_id: Option<u64>,
    error: Option<String>,
}

/// Drain one turn: auto-approve any gate, collect usage + layer completions,
/// stop at `TurnComplete`. A live model call is slow, so the per-event timeout
/// is generous.
async fn run_turn(events: &mut EventRx) -> TurnReport {
    let mut report = TurnReport {
        usages: Vec::new(),
        layers_finished: 0,
        last_turn_id: None,
        error: None,
    };
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(180), events.recv())
            .await
            .expect("core stalled waiting for the live model")
            .expect("core closed the event channel before completing the turn");
        match ev {
            AppEvent::ApprovalRequested(req) => {
                let _ = req.reply.send(ApprovalDecision::AllowOnce);
            }
            AppEvent::UsageUpdated(u) => report.usages.push(u),
            AppEvent::LayerFinished { .. } => report.layers_finished += 1,
            AppEvent::Error(e) => report.error = Some(e),
            AppEvent::TurnComplete { turn_id } => {
                report.last_turn_id = Some(turn_id);
                break;
            }
            _ => {}
        }
    }
    report
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: STEPPER_E2E=1 + STEPPER_OLLAMA_CLOUD_API_KEY + oMLX server"]
async fn two_layer_pipeline_writes_a_file_and_rewinds() {
    if !e2e_on() {
        eprintln!("skip two_layer_pipeline: set STEPPER_E2E=1");
        return;
    }
    if std::env::var("STEPPER_OLLAMA_CLOUD_API_KEY").is_err() {
        eprintln!("skip two_layer_pipeline: STEPPER_OLLAMA_CLOUD_API_KEY unset");
        return;
    }

    let ollama_model =
        std::env::var("STEPPER_E2E_OLLAMA_MODEL").unwrap_or_else(|_| "qwen3-coder".into());
    let omlx_model =
        std::env::var("STEPPER_E2E_OMLX_MODEL").unwrap_or_else(|_| "deepseek-coder".into());
    let omlx_base =
        std::env::var("STEPPER_OMLX_BASE_URL").unwrap_or_else(|_| "http://localhost:8000/v1".into());

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let mut config = Config::load(&root).expect("default config loads");
    config
        .settings
        .providers
        .insert("ollama-cloud".into(), compat_provider("https://ollama.com/v1"));
    config
        .settings
        .providers
        .insert("omlx".into(), compat_provider(&omlx_base));

    let factory = ProviderFactory::new().expect("reqwest client builds");
    let resolver = Arc::new(ConfigProviderResolver::new(
        config,
        factory,
        ModelRegistry::builtin(),
        None,
        None,
    ));

    let orch = Orchestrator {
        agents: Default::default(),
        formatters: Default::default(),
        lsp: Default::default(),
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![
            step(
                "plan",
                &format!("ollama-cloud/{ollama_model}"),
                "You are the planning layer. Output a short 2-line plan as plain text. Do NOT call any tools.",
                4,
            ),
            step(
                "implement",
                &format!("omlx/{omlx_model}"),
                "You are the implementation layer. Carry out the plan by creating the requested file using the write_file tool. The file content must be exactly the requested string.",
                8,
            ),
        ],
        base_context: "This is an end-to-end test workspace.".into(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(std::sync::RwLock::new(RuleSet::from_lists(
            &["Write(**)".into(), "Edit(**)".into(), "Read(**)".into()],
            &[],
            &[],
        ))),
        mode: Arc::new(std::sync::RwLock::new(PermissionMode::Auto)),
        hooks: Arc::new(HookHost::empty(root.clone())),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,        limits: stepper_core::SessionLimits::default(),
        fallback_models: Vec::new(),
        resume_seed: Vec::new(),
        sandbox_writable_roots: None,
    };

    let session = SessionRecord::fresh();
    let session_id = session.id.clone();
    let (action_tx, action_rx) = mpsc::channel(32);
    let mut events = spawn_core(orch, session, action_rx, CancellationToken::new());

    action_tx
        .send(Action::SubmitInput(
            "Create a file named hello.txt in the project directory whose entire content is exactly: stepper-e2e-ok".into(),
        ))
        .await
        .unwrap();

    let report = run_turn(&mut events).await;
    assert!(report.error.is_none(), "turn errored: {:?}", report.error);

    let hello = root.join("hello.txt");
    assert!(
        hello.exists(),
        "implement layer must have written hello.txt via write_file"
    );
    let content = std::fs::read_to_string(&hello).unwrap();
    assert!(
        content.contains("stepper-e2e-ok"),
        "file content should carry the requested string, got: {content:?}"
    );

    assert_eq!(report.layers_finished, 2, "both layers must finish");
    let total_tokens: u64 = report.usages.iter().map(|u| u.tokens_total()).sum();
    assert!(total_tokens > 0, "real usage tokens must come back from the endpoints");

    let persisted = SessionStore::new(&root)
        .load(&session_id)
        .expect("session persisted");
    assert_eq!(persisted.turns.len(), 1, "one user turn recorded");
    let summaries = &persisted.turns[0].summaries;
    assert_eq!(summaries.len(), 2, "both layer summaries recorded (the handoff carriers)");
    assert!(
        summaries.iter().all(|(_, s)| !s.trim().is_empty()),
        "each layer must produce a non-empty free-text outcome: {summaries:?}"
    );

    // /rewind to the pre-turn checkpoint must prune the file the run produced and
    // truncate the session back to zero turns — the real production path.
    action_tx
        .send(Action::Rewind {
            checkpoint_id: "turn-1".into(),
        })
        .await
        .unwrap();
    let notice = loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .expect("rewind stalled")
            .expect("channel closed");
        if let AppEvent::Notice { text, .. } = ev {
            break text;
        }
    };
    assert!(notice.contains("rewound to turn-1"), "got: {notice}");
    assert!(!hello.exists(), "rewind must prune the file created during the turn");
    let after = SessionStore::new(&root).load(&session_id).expect("session still loads");
    assert_eq!(after.turns.len(), 0, "rewind to turn-1 truncates all turns");
}

/// A cheap single-layer (ollama-cloud, text-only) orchestrator for the resume
/// path, which is about turn numbering + context seeding, not the 2-layer
/// pipeline.
fn ollama_orchestrator(
    root: &std::path::Path,
    base_context: String,
    ollama_model: &str,
) -> Orchestrator {
    let mut config = Config::load(root).expect("default config loads");
    config
        .settings
        .providers
        .insert("ollama-cloud".into(), compat_provider("https://ollama.com/v1"));
    let factory = ProviderFactory::new().expect("reqwest client builds");
    let resolver = Arc::new(ConfigProviderResolver::new(
        config,
        factory,
        ModelRegistry::builtin(),
        None,
        None,
    ));
    Orchestrator {
        agents: Default::default(),
        formatters: Default::default(),
        lsp: Default::default(),
        resolver,
        base_tools: ToolRegistry::builtins(),
        steps: vec![step(
            "plan",
            &format!("ollama-cloud/{ollama_model}"),
            "Reply with one short line of plain text. Do NOT call any tools.",
            3,
        )],
        base_context,
        project_root: root.to_path_buf(),
        cwd: root.to_path_buf(),
        home: None,
        rules: Arc::new(std::sync::RwLock::new(RuleSet::default())),
        mode: Arc::new(std::sync::RwLock::new(PermissionMode::Auto)),
        hooks: Arc::new(HookHost::empty(root.to_path_buf())),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,        limits: stepper_core::SessionLimits::default(),
        fallback_models: Vec::new(),
        resume_seed: Vec::new(),
        sandbox_writable_roots: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: STEPPER_E2E=1 + STEPPER_OLLAMA_CLOUD_API_KEY"]
async fn resume_continues_session_turn_numbering() {
    if !e2e_on() {
        eprintln!("skip resume: set STEPPER_E2E=1");
        return;
    }
    if std::env::var("STEPPER_OLLAMA_CLOUD_API_KEY").is_err() {
        eprintln!("skip resume: STEPPER_OLLAMA_CLOUD_API_KEY unset");
        return;
    }
    let ollama_model =
        std::env::var("STEPPER_E2E_OLLAMA_MODEL").unwrap_or_else(|_| "qwen3-coder".into());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let session = SessionRecord::fresh();
    let session_id = session.id.clone();
    let orch1 = ollama_orchestrator(&root, "Fresh session.".into(), &ollama_model);
    let (tx1, rx1) = mpsc::channel(16);
    let mut ev1 = spawn_core(orch1, session, rx1, CancellationToken::new());
    tx1.send(Action::SubmitInput("Say the word: alpha".into()))
        .await
        .unwrap();
    let r1 = run_turn(&mut ev1).await;
    assert!(r1.error.is_none(), "turn 1 errored: {:?}", r1.error);
    assert_eq!(r1.last_turn_id, Some(1));
    drop(tx1);

    let reloaded = SessionStore::new(&root)
        .load(&session_id)
        .expect("session persisted after turn 1");
    assert_eq!(reloaded.turns.len(), 1);
    assert!(
        reloaded.has_messages(),
        "turn 1 must persist its real message transcript for resume"
    );

    // `spawn_core` seeds the resumed run from the record itself (real prior
    // messages); the base context stays the caller's, rebuilt fresh.
    let orch2 = ollama_orchestrator(&root, "Fresh session.".into(), &ollama_model);
    let (tx2, rx2) = mpsc::channel(16);
    let mut ev2 = spawn_core(orch2, reloaded, rx2, CancellationToken::new());
    tx2.send(Action::SubmitInput("Say the word: beta".into()))
        .await
        .unwrap();
    let r2 = run_turn(&mut ev2).await;
    assert!(r2.error.is_none(), "turn 2 errored: {:?}", r2.error);
    assert_eq!(
        r2.last_turn_id,
        Some(2),
        "a resumed session continues turn numbering from where it left off"
    );

    let after = SessionStore::new(&root)
        .load(&session_id)
        .expect("session persisted after resume");
    assert_eq!(after.turns.len(), 2, "the resumed turn appends to the same session");
}
