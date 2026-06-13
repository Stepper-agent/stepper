use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stepper_permission::{evaluate, Decision, PermissionMode, PermissionRequest, RuleSet};
use stepper_provider::ToolError;
use tokio_util::sync::CancellationToken;

/// A side-effecting action that needs the user's say-so. `stepper-core` maps
/// these onto the protocol's `ApprovalRequest` (the TUI overlay).
#[derive(Debug, Clone)]
pub enum Approval {
    Command { command: String, outside_project: bool },
    FileEdit { path: PathBuf, old: String, new: String },
    OutsideProject { path: PathBuf, action: String },
    Mcp { server: String, tool: String },
}

/// Asks the user to approve an action. Implemented by core (oneshot → TUI); in
/// tests, a canned implementation.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn request(&self, approval: Approval) -> Decision;
}

/// Everything a tool needs to run: where it runs, the permission policy, the
/// approver, and a cancellation handle.
#[derive(Clone)]
pub struct ToolCx {
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub home: Option<PathBuf>,
    pub mode: PermissionMode,
    pub rules: Arc<RuleSet>,
    pub approver: Arc<dyn Approver>,
    pub cancel: CancellationToken,
}

impl ToolCx {
    /// Resolve a (possibly relative) tool-supplied path against the cwd.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    /// Gate an action through `deny > ask > allow`: `Allow` proceeds, `Deny`
    /// errors, `Ask` consults the approver.
    pub async fn gate(
        &self,
        request: PermissionRequest,
        approval: Approval,
    ) -> Result<(), ToolError> {
        match evaluate(
            &request,
            &self.rules,
            &self.project_root,
            self.home.as_deref(),
            self.mode,
        ) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(ToolError::Denied(format!(
                "{} denied by permission policy",
                request.tool()
            ))),
            Decision::Ask => match self.approver.request(approval).await {
                Decision::Allow => Ok(()),
                _ => Err(ToolError::Denied(format!(
                    "{} was not approved",
                    request.tool()
                ))),
            },
        }
    }

    pub fn is_in_project(&self, path: &Path) -> bool {
        stepper_permission::path::is_in_project(path, &self.project_root)
    }
}
