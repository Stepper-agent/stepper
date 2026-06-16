//! Live MCP integration test against an EXTERNALLY CONFIGURED server — no server
//! is hardcoded. The server (stdio command/args or http url), its name, the
//! optional API key, and an optional tool invocation are all pulled from env.
//! Skips (no-op) unless a server is configured. The API key is OPTIONAL.
//!
//! Example — context7 over stdio, key fed from the repo `.env`:
//!   STEPPER_MCP_TEST_CMD=npx \
//!   STEPPER_MCP_TEST_ARGS="-y @upstash/context7-mcp" \
//!   STEPPER_MCP_TEST_NAME=context7 \
//!   STEPPER_MCP_TEST_API_KEY="$(grep CONTEXT7_KEY .env | cut -d= -f2-)" \
//!   STEPPER_MCP_TEST_TOOL=resolve-library-id \
//!   STEPPER_MCP_TEST_TOOL_ARGS='{"query":"react hooks","libraryName":"react"}' \
//!   STEPPER_MCP_TEST_EXPECT=react \
//!   cargo test -p stepper-mcp --test mcp_live -- --nocapture
//!
//! Example — a keyless / http server: set STEPPER_MCP_TEST_URL=... and omit the
//! API key entirely.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;
use stepper_config::McpServerConfig;
use stepper_mcp::McpManager;
use stepper_permission::{Decision, PermissionMode, RuleSet};
use stepper_tools::{Approval, Approver, ToolCx};
use tokio_util::sync::CancellationToken;

struct AllowAll;
#[async_trait]
impl Approver for AllowAll {
    async fn request(&self, _approval: Approval) -> Decision {
        Decision::Allow
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Build the server config purely from env. Returns None (skip) when neither a
/// stdio command nor an http url is set. The API key is optional everywhere.
fn server_from_env() -> Option<(String, McpServerConfig)> {
    let name = env("STEPPER_MCP_TEST_NAME").unwrap_or_else(|| "testmcp".into());
    let api_key = env("STEPPER_MCP_TEST_API_KEY");

    if let Some(url) = env("STEPPER_MCP_TEST_URL") {
        let mut headers = BTreeMap::new();
        if let Some(key) = &api_key {
            headers.insert("Authorization".into(), format!("Bearer {key}"));
        }
        return Some((
            name,
            McpServerConfig {
                transport: Some("http".into()),
                command: None,
                args: Vec::new(),
                url: Some(url),
                headers,
                env: BTreeMap::new(),
                always_load: false,
            },
        ));
    }

    let command = env("STEPPER_MCP_TEST_CMD")?;
    let mut args: Vec<String> = env("STEPPER_MCP_TEST_ARGS")
        .map(|a| a.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    if let Some(key) = api_key {
        let flag = env("STEPPER_MCP_TEST_API_KEY_FLAG").unwrap_or_else(|| "--api-key".into());
        args.push(flag);
        args.push(key);
    }
    Some((
        name,
        McpServerConfig {
            transport: Some("stdio".into()),
            command: Some(command),
            args,
            url: None,
            headers: BTreeMap::new(),
            env: BTreeMap::new(),
            always_load: false,
        },
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn external_mcp_server_lists_and_optionally_calls_a_tool() {
    let Some((name, cfg)) = server_from_env() else {
        eprintln!(
            "SKIP: configure a server via STEPPER_MCP_TEST_CMD (stdio) or STEPPER_MCP_TEST_URL (http)"
        );
        return;
    };

    let mut servers = BTreeMap::new();
    servers.insert(name.clone(), cfg);

    let manager = McpManager::connect(&servers).await;
    let names = manager.tool_names();
    eprintln!("{name} tools: {names:?}");
    assert!(!manager.is_empty(), "expected the MCP server to expose tools");
    assert!(
        names.iter().all(|n| n.starts_with("mcp__")),
        "every tool must be namespaced mcp__<server>__<tool>: {names:?}"
    );

    if let Some(tool_query) = env("STEPPER_MCP_TEST_TOOL") {
        let tool_args: serde_json::Value = env("STEPPER_MCP_TEST_TOOL_ARGS")
            .map(|s| serde_json::from_str(&s).expect("STEPPER_MCP_TEST_TOOL_ARGS must be JSON"))
            .unwrap_or_else(|| serde_json::json!({}));

        let tool = manager
            .tools()
            .into_iter()
            .find(|t| t.name().contains(&tool_query))
            .unwrap_or_else(|| panic!("no tool matching '{tool_query}' in {names:?}"));

        let cx = ToolCx {
            cwd: std::env::current_dir().unwrap(),
            project_root: std::env::current_dir().unwrap(),
            home: None,
            mode: PermissionMode::AcceptEdits,
            live_mode: None,
            rules: Arc::new(RuleSet::default()),
            approver: Arc::new(AllowAll),
            cancel: CancellationToken::new(),
            sandbox_writable_roots: None,
        };

        let result = tool
            .call(tool_args, &cx)
            .await
            .expect("tool call returned a result");
        let text = result.content_text();
        eprintln!("{} →\n{}", tool.name(), &text[..text.len().min(300)]);
        assert!(!result.is_error, "tool returned an error: {text}");
        if let Some(expect) = env("STEPPER_MCP_TEST_EXPECT") {
            assert!(
                text.to_lowercase().contains(&expect.to_lowercase()),
                "expected '{expect}' in tool result, got: {text}"
            );
        }
    }

    manager.shutdown().await;
}
