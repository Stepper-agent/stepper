//! The model-callable `exit_plan_mode` tool — the plan-mode handshake.
//!
//! Registered only while the session is in Plan mode (read-only). The model
//! calls it with its completed plan; the plan is surfaced for approval, and on
//! approval the shared permission mode flips to AcceptEdits so the same turn may
//! begin executing. On rejection the session stays read-only and the feedback is
//! returned so the model keeps planning.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use stepper_permission::{PermissionMode, PermissionRequest};
use stepper_provider::{ToolError, ToolResult, ToolSpec};
use stepper_tools::{Approval, Tool, ToolCx};

pub struct ExitPlanTool {
    spec: ToolSpec,
    /// The shared session mode cell (the same one the orchestrator holds and the
    /// sequential layer's gate reads), flipped to AcceptEdits on approval.
    mode_cell: Arc<RwLock<PermissionMode>>,
}

#[derive(Deserialize)]
struct Args {
    plan: String,
}

impl ExitPlanTool {
    pub fn new(mode_cell: Arc<RwLock<PermissionMode>>) -> Self {
        ExitPlanTool {
            spec: ToolSpec {
                name: "exit_plan_mode".into(),
                description: "Present your completed plan for approval. On approval the session \
                              leaves read-only plan mode and you may begin making changes; on \
                              rejection, stay in plan mode and refine the plan from the feedback. \
                              Call with {\"plan\": \"<your plan>\"} only once the plan is ready."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "plan": {"type": "string"} },
                    "required": ["plan"]
                }),
                read_only: false,
                parallel_safe: false,
            },
            mode_cell,
        }
    }
}

#[async_trait]
impl Tool for ExitPlanTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let Args { plan } = serde_json::from_value(args).map_err(|e| {
            ToolError::InvalidArgs(format!("exit_plan_mode expects {{\"plan\": \"…\"}}: {e}"))
        })?;
        match cx
            .gate(
                PermissionRequest::Other {
                    tool: "exit_plan".into(),
                    arg: plan.clone(),
                },
                Approval::Command {
                    command: plan,
                    outside_project: false,
                },
            )
            .await
        {
            Ok(()) => {
                *self.mode_cell.write().unwrap() = PermissionMode::AcceptEdits;
                Ok(ToolResult::text(
                    "Plan approved — plan mode lifted. You may now make changes; proceed.",
                ))
            }
            // A rejection is not a tool error, so the model keeps planning.
            Err(ToolError::Denied(msg)) => Ok(ToolResult::text(format!(
                "Plan not approved ({msg}). Stay in plan mode and refine the plan."
            ))),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use stepper_permission::{Decision, RuleSet};
    use stepper_tools::Approver;
    use tokio_util::sync::CancellationToken;

    struct FixedApprover(Decision);
    #[async_trait]
    impl Approver for FixedApprover {
        async fn request(&self, _approval: Approval) -> Decision {
            self.0
        }
    }

    fn plan_cx(cell: Arc<RwLock<PermissionMode>>, verdict: Decision) -> ToolCx {
        ToolCx {
            cwd: PathBuf::from("/project"),
            project_root: PathBuf::from("/project"),
            home: None,
            mode: PermissionMode::Plan,
            live_mode: Some(cell),
            rules: Arc::new(RuleSet::from_lists(&[], &[], &[])),
            approver: Arc::new(FixedApprover(verdict)),
            cancel: CancellationToken::new(),
            sandbox_writable_roots: None,
        }
    }

    #[tokio::test]
    async fn approved_plan_flips_mode_to_accept_edits() {
        let cell = Arc::new(RwLock::new(PermissionMode::Plan));
        let tool = ExitPlanTool::new(cell.clone());
        let cx = plan_cx(cell.clone(), Decision::Allow);
        let res = tool.call(json!({ "plan": "do the thing" }), &cx).await.unwrap();
        assert!(res.content_text().contains("approved"), "got: {}", res.content_text());
        assert_eq!(*cell.read().unwrap(), PermissionMode::AcceptEdits);
    }

    #[tokio::test]
    async fn rejected_plan_stays_in_plan_mode_without_erroring() {
        let cell = Arc::new(RwLock::new(PermissionMode::Plan));
        let tool = ExitPlanTool::new(cell.clone());
        let cx = plan_cx(cell.clone(), Decision::Deny);
        // A rejection returns Ok with feedback (not an error), so the model plans on.
        let res = tool.call(json!({ "plan": "do the thing" }), &cx).await.unwrap();
        assert!(res.content_text().contains("not approved"));
        assert_eq!(*cell.read().unwrap(), PermissionMode::Plan);
    }
}
