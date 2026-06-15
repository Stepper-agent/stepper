//! End-to-end `/rewind` through the real `spawn_core` channel contract. Two
//! text-only turns each checkpoint the working tree; a file created after a
//! checkpoint is pruned when rewinding to it, while a file already present in
//! that checkpoint survives, and the persisted session is truncated with the
//! turn counter reset. This drives the production `Action::Rewind` handler in
//! `spawn_core` rather than re-implementing its arithmetic in the test.

use async_trait::async_trait;
use std::sync::Arc;
use stepper_core::{
    spawn_core, CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver,
    SessionRecord, SessionStore, StepDef,
};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{Action, AppEvent, EventRx};
use stepper_provider::{ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError, StopReason};
use stepper_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct TextProvider;

#[async_trait]
impl LlmProvider for TextProvider {
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
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct SoloResolver;

impl ProviderResolver for SoloResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "solo/m" => Ok(Box::new(TextProvider)),
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

async fn wait_turn_complete(rx: &mut EventRx, n: u64) {
    while let Some(ev) = rx.recv().await {
        if let AppEvent::TurnComplete { turn_id } = ev
            && turn_id == n
        {
            return;
        }
    }
    panic!("event stream ended before TurnComplete {n}");
}

async fn wait_notice(rx: &mut EventRx) -> String {
    while let Some(ev) = rx.recv().await {
        if let AppEvent::Notice { text, .. } = ev {
            return text;
        }
    }
    panic!("event stream ended before a Notice");
}

#[tokio::test(flavor = "multi_thread")]
async fn rewind_prunes_later_files_and_truncates_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let orch = Orchestrator {
        resolver: Arc::new(SoloResolver),
        base_tools: ToolRegistry::builtins(),
        steps: vec![step()],
        base_context: "ctx".into(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(RuleSet::default()),
        mode: PermissionMode::AcceptEdits,
        hooks: Arc::new(HookHost::empty(root.clone())),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        dispatch_concurrency: 8,
        dispatch_step_cap: None,        limits: stepper_core::SessionLimits::default(),
        fallback_model: None,
        resume_seed: Vec::new(),
    };

    let session = SessionRecord::fresh();
    let session_id = session.id.clone();

    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(orch, session, action_rx, CancellationToken::new());

    action_tx
        .send(Action::SubmitInput("one".into()))
        .await
        .unwrap();
    wait_turn_complete(&mut events, 1).await;
    std::fs::write(root.join("kept.txt"), "present at the turn-2 snapshot").unwrap();

    action_tx
        .send(Action::SubmitInput("two".into()))
        .await
        .unwrap();
    wait_turn_complete(&mut events, 2).await;
    std::fs::write(root.join("pruned.txt"), "created after the turn-2 snapshot").unwrap();

    assert!(root.join("kept.txt").exists());
    assert!(root.join("pruned.txt").exists());

    action_tx
        .send(Action::Rewind {
            checkpoint_id: "turn-2".into(),
        })
        .await
        .unwrap();
    let notice = wait_notice(&mut events).await;
    assert!(notice.contains("rewound to turn-2"), "got notice: {notice}");

    assert!(
        root.join("kept.txt").exists(),
        "a file present in the turn-2 snapshot must survive the rewind"
    );
    assert!(
        !root.join("pruned.txt").exists(),
        "a file created after the turn-2 snapshot must be pruned by the rewind"
    );

    let reloaded = SessionStore::new(&root)
        .load(&session_id)
        .expect("session persisted across the rewind");
    assert_eq!(
        reloaded.turns.len(),
        1,
        "rewinding to turn-2 keeps only turn 1"
    );
    assert_eq!(reloaded.turns[0].user, "one");
}
