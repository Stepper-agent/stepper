use crate::bridge::namespaced_name;
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as RmcpTool};
use rmcp::service::{Peer, RoleClient};
use serde_json::Value;
use std::time::Duration;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolContent, ToolError, ToolResult, ToolSpec};
use stepper_tools::{Approval, Tool, ToolCx};

/// Per-call wall-clock limit for an MCP tool. A hung server must not freeze a
/// turn. Override with `STEPPER_MCP_TOOL_TIMEOUT_MS`.
fn tool_timeout() -> Duration {
    let ms = std::env::var("STEPPER_MCP_TOOL_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120_000);
    Duration::from_millis(ms)
}

/// Total byte cap on a single MCP tool result's content — an adversarial or
/// chatty server must not exhaust the model context (the built-in tools cap at
/// 30000/100000 bytes too). Override with `STEPPER_MCP_MAX_OUTPUT_BYTES`.
fn max_output_bytes() -> usize {
    std::env::var("STEPPER_MCP_MAX_OUTPUT_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100_000)
}

/// A bridged MCP tool: presents an MCP server's tool through the native `Tool`
/// trait (namespaced `mcp__server__tool`), gated by the permission engine like
/// any other tool, and forwarding the call over the live MCP peer.
pub struct McpTool {
    server: String,
    real_name: String,
    spec: ToolSpec,
    peer: Peer<RoleClient>,
}

impl McpTool {
    pub fn new(server: &str, tool: RmcpTool, peer: Peer<RoleClient>) -> Self {
        let name = namespaced_name(server, &tool.name);
        McpTool::with_name(server, tool, peer, name)
    }

    /// Bridge with an explicit namespaced name — the manager uses this to keep
    /// registered names collision-free across servers (see `claim_namespaced_name`).
    pub fn with_name(server: &str, tool: RmcpTool, peer: Peer<RoleClient>, name: String) -> Self {
        let real_name = tool.name.to_string();
        let spec = ToolSpec {
            name,
            description: tool
                .description
                .map(|d| d.to_string())
                .unwrap_or_default(),
            input_schema: Value::Object((*tool.input_schema).clone()),
            read_only: false,
            parallel_safe: false,
        };
        McpTool {
            server: server.to_string(),
            real_name,
            spec,
            peer,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        cx.gate(
            PermissionRequest::Mcp {
                server: self.server.clone(),
                tool: self.real_name.clone(),
            },
            Approval::Mcp {
                server: self.server.clone(),
                tool: self.real_name.clone(),
            },
        )
        .await?;

        let mut param = CallToolRequestParams::new(self.real_name.clone());
        if let Some(arguments) = args.as_object().cloned() {
            param = param.with_arguments(arguments);
        }
        // Honor interrupt (Esc) and a wall-clock cap so one hung MCP server can't
        // freeze the turn (the built-in tools are cancel-aware too).
        let result = tokio::select! {
            _ = cx.cancel.cancelled() => {
                return Err(ToolError::Execution(format!("mcp call '{}' cancelled", self.real_name)));
            }
            r = tokio::time::timeout(tool_timeout(), self.peer.call_tool(param)) => match r {
                Err(_) => return Err(ToolError::Execution(format!(
                    "mcp call '{}' timed out", self.real_name
                ))),
                Ok(Ok(res)) => res,
                Ok(Err(e)) => return Err(ToolError::Execution(format!(
                    "mcp call '{}' failed: {e}", self.real_name
                ))),
            }
        };
        Ok(fold(result))
    }
}

fn fold(result: CallToolResult) -> ToolResult {
    fold_with_cap(result, max_output_bytes())
}

/// Fold an MCP result into a native `ToolResult`: a `structuredContent`-only
/// result (2025-06-18 spec) is surfaced as a Json block instead of being
/// dropped, an error with no content gets an explicit error text, and the total
/// content is byte-capped (char-boundary safe) with `truncated: true`.
fn fold_with_cap(result: CallToolResult, cap: usize) -> ToolResult {
    let is_error = result.is_error.unwrap_or(false);
    let mut content: Vec<ToolContent> = result
        .content
        .into_iter()
        .map(|c| match c.as_text() {
            Some(text) => ToolContent::text(text.text.clone()),
            None => ToolContent::Json {
                json: serde_json::to_value(&c).unwrap_or(Value::Null),
            },
        })
        .collect();
    if content.is_empty() {
        if let Some(json) = result.structured_content {
            content.push(ToolContent::Json { json });
        } else if is_error {
            content.push(ToolContent::text(
                "mcp tool reported an error without any content",
            ));
        }
    }
    let (mut content, truncated) = cap_total_bytes(content, cap);
    if truncated {
        content.push(ToolContent::text("… [output truncated]"));
    }
    ToolResult {
        content,
        is_error,
        truncated,
    }
}

fn cap_total_bytes(blocks: Vec<ToolContent>, cap: usize) -> (Vec<ToolContent>, bool) {
    let mut used = 0usize;
    let mut capped = Vec::with_capacity(blocks.len());
    let mut truncated = false;
    for block in blocks {
        if used >= cap {
            truncated = true;
            break;
        }
        let remaining = cap - used;
        let mut text = match block {
            ToolContent::Text { text } => text,
            ToolContent::Json { json } => {
                let serialized = json.to_string();
                if serialized.len() <= remaining {
                    used += serialized.len();
                    capped.push(ToolContent::Json { json });
                    continue;
                }
                serialized
            }
        };
        if text.len() > remaining {
            truncated = true;
            truncate_on_char_boundary(&mut text, remaining);
        }
        used += text.len();
        capped.push(ToolContent::text(text));
    }
    (capped, truncated)
}

/// Truncate `s` to at most `max` bytes, backing up to the nearest UTF-8 char
/// boundary so a multibyte codepoint straddling the cap never panics
/// `String::truncate` (a server-triggerable crash on large non-ASCII output).
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    #[test]
    fn structured_content_only_result_surfaces_as_json_block() {
        let mut result = CallToolResult::structured(serde_json::json!({ "echoed": "payload" }));
        result.content = vec![];
        let folded = fold_with_cap(result, 100_000);
        assert!(!folded.is_error);
        assert!(!folded.truncated);
        assert_eq!(folded.content.len(), 1);
        match &folded.content[0] {
            ToolContent::Json { json } => assert_eq!(json["echoed"], "payload"),
            other => panic!("structuredContent must fold into a Json block, got {other:?}"),
        }
    }

    #[test]
    fn structured_content_is_secondary_to_non_empty_content() {
        let mut result = CallToolResult::structured(serde_json::json!({ "ignored": true }));
        result.content = vec![Content::text("primary")];
        let folded = fold_with_cap(result, 100_000);
        assert_eq!(folded.content.len(), 1);
        assert_eq!(folded.content_text(), "primary");
    }

    #[test]
    fn error_with_empty_content_gets_a_non_empty_error_text() {
        let result = CallToolResult::error(vec![]);
        let folded = fold_with_cap(result, 100_000);
        assert!(folded.is_error);
        assert!(
            !folded.content_text().trim().is_empty(),
            "an empty MCP error must not fold into an empty error"
        );
    }

    #[test]
    fn empty_success_without_structured_content_stays_empty() {
        let result = CallToolResult::success(vec![]);
        let folded = fold_with_cap(result, 100_000);
        assert!(!folded.is_error);
        assert!(!folded.truncated);
        assert!(folded.content.is_empty());
    }

    #[test]
    fn oversized_text_is_capped_with_truncated_flag() {
        let result = CallToolResult::success(vec![Content::text("x".repeat(500))]);
        let folded = fold_with_cap(result, 100);
        assert!(folded.truncated);
        match &folded.content[0] {
            ToolContent::Text { text } => assert_eq!(text.len(), 100),
            other => panic!("expected capped text, got {other:?}"),
        }
        assert!(folded.content_text().contains("[output truncated]"));
    }

    #[test]
    fn cap_backs_up_to_a_char_boundary_on_multibyte_output() {
        let result = CallToolResult::success(vec![Content::text("가".repeat(100))]);
        let folded = fold_with_cap(result, 100);
        assert!(folded.truncated);
        match &folded.content[0] {
            ToolContent::Text { text } => {
                assert_eq!(text.len(), 99, "100 lands mid-codepoint, must back up to 99");
                assert!(text.chars().all(|c| c == '가'));
            }
            other => panic!("expected capped text, got {other:?}"),
        }
    }

    #[test]
    fn cap_spans_blocks_and_drops_the_overflow() {
        let result = CallToolResult::success(vec![
            Content::text("a".repeat(60)),
            Content::text("b".repeat(60)),
            Content::text("c".repeat(60)),
        ]);
        let folded = fold_with_cap(result, 100);
        assert!(folded.truncated);
        assert_eq!(folded.content.len(), 3, "two capped blocks plus the marker");
        match (&folded.content[0], &folded.content[1]) {
            (ToolContent::Text { text: a }, ToolContent::Text { text: b }) => {
                assert_eq!(a.len(), 60);
                assert_eq!(b.len(), 40);
            }
            other => panic!("expected two text blocks, got {other:?}"),
        }
    }

    #[test]
    fn json_block_within_cap_keeps_its_type_and_overflowing_json_becomes_text() {
        let small = CallToolResult::success(vec![Content::image("aGVsbG8=", "image/png")]);
        let folded = fold_with_cap(small, 100_000);
        assert!(!folded.truncated);
        assert!(matches!(&folded.content[0], ToolContent::Json { .. }));

        let big = CallToolResult::success(vec![Content::image("A".repeat(500), "image/png")]);
        let folded = fold_with_cap(big, 100);
        assert!(folded.truncated);
        match &folded.content[0] {
            ToolContent::Text { text } => assert_eq!(text.len(), 100),
            other => panic!("an overflowing json block must cap as text, got {other:?}"),
        }
    }

    #[test]
    fn oversized_structured_content_is_also_capped() {
        let mut result =
            CallToolResult::structured(serde_json::json!({ "blob": "y".repeat(500) }));
        result.content = vec![];
        let folded = fold_with_cap(result, 100);
        assert!(folded.truncated);
        match &folded.content[0] {
            ToolContent::Text { text } => assert_eq!(text.len(), 100),
            other => panic!("expected capped text, got {other:?}"),
        }
    }

    #[test]
    fn max_output_bytes_defaults_and_honors_the_env_override() {
        assert_eq!(max_output_bytes(), 100_000);
        // SAFETY: single test body setting a process env var it then removes;
        // no other test in this binary reads STEPPER_MCP_MAX_OUTPUT_BYTES.
        unsafe {
            std::env::set_var("STEPPER_MCP_MAX_OUTPUT_BYTES", "1234");
        }
        assert_eq!(max_output_bytes(), 1234);
        unsafe {
            std::env::set_var("STEPPER_MCP_MAX_OUTPUT_BYTES", "not-a-number");
        }
        assert_eq!(max_output_bytes(), 100_000);
        unsafe {
            std::env::remove_var("STEPPER_MCP_MAX_OUTPUT_BYTES");
        }
    }
}
