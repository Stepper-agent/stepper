//! `stepper-tools` — the unified `Tool` trait plus the built-in tools, all gated
//! through the permission engine + an `Approver`. Native and (later) MCP tools
//! implement the same trait so the model can't tell them apart.
//!
//! The real security boundary is the canonicalized path check in
//! `stepper-permission` (a symlink inside the project that escapes is judged by
//! its real location). The [`sandbox`] module adds an opt-in OS-level
//! depth-defense layer (macOS Seatbelt) that confines the `bash` tool's writes.

pub mod context;
pub mod registry;
pub mod sandbox;
pub mod secret;
pub mod tools;

pub use context::{Approval, Approver, ToolCx};
pub use registry::ToolRegistry;

pub use stepper_provider::{ToolContent, ToolError, ToolResult, ToolSpec};

use async_trait::async_trait;
use serde_json::Value;

/// Truncate `s` to at most `max` bytes, backing up to the nearest UTF-8 char
/// boundary so a multibyte codepoint straddling the cap is never split (which
/// `String::truncate` panics on — a model-triggerable crash on large non-ASCII
/// tool output).
pub(crate) fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// A callable tool. Built-ins and bridged MCP tools share this trait.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError>;

    fn name(&self) -> &str {
        &self.spec().name
    }

    fn read_only(&self) -> bool {
        self.spec().read_only
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_on_char_boundary;

    #[test]
    fn truncate_backs_up_to_a_char_boundary_and_never_panics() {
        // "가" is 3 bytes; a cap landing mid-codepoint would panic String::truncate.
        let mut s = "가".repeat(100); // 300 bytes
        truncate_on_char_boundary(&mut s, 100); // 100 is not a char boundary (100 % 3 != 0)
        assert!(s.len() <= 100);
        assert!(s.is_char_boundary(s.len()));
        assert!(s.chars().all(|c| c == '가'));
    }

    #[test]
    fn truncate_is_a_noop_when_within_cap() {
        let mut s = "hello".to_string();
        truncate_on_char_boundary(&mut s, 100);
        assert_eq!(s, "hello");
    }
}
