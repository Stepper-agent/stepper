//! Within-turn tool execution: `parallel_safe` calls run concurrently while
//! mutating calls stay sequential, results keep request order (tool_call_id
//! pairing), and an oversized tool result is shrunk to a head+tail excerpt.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stepper_core::{AgentLoop, HookHost, ModelRegistry};
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatStream, ContentBlock, LlmProvider, Message, ProviderError, Role,
    StopReason, ToolError, ToolResult, ToolSpec,
};
use stepper_tools::{Approval, Approver, Tool, ToolCx, ToolRegistry};
use tokio::sync::{mpsc, Barrier};
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    calls: Mutex<usize>,
    scripts: Vec<Vec<ChatEvent>>,
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
        let script = self.scripts.get(index).cloned().unwrap_or_else(|| {
            vec![
                ChatEvent::TextDelta("done".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        });
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

fn spec(name: &str, parallel_safe: bool) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: String::new(),
        input_schema: serde_json::json!({ "type": "object" }),
        read_only: parallel_safe,
        parallel_safe,
    }
}

/// A parallel_safe tool that blocks on a shared barrier — two concurrent calls
/// release each other; run serially, the first would wait forever.
struct Barriered {
    spec: ToolSpec,
    barrier: Arc<Barrier>,
}
#[async_trait]
impl Tool for Barriered {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        self.barrier.wait().await;
        Ok(ToolResult::text("read-ok"))
    }
}

struct Plain {
    spec: ToolSpec,
}
#[async_trait]
impl Tool for Plain {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text("write-ok"))
    }
}

struct Big {
    spec: ToolSpec,
    body: String,
}
#[async_trait]
impl Tool for Big {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _args: serde_json::Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::text(self.body.clone()))
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

fn agent<'a>(
    provider: &'a FakeProvider,
    registry: &'a ToolRegistry,
    cx: ToolCx,
    tx: mpsc::Sender<stepper_protocol::AppEvent>,
    dir: &std::path::Path,
) -> AgentLoop<'a> {
    AgentLoop {
        formatters: Default::default(),
        lsp: Default::default(),
        budget: None,
        layer_name: "test".into(),
        provider,
        tools: registry,
        cx,
        event_tx: tx,
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        reasoning_effort: None,
        thinking_budget: None,
        worker: None,
    }
}

fn tool_result_ids(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_safe_tools_run_concurrently_and_keep_request_order() {
    let dir = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(2));

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Barriered {
        spec: spec("pread", true),
        barrier: barrier.clone(),
    }));
    registry.register(Arc::new(Plain {
        spec: spec("swrite", false),
    }));

    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![vec![
            ChatEvent::ToolCallCompleted {
                index: 0,
                id: "c1".into(),
                name: "pread".into(),
                input: serde_json::json!({}),
            },
            ChatEvent::ToolCallCompleted {
                index: 1,
                id: "c2".into(),
                name: "pread".into(),
                input: serde_json::json!({}),
            },
            ChatEvent::ToolCallCompleted {
                index: 2,
                id: "c3".into(),
                name: "swrite".into(),
                input: serde_json::json!({}),
            },
            ChatEvent::Done(StopReason::ToolUse),
        ]],
    };

    let (tx, _rx) = mpsc::channel(256);
    let agent = agent(&provider, &registry, cx(dir.path()), tx, dir.path());

    // The two pread calls only complete if run concurrently (shared Barrier(2));
    // a serial loop would deadlock, so the timeout is the concurrency assertion.
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        agent.drive("t".into(), vec![Message::user("go")]),
    )
    .await
    .expect("did not deadlock — tools ran concurrently")
    .unwrap();

    // Results are threaded back in the original request order regardless of which
    // bucket (parallel/sequential) each fell into.
    assert_eq!(tool_result_ids(&outcome.messages), vec!["c1", "c2", "c3"]);
}

#[tokio::test]
async fn sequential_tools_keep_request_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Plain { spec: spec("w", false) }));

    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![vec![
            ChatEvent::ToolCallCompleted { index: 0, id: "a".into(), name: "w".into(), input: serde_json::json!({}) },
            ChatEvent::ToolCallCompleted { index: 1, id: "b".into(), name: "w".into(), input: serde_json::json!({}) },
            ChatEvent::Done(StopReason::ToolUse),
        ]],
    };
    let (tx, _rx) = mpsc::channel(256);
    let agent = agent(&provider, &registry, cx(dir.path()), tx, dir.path());
    let outcome = agent.drive("t".into(), vec![Message::user("go")]).await.unwrap();
    assert_eq!(tool_result_ids(&outcome.messages), vec!["a", "b"]);
}

#[tokio::test]
async fn oversized_tool_result_is_shrunk_with_an_elision_marker() {
    let dir = tempfile::tempdir().unwrap();
    let body = "X".repeat(80_000); // well over the per-call cap
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Big {
        spec: spec("bigread", true),
        body: body.clone(),
    }));

    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![vec![
            ChatEvent::ToolCallCompleted { index: 0, id: "c1".into(), name: "bigread".into(), input: serde_json::json!({}) },
            ChatEvent::Done(StopReason::ToolUse),
        ]],
    };
    let (tx, _rx) = mpsc::channel(256);
    let agent = agent(&provider, &registry, cx(dir.path()), tx, dir.path());
    let outcome = agent.drive("t".into(), vec![Message::user("go")]).await.unwrap();

    let tool_msg = outcome
        .messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .unwrap();
    let text = match &tool_msg.content[0] {
        ContentBlock::ToolResult { content, .. } => content
            .iter()
            .map(|c| match c {
                stepper_provider::ToolContent::Text { text } => text.clone(),
                stepper_provider::ToolContent::Json { json } => json.to_string(),
            })
            .collect::<String>(),
        other => panic!("expected tool result, got {other:?}"),
    };
    assert!(text.len() < body.len(), "oversized result shrunk: {} < {}", text.len(), body.len());
    assert!(text.contains("chars elided"), "carries the elision marker: {}", &text[..text.len().min(200)]);
}
