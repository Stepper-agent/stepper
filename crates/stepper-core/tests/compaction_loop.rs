//! Compaction inside the live ReAct loop: a provider that reports usage above
//! the 0.70 soft threshold makes the loop emit a compaction round, and the system
//! prompt (the pinned prefix passed separately from the message history) is byte
//! identical on every request so compaction never rewrites it.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use stepper_core::{AgentLoop, HookHost, ModelInfo};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, LlmProvider, Message, ProviderError, StopReason, Usage,
};
use stepper_tools::{Approval, Approver, ToolCx, ToolRegistry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct RecordingProvider {
    calls: Mutex<usize>,
    systems: Mutex<Vec<Option<String>>>,
    message_counts: Mutex<Vec<usize>>,
    high_usage: Usage,
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    fn provider(&self) -> &str {
        "fake"
    }
    fn model(&self) -> &str {
        "fake-model"
    }
    async fn chat_stream(
        &self,
        request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        self.systems.lock().unwrap().push(request.system.clone());
        self.message_counts
            .lock()
            .unwrap()
            .push(request.messages.len());
        let index = {
            let mut c = self.calls.lock().unwrap();
            let i = *c;
            *c += 1;
            i
        };
        let script = if index == 0 {
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "call_1".into(),
                    name: "list_dir".into(),
                    input: serde_json::json!({ "path": "." }),
                },
                ChatEvent::Usage(self.high_usage),
                ChatEvent::Done(StopReason::ToolUse),
            ]
        } else {
            vec![
                ChatEvent::TextDelta("settled".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        };
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

#[tokio::test]
async fn compacts_above_soft_threshold_and_pins_the_system_prompt() {
    let dir = tempfile::tempdir().unwrap();

    let model_info = ModelInfo {
        context_window: 1000,
        max_output_tokens: 0,
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
        cache_read_per_mtok: 0.0,
        cache_write_per_mtok: 0.0,
        estimated: false,
    };
    let provider = RecordingProvider {
        calls: Mutex::new(0),
        systems: Mutex::new(Vec::new()),
        message_counts: Mutex::new(Vec::new()),
        high_usage: Usage {
            input: 800,
            output: 50,
            cache_read: 0,
            cache_write: 0,
        },
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
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
    };

    let pinned_system = "PINNED SYSTEM PROMPT — do not rewrite".to_string();
    let mut history = vec![Message::user("kick off a long conversation")];
    for i in 0..20 {
        history.push(Message::assistant(format!("filler turn {i}")));
        history.push(Message::user(format!("more {i}")));
    }

    let agent = AgentLoop {
        layer_name: "compact".into(),
        provider: &provider,
        tools: &registry,
        cx,
        event_tx: tx,
        model_info,
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        reasoning_effort: None,
        thinking_budget: None,
        worker: None,
    };

    let outcome = agent.drive(pinned_system.clone(), history).await.unwrap();
    assert_eq!(outcome.summary, "settled");

    let systems = provider.systems.lock().unwrap().clone();
    assert!(systems.len() >= 2, "expected multiple requests: {systems:?}");
    for s in systems.iter() {
        assert_eq!(
            s.as_deref(),
            Some(pinned_system.as_str()),
            "system prompt must be identical (pinned) on every request"
        );
    }

    let counts = provider.message_counts.lock().unwrap().clone();
    assert!(
        counts[1] < counts[0],
        "second request must carry a compacted (shorter) history: {counts:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let events = seen.lock().unwrap().join("\n");
    assert!(events.contains("CompactionStarted"), "events: {events}");
    assert!(events.contains("CompactionDone"), "events: {events}");
}

/// A model used only as the compaction summarizer: counts how often it is asked
/// to summarize and returns a recognizable summary line.
struct SummaryProvider {
    calls: Arc<Mutex<usize>>,
}

#[async_trait]
impl LlmProvider for SummaryProvider {
    fn provider(&self) -> &str {
        "summarizer"
    }
    fn model(&self) -> &str {
        "cheap-summary-model"
    }
    async fn chat_stream(
        &self,
        _request: ChatRequest,
        _cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        *self.calls.lock().unwrap() += 1;
        let script = vec![
            ChatEvent::TextDelta("MODEL SUMMARY: prior work condensed".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ];
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

#[tokio::test]
async fn uses_the_model_summarizer_when_a_compaction_provider_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let model_info = ModelInfo {
        context_window: 1000,
        max_output_tokens: 0,
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
        cache_read_per_mtok: 0.0,
        cache_write_per_mtok: 0.0,
        estimated: false,
    };
    let provider = RecordingProvider {
        calls: Mutex::new(0),
        systems: Mutex::new(Vec::new()),
        message_counts: Mutex::new(Vec::new()),
        high_usage: Usage {
            input: 800,
            output: 50,
            cache_read: 0,
            cache_write: 0,
        },
    };
    let summary_calls = Arc::new(Mutex::new(0));
    let compaction_provider: Arc<dyn LlmProvider> = Arc::new(SummaryProvider {
        calls: summary_calls.clone(),
    });

    let (tx, mut rx) = mpsc::channel(256);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let registry = ToolRegistry::builtins();
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
    };

    let mut history = vec![Message::user("kick off")];
    for i in 0..20 {
        history.push(Message::assistant(format!("filler {i}")));
        history.push(Message::user(format!("more {i}")));
    }

    let agent = AgentLoop {
        layer_name: "compact".into(),
        provider: &provider,
        tools: &registry,
        cx,
        event_tx: tx,
        model_info,
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: Some(compaction_provider),
        temperature: None,
        top_p: None,
        reasoning_effort: None,
        thinking_budget: None,
        worker: None,
    };

    agent.drive("system".into(), history).await.unwrap();

    assert_eq!(
        *summary_calls.lock().unwrap(),
        1,
        "the configured compaction provider must be asked to summarize exactly once"
    );
}

#[tokio::test]
async fn overflowing_seeded_history_is_compacted_before_the_first_request() {
    let dir = tempfile::tempdir().unwrap();

    let model_info = ModelInfo {
        context_window: 1000,
        max_output_tokens: 0,
        input_per_mtok: 0.0,
        output_per_mtok: 0.0,
        cache_read_per_mtok: 0.0,
        cache_write_per_mtok: 0.0,
        estimated: false,
    };
    // The provider never reports usage: only the chars/4 estimate of the seeded
    // history can trigger compaction, and it must do so on the very step that
    // overflows (step 0), not one step late.
    let provider = RecordingProvider {
        calls: Mutex::new(0),
        systems: Mutex::new(Vec::new()),
        message_counts: Mutex::new(Vec::new()),
        high_usage: Usage::default(),
    };

    let (tx, mut rx) = mpsc::channel(256);
    let seen = Arc::new(Mutex::new(Vec::<stepper_protocol::AppEvent>::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            seen2.lock().unwrap().push(ev);
        }
    });

    let registry = ToolRegistry::builtins();
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
    };

    // 21 messages x 200 chars = 4200 chars ≈ 1050 tokens, over the 700 soft
    // threshold of a 1000-token window before any provider usage exists.
    let filler = "x".repeat(200);
    let mut history = vec![Message::user(filler.clone())];
    for _ in 0..10 {
        history.push(Message::assistant(filler.clone()));
        history.push(Message::user(filler.clone()));
    }
    let seeded = history.clone();

    let agent = AgentLoop {
        layer_name: "compact-first".into(),
        provider: &provider,
        tools: &registry,
        cx,
        event_tx: tx,
        model_info,
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        reasoning_effort: None,
        thinking_budget: None,
        worker: None,
    };

    agent.drive("system".into(), history).await.unwrap();

    let counts = provider.message_counts.lock().unwrap().clone();
    assert!(
        counts[0] < seeded.len(),
        "the FIRST request must already carry the compacted history: {counts:?}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let freed = seen
        .lock()
        .unwrap()
        .iter()
        .find_map(|ev| match ev {
            stepper_protocol::AppEvent::CompactionDone { freed_tokens } => Some(*freed_tokens),
            _ => None,
        })
        .expect("a CompactionDone event was emitted");

    // freed_tokens is the honest chars/4 estimate of the dropped prefix
    // (keep_recent = 6 tail survives), not the old fabricated cut * 200.
    let cut = seeded.len() - 6;
    let expected = stepper_core::compaction::estimate_tokens(&seeded[..cut]);
    assert_eq!(
        freed, expected,
        "freed_tokens must be the estimate of the dropped messages"
    );
    assert_ne!(
        freed,
        cut as u64 * 200,
        "the fabricated message-count metric must be gone"
    );
}

#[tokio::test]
async fn summarize_with_model_returns_the_models_text() {
    let calls = Arc::new(Mutex::new(0));
    let provider = SummaryProvider {
        calls: calls.clone(),
    };
    let dropped = vec![
        Message::user("delete the old config"),
        Message::assistant("done, removed config.toml"),
    ];
    let summary = stepper_core::compaction::summarize_with_model(&provider, &dropped, None)
        .await
        .expect("summarizer returns text");
    assert!(summary.contains("MODEL SUMMARY"), "got: {summary}");
    assert_eq!(*calls.lock().unwrap(), 1);
}
