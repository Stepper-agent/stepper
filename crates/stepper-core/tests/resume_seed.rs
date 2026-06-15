//! Full-fidelity sessions through the real `spawn_core` path: a turn persists
//! its real message transcript (assistant blocks, tool calls/results — thinking
//! stripped), resuming seeds the next request with those REAL prior messages
//! (the provider sees them, ahead of the new user turn, with the system prompt
//! rebuilt fresh), and old-format records (digest only) fall back to the
//! markdown-digest seeding in `base_context`.

use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use stepper_core::{
    spawn_core, CoreError, FailurePolicy, HookHost, ModelInfo, Orchestrator, ProviderResolver,
    SessionRecord, SessionStore, StepDef, TurnRecord,
};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::{Action, AppEvent, ApprovalDecision, EventRx};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, ContentBlock, LlmProvider, Message, ProviderError, Role,
    StopReason, ToolContent,
};
use stepper_tools::ToolRegistry;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Records every incoming request (messages + system) and replies per script:
/// with `tool_first`, the first call issues a `list_dir` tool call (preceded by
/// a thinking delta), the second ends the turn; otherwise every call is a plain
/// text reply.
struct RecordingProvider {
    tool_first: bool,
    calls: Arc<AtomicUsize>,
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
    seen_systems: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    fn provider(&self) -> &str {
        "solo"
    }
    fn model(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.seen_messages.lock().unwrap().push(request.messages.clone());
        self.seen_systems
            .lock()
            .unwrap()
            .push(request.system.clone().unwrap_or_default());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let script = if self.tool_first && n == 0 {
            vec![
                ChatEvent::ThinkingDelta("pondering the layout".into()),
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "c1".into(),
                    name: "list_dir".into(),
                    input: serde_json::json!({ "path": "." }),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta("all done".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

struct RecordingResolver {
    tool_first: bool,
    calls: Arc<AtomicUsize>,
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
    seen_systems: Arc<Mutex<Vec<String>>>,
}

impl ProviderResolver for RecordingResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        match model_ref {
            "solo/m" => Ok(Box::new(RecordingProvider {
                tool_first: self.tool_first,
                calls: self.calls.clone(),
                seen_messages: self.seen_messages.clone(),
                seen_systems: self.seen_systems.clone(),
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
        steps: vec![step()],
        base_context: "base ctx".into(),
        project_root: root.clone(),
        cwd: root.clone(),
        home: None,
        rules: Arc::new(RuleSet::default()),
        mode: PermissionMode::AcceptEdits,
        hooks: Arc::new(HookHost::empty(root)),
        always_load_mcp: Vec::new(),
        compaction_model: None,
        dispatch_enabled: false,
        limits: stepper_core::SessionLimits::default(),
        fallback_model: None,
        resume_seed: Vec::new(),
    }
}

/// Drain events until `TurnComplete`, auto-approving any permission gate.
async fn drive_turn(events: &mut EventRx) {
    while let Some(ev) = events.recv().await {
        match ev {
            AppEvent::ApprovalRequested(req) => {
                let _ = req.reply.send(ApprovalDecision::AllowOnce);
            }
            AppEvent::TurnComplete { .. } => return,
            _ => {}
        }
    }
    panic!("event stream ended before TurnComplete");
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_persists_the_full_message_transcript_with_thinking_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let resolver = Arc::new(RecordingResolver {
        tool_first: true,
        calls: Arc::default(),
        seen_messages: Arc::default(),
        seen_systems: Arc::default(),
    });
    let orch = orchestrator(resolver, root.clone());

    let session = SessionRecord::fresh();
    let session_id = session.id.clone();
    let (action_tx, action_rx) = mpsc::channel(16);
    let mut events = spawn_core(orch, session, action_rx, CancellationToken::new());
    action_tx
        .send(Action::SubmitInput("inspect the project".into()))
        .await
        .unwrap();
    drive_turn(&mut events).await;

    let persisted = SessionStore::new(&root).load(&session_id).expect("session persisted");
    assert_eq!(persisted.turns.len(), 1);
    assert_eq!(persisted.turns[0].summaries, vec![("solo".into(), "all done".into())]);

    let messages = &persisted.turns[0].messages;
    assert_eq!(
        messages.len(),
        4,
        "user + assistant(tool call) + tool result + assistant(text): {messages:#?}"
    );
    assert!(matches!(messages[0].role, Role::User));
    assert!(messages[0].text().contains("inspect the project"));
    assert!(matches!(messages[1].role, Role::Assistant));
    assert!(matches!(
        &messages[1].content[0],
        ContentBlock::ToolUse { id, name, .. } if id == "c1" && name == "list_dir"
    ));
    assert!(matches!(messages[2].role, Role::Tool));
    assert!(matches!(
        &messages[2].content[0],
        ContentBlock::ToolResult { tool_call_id, is_error: false, .. } if tool_call_id == "c1"
    ));
    assert!(matches!(messages[3].role, Role::Assistant));
    assert_eq!(messages[3].text(), "all done");
    let any_thinking = messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::Thinking { .. }));
    assert!(!any_thinking, "thinking blocks are stripped from the persisted transcript");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_session_remembers_prior_turns_without_resume() {
    // Two turns in ONE running session: the second request must carry the first
    // turn's messages. (Regression: resume_seed was only ever set from --resume,
    // so a live session forgot everything between turns.)
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let seen_messages: Arc<Mutex<Vec<Vec<Message>>>> = Arc::default();
    let resolver = Arc::new(RecordingResolver {
        tool_first: false,
        calls: Arc::default(),
        seen_messages: seen_messages.clone(),
        seen_systems: Arc::default(),
    });
    let orch = orchestrator(resolver, root.clone());

    let (action_tx, action_rx) = mpsc::channel(16);
    let mut events = spawn_core(orch, SessionRecord::fresh(), action_rx, CancellationToken::new());

    action_tx
        .send(Action::SubmitInput("remember MAGIC_TOKEN_42".into()))
        .await
        .unwrap();
    drive_turn(&mut events).await;
    action_tx
        .send(Action::SubmitInput("what did I tell you?".into()))
        .await
        .unwrap();
    drive_turn(&mut events).await;

    let requests = seen_messages.lock().unwrap();
    assert_eq!(requests[0].len(), 1, "turn 1 starts fresh: {:#?}", requests[0]);
    let second = &requests[1];
    assert!(
        second.len() >= 3,
        "turn 2 must include the prior turn (user + reply) before the new prompt: {second:#?}"
    );
    assert!(
        second.iter().any(|m| m.text().contains("MAGIC_TOKEN_42")),
        "turn 2 remembers what turn 1 said: {second:#?}"
    );
    assert!(second.last().unwrap().text().contains("what did I tell you"));
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_seeds_the_real_prior_messages_into_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = SessionStore::new(&root);

    let prior = SessionRecord {
        id: "prior".into(),
        name: None,
        turns: vec![TurnRecord {
            user: "earlier ask".into(),
            summaries: vec![("solo".into(), "prior answer".into())],
            messages: vec![
                Message::user("earlier ask"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "t9".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({ "path": "a.txt" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: "t9".into(),
                        content: vec![ToolContent::text("old file body")],
                        is_error: false,
                    }],
                },
                Message::assistant("prior answer"),
            ],
        }],
    };
    store.save(&prior).unwrap();
    let loaded = store.load("prior").expect("saved session reloads");

    let seen_messages: Arc<Mutex<Vec<Vec<Message>>>> = Arc::default();
    let seen_systems: Arc<Mutex<Vec<String>>> = Arc::default();
    let resolver = Arc::new(RecordingResolver {
        tool_first: false,
        calls: Arc::default(),
        seen_messages: seen_messages.clone(),
        seen_systems: seen_systems.clone(),
    });
    let orch = orchestrator(resolver, root.clone());

    let (action_tx, action_rx) = mpsc::channel(16);
    let mut events = spawn_core(orch, loaded, action_rx, CancellationToken::new());
    action_tx
        .send(Action::SubmitInput("follow up".into()))
        .await
        .unwrap();
    drive_turn(&mut events).await;

    let requests = seen_messages.lock().unwrap();
    let request = &requests[0];
    assert_eq!(
        request.len(),
        5,
        "4 seeded prior messages + the new user turn: {request:#?}"
    );
    assert_eq!(request[0].text(), "earlier ask");
    assert!(matches!(
        &request[1].content[0],
        ContentBlock::ToolUse { id, name, .. } if id == "t9" && name == "read_file"
    ));
    assert!(matches!(
        &request[2].content[0],
        ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == "t9"
    ));
    assert_eq!(request[3].text(), "prior answer");
    assert!(matches!(request[4].role, Role::User));
    assert!(request[4].text().contains("follow up"));

    // The system prompt is rebuilt fresh, never persisted: base context + layer
    // prompt only, no digest of the prior session.
    let system = seen_systems.lock().unwrap()[0].clone();
    assert!(system.contains("base ctx"));
    assert!(system.contains("you are solo"));
    assert!(!system.contains("Earlier in this session"));
    assert!(!system.contains("prior answer"));

    // The resumed turn appends WITHOUT re-saving the seeded history.
    let after = store.load("prior").expect("session persisted after resume");
    assert_eq!(after.turns.len(), 2);
    let new_messages = &after.turns[1].messages;
    assert_eq!(new_messages.len(), 2, "just this turn's user + reply: {new_messages:#?}");
    assert!(new_messages[0].text().contains("follow up"));
    assert_eq!(new_messages[1].text(), "all done");
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_drops_the_resume_seed_for_later_turns() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let store = SessionStore::new(&root);
    let prior = SessionRecord {
        id: "prior".into(),
        name: None,
        turns: vec![TurnRecord {
            user: "earlier ask".into(),
            summaries: vec![("solo".into(), "prior answer".into())],
            messages: vec![Message::user("earlier ask"), Message::assistant("prior answer")],
        }],
    };
    store.save(&prior).unwrap();

    let seen_messages: Arc<Mutex<Vec<Vec<Message>>>> = Arc::default();
    let resolver = Arc::new(RecordingResolver {
        tool_first: false,
        calls: Arc::default(),
        seen_messages: seen_messages.clone(),
        seen_systems: Arc::default(),
    });
    let orch = orchestrator(resolver, root.clone());

    let (action_tx, action_rx) = mpsc::channel(16);
    let mut events = spawn_core(
        orch,
        store.load("prior").unwrap(),
        action_rx,
        CancellationToken::new(),
    );
    action_tx
        .send(Action::SlashCommand {
            name: "clear".into(),
            args: String::new(),
        })
        .await
        .unwrap();
    action_tx
        .send(Action::SubmitInput("start over".into()))
        .await
        .unwrap();
    drive_turn(&mut events).await;

    let requests = seen_messages.lock().unwrap();
    let request = &requests[0];
    assert_eq!(
        request.len(),
        1,
        "/clear must drop the seeded prior conversation: {request:#?}"
    );
    assert!(request[0].text().contains("start over"));
}

#[tokio::test(flavor = "multi_thread")]
async fn old_format_record_falls_back_to_the_digest_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let sessions = root.join(".stepper").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join("old.json"),
        r#"{ "id": "old", "turns": [ { "user": "earlier ask", "summaries": [["solo", "prior answer"]] } ] }"#,
    )
    .unwrap();
    let loaded = SessionStore::new(&root).load("old").expect("old-format file loads");

    let seen_messages: Arc<Mutex<Vec<Vec<Message>>>> = Arc::default();
    let seen_systems: Arc<Mutex<Vec<String>>> = Arc::default();
    let resolver = Arc::new(RecordingResolver {
        tool_first: false,
        calls: Arc::default(),
        seen_messages: seen_messages.clone(),
        seen_systems: seen_systems.clone(),
    });
    let orch = orchestrator(resolver, root.clone());

    let (action_tx, action_rx) = mpsc::channel(16);
    let mut events = spawn_core(orch, loaded, action_rx, CancellationToken::new());
    action_tx
        .send(Action::SubmitInput("follow up".into()))
        .await
        .unwrap();
    drive_turn(&mut events).await;

    let requests = seen_messages.lock().unwrap();
    let request = &requests[0];
    assert_eq!(request.len(), 1, "no message seed for a digest-only record: {request:#?}");
    assert!(request[0].text().contains("follow up"));

    let system = seen_systems.lock().unwrap()[0].clone();
    assert!(
        system.contains("Earlier in this session") && system.contains("prior answer"),
        "digest fallback seeds the prior turn via the system context: {system}"
    );
}
