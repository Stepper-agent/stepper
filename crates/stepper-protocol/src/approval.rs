use crate::view::DiffView;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalKind {
    Command { cmd: String, outside_project: bool },
    FileEdit(DiffView),
    OutsideProject { path: PathBuf, action: String },
    Mcp { server: String, tool: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalDecision {
    AllowOnce,
    /// Persist a scoped allow rule into `.stepper/setting.json` `approvals`.
    AlwaysAllow { scope: String },
    Deny,
}

/// A request from core for the user to approve a side-effecting action.
///
/// The embedded `reply` oneshot IS the suspension mechanism: the agent loop in
/// core awaits `reply` while the TUI stays responsive and renders an overlay,
/// sending the decision back through this channel (§4.1).
///
/// Not `Clone`/`Serialize` on purpose — it carries a live channel end.
#[derive(Debug)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub kind: ApprovalKind,
    pub reply: oneshot::Sender<ApprovalDecision>,
}
