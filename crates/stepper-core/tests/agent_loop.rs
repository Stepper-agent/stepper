//! The ReAct loop end-to-end with a scripted FakeProvider: the model asks for a
//! tool, the loop runs it (really writing a file via the built-in tool through
//! the permission gate), threads the result back, and the second turn finishes.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use stepper_core::{AgentLoop, HookHost, ModelRegistry};
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

#[tokio::test]
async fn react_loop_runs_a_tool_then_finishes() {
    let dir = tempfile::tempdir().unwrap();

    let provider = FakeProvider {
        calls: Mutex::new(0),
        scripts: vec![
            // turn 1: ask to write a file
            vec![
                ChatEvent::ToolCallCompleted {
                    index: 0,
                    id: "call_1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({"path": "out.txt", "content": "hello tools"}),
                },
                ChatEvent::Done(StopReason::ToolUse),
            ],
            // turn 2: finish
            vec![
                ChatEvent::TextDelta("wrote the file".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ],
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
    let cx = ToolCx {
        cwd: dir.path().to_path_buf(),
        project_root: dir.path().to_path_buf(),
        home: None,
        mode: PermissionMode::AcceptEdits,
        rules: Arc::new(RuleSet::default()),
        approver: Arc::new(AllowAll),
        cancel: CancellationToken::new(),
    };
    let agent = AgentLoop {
        layer_name: "test".into(),
        provider: &provider,
        tools: &registry,
        cx,
        event_tx: tx,
        model_info: ModelRegistry::builtin().lookup("fake", "fake-model"),
        step_cap: 5,
        hooks: Arc::new(HookHost::empty(dir.path().to_path_buf())),
        compaction_provider: None,
        temperature: None,
        top_p: None,
        worker: None,
    };

    let outcome = agent
        .drive("you are a test agent".into(), vec![Message::user("write out.txt")])
        .await
        .unwrap();

    assert_eq!(outcome.summary, "wrote the file");
    let written = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(written, "hello tools");

    // The tool call surfaced to the TUI.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let events = seen.lock().unwrap().join("\n");
    assert!(events.contains("ToolCallStarted"));
    assert!(events.contains("ToolCallFinished"));
}
