use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelView {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageView {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub context_used: u64,
    pub context_limit: u64,
    pub cost_usd: f64,
}

impl UsageView {
    /// Percentage of the model's context window still free (0..=100).
    /// Drives the Claude-Code-style footer gauge (§4.6.1).
    pub fn context_pct_left(&self) -> u8 {
        if self.context_limit == 0 {
            return 100;
        }
        let used = self.context_used.min(self.context_limit);
        let free = (self.context_limit - used) as u128;
        ((free * 100) / self.context_limit as u128) as u8
    }

    pub fn tokens_total(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallView {
    pub id: String,
    pub name: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffView {
    pub path: PathBuf,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItemView {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayerStatus {
    Pending,
    Running,
    Done,
    Failed,
}

/// Which layer of the orchestrator's `step` pipeline is active — rendered as the
/// left segment of the status line (§4.6.1 "활성 LAYER 표시").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerView {
    pub name: String,
    pub index: usize,
    pub total: usize,
    pub status: LayerStatus,
}

/// One concurrent worker of a fan-out (a structural parallel layer or a model's
/// `dispatch`). Rendered as a row in the live worker panel (Claude-Code-style
/// sub-agent view): index/total, label, model, live token count + last tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerView {
    pub index: usize,
    pub total: usize,
    pub label: String,
    pub provider: String,
    pub model: String,
    pub tokens: u64,
    pub last_tool: Option<String>,
    pub status: LayerStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// `/context` — an estimated decomposition of the primary layer's context
/// window into categories (tokens). `free` is the remainder of `context_limit`
/// after every category. Rendered by the TUI as a compact panel; the footer
/// gauge keeps using the live `UsageView` instead.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ContextBreakdownView {
    pub system_prompt: u64,
    pub tools: u64,
    pub mcp_tools: u64,
    pub skills: u64,
    pub memory: u64,
    pub messages: u64,
    pub free: u64,
    pub context_limit: u64,
}

/// One permission rule in the `/permissions` snapshot: its verdict list
/// (`allow`/`ask`/`deny`), the rule spec, and where it came from
/// (`scaffold`/`user`/`project`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleView {
    pub verdict: String,
    pub rule: String,
    pub source: String,
}

/// A persisted always-allow approval (`setting.json` `approvals[]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRuleView {
    pub rule: String,
    pub scope: Option<String>,
    pub granted_at: Option<String>,
}

/// `/permissions` — a read-only snapshot of the live permission posture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsSnapshotView {
    pub mode: String,
    pub rules: Vec<PermissionRuleView>,
    pub approvals: Vec<ApprovalRuleView>,
}

/// One `/rewind` candidate: a `turn-N` working-tree checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointView {
    pub id: String,
    pub turn: u64,
}

/// One `/resume` candidate: a persisted session, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub name: Option<String>,
    /// First line of the session's opening user request.
    pub digest: String,
    pub turns: usize,
    /// Human-readable age of the session file ("3m ago").
    pub age: String,
}
