use async_trait::async_trait;
use stepper_permission::Decision;
use stepper_protocol::{
    AppEvent, ApprovalDecision, ApprovalKind, ApprovalRequest, DiffView, EventTx,
};
use stepper_tools::{Approval, Approver};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Bridges a tool's `Approval` to the TUI: emits `ApprovalRequested` carrying a
/// oneshot and awaits the user's decision (the Phase-1 approval overlay).
pub struct ChannelApprover {
    pub event_tx: EventTx,
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn request(&self, approval: Approval) -> Decision {
        let (reply, rx) = oneshot::channel();
        let request = ApprovalRequest {
            id: Uuid::new_v4(),
            kind: to_kind(approval),
            reply,
        };
        if self
            .event_tx
            .send(AppEvent::ApprovalRequested(request))
            .await
            .is_err()
        {
            return Decision::Deny;
        }
        match rx.await {
            Ok(ApprovalDecision::Deny) | Err(_) => Decision::Deny,
            // AllowOnce / AlwaysAllow both proceed; persisting AlwaysAllow is a
            // follow-up.
            Ok(_) => Decision::Allow,
        }
    }
}

fn to_kind(approval: Approval) -> ApprovalKind {
    match approval {
        Approval::Command {
            command,
            outside_project,
        } => ApprovalKind::Command {
            cmd: command,
            outside_project,
        },
        Approval::FileEdit { path, old, new } => {
            ApprovalKind::FileEdit(DiffView { path, old, new })
        }
        Approval::OutsideProject { path, action } => {
            ApprovalKind::OutsideProject { path, action }
        }
        Approval::Mcp { server, tool } => ApprovalKind::Mcp { server, tool },
    }
}
