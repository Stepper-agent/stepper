use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stepper_permission::{evaluate_in, Decision, PermissionMode, PermissionRequest, RuleSet};
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

/// A self-contained read-deny checker cloned from a [`ToolCx`], usable inside a
/// `spawn_blocking` walk (which can't borrow the cx). It lets enumerators
/// (grep/glob/list_dir) drop paths an explicit `deny Read(...)` rule covers, so a
/// subpath deny is honored even when the search root itself is allowed.
pub struct ReadGate {
    rules: Arc<RuleSet>,
    project_root: PathBuf,
    home: Option<PathBuf>,
    mode: PermissionMode,
}

impl ReadGate {
    /// Whether an explicit `deny` rule covers reading `path` (Ask/Allow do not
    /// filter — a denied path is silently skipped, never prompted mid-walk).
    pub fn denies(&self, path: &Path) -> bool {
        evaluate_in(
            &PermissionRequest::Read(path.to_path_buf()),
            &self.rules,
            &self.project_root,
            self.home.as_deref(),
            &self.project_root,
            self.mode,
        ) == Decision::Deny
    }
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

    /// A [`ReadGate`] snapshot for filtering deny-listed paths inside a blocking
    /// walk (grep/glob/list_dir run in `spawn_blocking`).
    pub fn read_gate(&self) -> ReadGate {
        ReadGate {
            rules: self.rules.clone(),
            project_root: self.project_root.clone(),
            home: self.home.clone(),
            mode: self.mode,
        }
    }

    /// Gate an action through `deny > ask > allow`: `Allow` proceeds, `Deny`
    /// errors, `Ask` consults the approver.
    pub async fn gate(
        &self,
        request: PermissionRequest,
        approval: Approval,
    ) -> Result<(), ToolError> {
        match evaluate_in(
            &request,
            &self.rules,
            &self.project_root,
            self.home.as_deref(),
            &self.cwd,
            self.mode,
        ) {
            Decision::Allow => Ok(()),
            Decision::Deny => Err(ToolError::Denied(format!(
                "{} denied by permission policy",
                request.tool()
            ))),
            // A parked approval must be interruptible: Esc / the per-turn
            // timeout fire `cancel`, and selecting on it drops the approver
            // future (unwinding its `rx.await`) so the tool can return instead
            // of hanging until the user also answers the overlay.
            Decision::Ask => {
                let decision = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return Err(ToolError::Denied(format!(
                        "{} interrupted before approval",
                        request.tool()
                    ))),
                    d = self.approver.request(approval) => d,
                };
                match decision {
                    Decision::Allow => Ok(()),
                    _ => Err(ToolError::Denied(format!(
                        "{} was not approved",
                        request.tool()
                    ))),
                }
            }
        }
    }

    pub fn is_in_project(&self, path: &Path) -> bool {
        stepper_permission::path::is_in_project(path, &self.project_root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// An approver that never answers — stands in for a user staring at an
    /// overlay while the turn is cancelled out from under them.
    struct NeverApprover;
    #[async_trait]
    impl Approver for NeverApprover {
        async fn request(&self, _approval: Approval) -> Decision {
            std::future::pending::<()>().await;
            Decision::Allow
        }
    }

    #[tokio::test]
    async fn gate_unwinds_when_cancelled_during_approval() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cx = ToolCx {
            cwd: PathBuf::from("/project"),
            project_root: PathBuf::from("/project"),
            home: None,
            mode: PermissionMode::Default,
            rules: Arc::new(RuleSet::from_lists(&[], &[], &[])),
            approver: Arc::new(NeverApprover),
            cancel,
        };
        // WebFetch in Default mode evaluates to Ask, so gate parks on the
        // approver; the already-fired cancel must resolve it promptly as Denied.
        let res = cx
            .gate(
                PermissionRequest::WebFetch("http://example.com".into()),
                Approval::Command {
                    command: "web_fetch http://example.com".into(),
                    outside_project: true,
                },
            )
            .await;
        match res {
            Err(ToolError::Denied(msg)) => assert!(msg.contains("interrupted"), "got {msg}"),
            other => panic!("expected interrupted denial, got {other:?}"),
        }
    }
}
