//! `Esc`/`Action::Interrupt` end-to-end through the real `spawn_core` loop. A
//! provider streams one token then blocks until its cancellation token fires, so
//! a turn would hang forever without interruption. Sending `Action::Interrupt`
//! must cancel just that turn (emitting `TurnComplete`) and leave the loop able
//! to run the next turn — the regression for the unwired-Esc / wedged-queue bug.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use stepper_core::{
    spawn_core, CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver,
    SessionRecord, StepDef,
};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{Action, AppEvent, EventRx};
use stepper_provider::{ChatEvent, ChatRequest, ChatStream, LlmProvider, ProviderError};
use stepper_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Yields one text delta, then blocks on its cancel token before ending — so the
/// turn only finishes when interrupted.
struct BlockingProvider;

#[async_trait]
impl LlmProvider for BlockingProvider {
    fn provider(&self) -> &str {
        "solo"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        let stream = futures::stream::unfold(0u8, move |i| {
            let cancel = cancel.clone();
            async move {
                match i {
                    0 => Some((
                        Ok::<ChatEvent, ProviderError>(ChatEvent::TextDelta("working".into())),
                        1u8,
                    )),
                    1 => {
                        // Block until the turn's cancel token fires (Interrupt),
                        // then surface a transport cancellation like the real SSE.
                        cancel.cancelled().await;
                        Some((Err(ProviderError::Cancelled), 2u8))
                    }
                    _ => None,
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

struct SoloResolver;

impl ProviderResolver for SoloResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "solo/m" => Ok(Box::new(BlockingProvider)),
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

fn orchestrator(root: std::path::PathBuf) -> Orchestrator {
    Orchestrator {
        resolver: Arc::new(SoloResolver),
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

async fn wait_for(rx: &mut EventRx, pred: impl Fn(&AppEvent) -> bool) {
    while let Some(ev) = rx.recv().await {
        if pred(&ev) {
            return;
        }
    }
    panic!("event stream ended before the awaited event");
}

/// Drain to `TurnComplete { turn_id: n }`, asserting no `Error` slips out — an
/// interrupt must be a clean cancellation, not a surfaced layer failure.
async fn expect_clean_complete(rx: &mut EventRx, n: u64) {
    while let Some(ev) = rx.recv().await {
        match ev {
            AppEvent::Error(e) => panic!("interrupt surfaced a spurious error: {e}"),
            AppEvent::TurnComplete { turn_id } if turn_id == n => return,
            _ => {}
        }
    }
    panic!("event stream ended before TurnComplete {n}");
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_ends_a_hanging_turn_and_does_not_wedge_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    // Turn 1: starts streaming, then blocks. Interrupt must end it.
    action_tx
        .send(Action::SubmitInput("one".into()))
        .await
        .unwrap();
    wait_for(&mut events, |e| {
        matches!(e, AppEvent::AssistantTokenDelta(_))
    })
    .await;
    action_tx.send(Action::Interrupt).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), expect_clean_complete(&mut events, 1))
        .await
        .expect("turn 1 must complete after Interrupt (it hangs otherwise)");

    // Turn 2: the loop is not wedged — it accepts and runs another turn.
    action_tx
        .send(Action::SubmitInput("two".into()))
        .await
        .unwrap();
    wait_for(&mut events, |e| {
        matches!(e, AppEvent::AssistantTokenDelta(_))
    })
    .await;
    action_tx.send(Action::Interrupt).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), expect_clean_complete(&mut events, 2))
        .await
        .expect("turn 2 must complete — the action loop wedged after the first interrupt");
}

#[tokio::test(flavor = "multi_thread")]
async fn slash_command_forwarded_mid_turn_runs_after_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let (action_tx, action_rx) = mpsc::channel(64);
    let mut events = spawn_core(
        orchestrator(dir.path().to_path_buf()),
        SessionRecord::fresh(),
        action_rx,
        CancellationToken::new(),
    );

    action_tx
        .send(Action::SubmitInput("one".into()))
        .await
        .unwrap();
    wait_for(&mut events, |e| matches!(e, AppEvent::AssistantTokenDelta(_))).await;
    // A slash command forwarded while the turn runs must be deferred, not dropped.
    action_tx
        .send(Action::SlashCommand {
            name: "help".into(),
            args: String::new(),
        })
        .await
        .unwrap();
    action_tx.send(Action::Interrupt).await.unwrap();

    let ran_after = tokio::time::timeout(Duration::from_secs(5), async {
        let mut turn_done = false;
        while let Some(ev) = events.recv().await {
            match ev {
                AppEvent::TurnComplete { turn_id: 1 } => turn_done = true,
                AppEvent::Notice { text, .. } if turn_done && text.contains("/help") => return true,
                _ => {}
            }
        }
        false
    })
    .await
    .expect("timed out waiting for the deferred /help");
    assert!(ran_after, "the mid-turn /help must run after the turn, not be dropped");
}
