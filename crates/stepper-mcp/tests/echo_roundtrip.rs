//! Hermetic, in-process MCP roundtrip: a minimal `rmcp` server exposing `echo`,
//! `echo_error`, and `echo_json` tools is connected to a client over an
//! in-memory `tokio::io::duplex` transport (NO network, NO child process). The
//! client peer + listed tools are fed through the real `McpTool` bridge — the
//! same construction `McpManager::connect` performs — so a `Tool::call` exercises
//! namespacing, the permission gate, the live MCP `call_tool`, and `fold`.

use async_trait::async_trait;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stepper_mcp::McpTool;
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_provider::{ToolContent, ToolError};
use stepper_tools::{Approval, Approver, Tool, ToolCx};
use tokio_util::sync::CancellationToken;

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoRequest {
    text: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct RepeatRequest {
    text: String,
    count: usize,
}

#[derive(Clone)]
struct EchoServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    calls: Arc<AtomicUsize>,
}

impl EchoServer {
    fn new(calls: Arc<AtomicUsize>) -> Self {
        EchoServer {
            tool_router: Self::tool_router(),
            calls,
        }
    }
}

#[tool_router]
impl EchoServer {
    #[tool(description = "Echo the input text back verbatim.")]
    async fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        text
    }

    #[tool(description = "Echo the input as a non-text image content block.")]
    async fn echo_image(
        &self,
        Parameters(EchoRequest { text }): Parameters<EchoRequest>,
    ) -> CallToolResult {
        CallToolResult::success(vec![Content::image(text, "image/png")])
    }

    #[tool(description = "Echo the input as an MCP error result.")]
    async fn echo_error(
        &self,
        Parameters(EchoRequest { text }): Parameters<EchoRequest>,
    ) -> CallToolResult {
        CallToolResult::error(vec![Content::text(text)])
    }

    #[tool(description = "Echo the input wrapped in a structured JSON content block.")]
    async fn echo_json(
        &self,
        Parameters(EchoRequest { text }): Parameters<EchoRequest>,
    ) -> CallToolResult {
        let block = Content::json(serde_json::json!({ "echoed": text })).expect("json content");
        CallToolResult::success(vec![block])
    }

    #[tool(description = "Return a fixed string; takes no arguments.")]
    async fn echo_ping(&self) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        "pong".to_string()
    }

    #[tool(description = "Return the payload in structuredContent only (empty content array).")]
    async fn echo_structured(
        &self,
        Parameters(EchoRequest { text }): Parameters<EchoRequest>,
    ) -> CallToolResult {
        let mut result = CallToolResult::structured(serde_json::json!({ "echoed": text }));
        result.content = vec![];
        result
    }

    #[tool(description = "Return an error result with no content blocks at all.")]
    async fn echo_error_empty(&self) -> CallToolResult {
        CallToolResult::error(vec![])
    }

    #[tool(description = "Echo the text repeated `count` times.")]
    async fn echo_repeat(
        &self,
        Parameters(RepeatRequest { text, count }): Parameters<RepeatRequest>,
    ) -> String {
        text.repeat(count)
    }
}

#[tool_handler]
impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("In-process echo server")
    }
}

struct Canned(Decision);

#[async_trait]
impl Approver for Canned {
    async fn request(&self, _approval: Approval) -> Decision {
        self.0
    }
}

fn cx(approver: Arc<dyn Approver>) -> ToolCx {
    let cwd = std::env::temp_dir();
    ToolCx {
        cwd: cwd.clone(),
        project_root: cwd,
        home: None,
        mode: PermissionMode::AcceptEdits,
        live_mode: None,
        rules: Arc::new(RuleSet::default()),
        approver,
        cancel: CancellationToken::new(),
        sandbox_writable_roots: None,
    }
}

async fn connect_echo() -> (RunningService<RoleClient, ()>, Peer<RoleClient>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let (server_transport, client_transport) = tokio::io::duplex(8192);
    let server_calls = calls.clone();
    tokio::spawn(async move {
        let running = EchoServer::new(server_calls)
            .serve(server_transport)
            .await
            .expect("echo server serves");
        let _ = running.waiting().await;
    });
    let client = ()
        .serve(client_transport)
        .await
        .expect("client connects to echo server");
    let peer = client.peer().clone();
    (client, peer, calls)
}

async fn bridge_tools(
    client: &RunningService<RoleClient, ()>,
    peer: &Peer<RoleClient>,
) -> Vec<Arc<dyn Tool>> {
    let mcp_tools = client.list_all_tools().await.expect("list tools");
    mcp_tools
        .into_iter()
        .map(|t| Arc::new(McpTool::new("echo", t, peer.clone())) as Arc<dyn Tool>)
        .collect()
}

fn find<'a>(tools: &'a [Arc<dyn Tool>], suffix: &str) -> &'a Arc<dyn Tool> {
    tools
        .iter()
        .find(|t| t.name() == format!("mcp__echo__{suffix}"))
        .unwrap_or_else(|| panic!("tool mcp__echo__{suffix} present, got {:?}", names(tools)))
}

fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools.iter().map(|t| t.name().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_namespaced_echo_tools_over_duplex() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let mut listed = names(&tools);
    listed.sort();
    assert_eq!(
        listed,
        vec![
            "mcp__echo__echo".to_string(),
            "mcp__echo__echo_error".to_string(),
            "mcp__echo__echo_error_empty".to_string(),
            "mcp__echo__echo_image".to_string(),
            "mcp__echo__echo_json".to_string(),
            "mcp__echo__echo_ping".to_string(),
            "mcp__echo__echo_repeat".to_string(),
            "mcp__echo__echo_structured".to_string(),
        ]
    );
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_call_roundtrips_text_through_bridge() {
    let (client, peer, calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo = find(&tools, "echo");

    let result = echo
        .call(
            serde_json::json!({ "text": "round-trip-payload" }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo call succeeds");

    assert!(!result.is_error);
    assert!(!result.truncated);
    assert_eq!(result.content_text(), "round-trip-payload");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "an allowed call reaches the server exactly once");
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_error_result_sets_is_error_flag() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_error = find(&tools, "echo_error");

    let result = echo_error
        .call(
            serde_json::json!({ "text": "boom" }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo_error call returns a result, not a transport error");

    assert!(result.is_error, "MCP is_error must fold into ToolResult");
    assert_eq!(result.content_text(), "boom");
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn echo_json_content_folds_into_text() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_json = find(&tools, "echo_json");

    let result = echo_json
        .call(
            serde_json::json!({ "text": "structured" }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo_json call succeeds");

    assert!(!result.is_error);
    let text = result.content_text();
    assert!(
        text.contains("structured") && text.contains("echoed"),
        "expected folded json content, got: {text}"
    );
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn non_text_content_folds_into_the_json_branch() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_image = find(&tools, "echo_image");

    let result = echo_image
        .call(
            serde_json::json!({ "text": "aGVsbG8=" }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo_image call succeeds");

    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    let json = match &result.content[0] {
        ToolContent::Json { json } => json,
        other => panic!("image content must fold into ToolContent::Json, got {other:?}"),
    };
    assert_eq!(json["type"], "image");
    assert_eq!(json["data"], "aGVsbG8=");
    assert_eq!(json["mimeType"], "image/png");
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_gate_denial_blocks_the_mcp_call() {
    let (client, peer, calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo = find(&tools, "echo");

    let err = echo
        .call(
            serde_json::json!({ "text": "should-never-run" }),
            &cx(Arc::new(Canned(Decision::Deny))),
        )
        .await
        .expect_err("denied approval must surface as an error");

    assert!(matches!(err, ToolError::Denied(_)), "got {err:?}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a denied call must never reach the MCP server"
    );
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn bridged_spec_carries_description_and_input_schema() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo = find(&tools, "echo");

    let spec = echo.spec();
    assert_eq!(spec.name, "mcp__echo__echo");
    assert_eq!(spec.description, "Echo the input text back verbatim.");
    assert!(!spec.read_only);
    assert!(!spec.parallel_safe);
    let schema = spec
        .input_schema
        .get("properties")
        .and_then(|p| p.get("text"))
        .expect("input schema exposes the `text` property");
    assert!(schema.is_object());
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_error_maps_to_execution_error() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo = find(&tools, "echo");

    client.cancel().await.expect("cancel server connection");

    let err = echo
        .call(
            serde_json::json!({ "text": "after-cancel" }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect_err("calling a tool after the peer is gone must error");

    match err {
        ToolError::Execution(message) => {
            assert!(
                message.starts_with("mcp call 'echo' failed: "),
                "transport failure must map to Execution with the failed-call prefix, got: {message}"
            );
        }
        other => panic!("transport failure must map to ToolError::Execution, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn null_args_skips_with_arguments_and_still_calls() {
    let (client, peer, calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_ping = find(&tools, "echo_ping");

    let result = echo_ping
        .call(serde_json::Value::Null, &cx(Arc::new(Canned(Decision::Allow))))
        .await
        .expect("a non-object args body must skip with_arguments, not panic");

    assert!(!result.is_error);
    assert_eq!(result.content_text(), "pong");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the no-arguments branch must still reach the server"
    );
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn structured_content_only_result_reaches_the_model() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_structured = find(&tools, "echo_structured");

    let result = echo_structured
        .call(
            serde_json::json!({ "text": "structured-only-payload" }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo_structured call succeeds");

    assert!(!result.is_error);
    assert!(!result.truncated);
    assert_eq!(
        result.content.len(),
        1,
        "an empty-content result with structuredContent must not fold to empty"
    );
    match &result.content[0] {
        ToolContent::Json { json } => assert_eq!(json["echoed"], "structured-only-payload"),
        other => panic!("structuredContent must surface as a Json block, got {other:?}"),
    }
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_error_result_folds_to_a_non_empty_error() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_error_empty = find(&tools, "echo_error_empty");

    let result = echo_error_empty
        .call(serde_json::Value::Null, &cx(Arc::new(Canned(Decision::Allow))))
        .await
        .expect("echo_error_empty call returns a result, not a transport error");

    assert!(result.is_error);
    assert!(
        !result.content_text().trim().is_empty(),
        "an MCP error with no content must still tell the model something went wrong"
    );
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_output_is_truncated_with_flag_on_a_char_boundary() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_repeat = find(&tools, "echo_repeat");

    // 40_000 × 3-byte '한' = 120_000 bytes > the 100_000 default cap, which lands
    // mid-codepoint (100_000 % 3 != 0) and must back up to a char boundary.
    let result = echo_repeat
        .call(
            serde_json::json!({ "text": "한", "count": 40_000 }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo_repeat call succeeds");

    assert!(result.truncated, "an oversized result must set truncated");
    match &result.content[0] {
        ToolContent::Text { text } => {
            assert_eq!(text.len(), 99_999, "cap backs up from 100_000 to the boundary");
            assert!(text.chars().all(|c| c == '한'));
        }
        other => panic!("expected capped text content, got {other:?}"),
    }
    assert!(result.content_text().contains("[output truncated]"));
    client.cancel().await.expect("cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn small_output_is_not_truncated() {
    let (client, peer, _calls) = connect_echo().await;
    let tools = bridge_tools(&client, &peer).await;
    let echo_repeat = find(&tools, "echo_repeat");

    let result = echo_repeat
        .call(
            serde_json::json!({ "text": "ab", "count": 3 }),
            &cx(Arc::new(Canned(Decision::Allow))),
        )
        .await
        .expect("echo_repeat call succeeds");

    assert!(!result.truncated);
    assert_eq!(result.content_text(), "ababab");
    client.cancel().await.expect("cancel");
}
