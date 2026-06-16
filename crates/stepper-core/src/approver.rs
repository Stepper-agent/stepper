use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use stepper_permission::{Decision, Rule, RuleSet};
use stepper_protocol::{
    AppEvent, ApprovalDecision, ApprovalKind, ApprovalRequest, DiffView, EventTx, NoticeLevel,
};
use stepper_tools::{Approval, Approver};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Bridges a tool's `Approval` to the TUI: emits `ApprovalRequested` carrying a
/// oneshot and awaits the user's decision. On `AlwaysAllow` it folds a scoped
/// allow rule into the live session rules and persists it, so the same action is
/// not re-prompted again.
pub struct ChannelApprover {
    pub event_tx: EventTx,
    /// The live session rules (the same cell the orchestrator snapshots each
    /// turn). An AlwaysAllow grant is folded in here.
    pub rules: Arc<RwLock<RuleSet>>,
    pub project_root: PathBuf,
    pub home: Option<PathBuf>,
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn request(&self, approval: Approval) -> Decision {
        // Build the scoped rule from the original variant before `to_kind`
        // consumes it (the TUI scope string drops the tool wrapper).
        let spec = always_allow_spec(&approval);
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
            Ok(ApprovalDecision::AllowOnce) => Decision::Allow,
            Ok(ApprovalDecision::AlwaysAllow { .. }) => {
                self.grant_always(&spec).await;
                Decision::Allow
            }
        }
    }
}

impl ChannelApprover {
    /// Fold a scoped allow rule into the live rules and persist it to
    /// setting.json, so the same action stops re-prompting. An unparseable spec
    /// is a one-shot Allow (warned, not persisted).
    async fn grant_always(&self, spec: &str) {
        if Rule::parse(spec).is_none() {
            self.warn(format!(
                "could not always-allow '{spec}' — kept as a one-time approval"
            ))
            .await;
            return;
        }
        // Live-fold so the next ToolCx snapshot allows it this session.
        {
            let mut w = self.rules.write().unwrap();
            *w = w.extended(&[spec.to_string()], &[], &[]);
        }
        if let Err(e) = persist_approval(&self.project_root, self.home.as_deref(), spec) {
            self.warn(format!("always-allow not persisted to setting.json: {e}"))
                .await;
        }
    }

    async fn warn(&self, text: String) {
        let _ = self
            .event_tx
            .send(AppEvent::Notice {
                level: NoticeLevel::Warn,
                text,
            })
            .await;
    }
}

/// Map an `Approval` to a permission rule spec for an always-allow grant.
fn always_allow_spec(approval: &Approval) -> String {
    match approval {
        Approval::Command { command, .. } => format!("Bash({command})"),
        Approval::FileEdit { path, .. } => format!("Edit({})", rule_path(path)),
        Approval::OutsideProject { path, action } => {
            let tool = if matches!(action.as_str(), "read" | "search" | "list_dir") {
                "Read"
            } else {
                "Write"
            };
            format!("{tool}({})", rule_path(path))
        }
        Approval::Mcp { server, tool } => format!("Mcp({server}, {tool})"),
    }
}

/// Render a path for a rule spec: an absolute path uses the `//` anchor, a
/// relative one stays project-relative.
fn rule_path(path: &Path) -> String {
    if path.is_absolute() {
        format!("/{}", path.display())
    } else {
        path.display().to_string()
    }
}

/// Append an `ApprovalRule` for `spec` to the project (or user) setting.json
/// `approvals` array, de-duped on the rule string. Dir selection mirrors the
/// `/model` persistence: project `.stepper` if present, else `~/.stepper`.
fn persist_approval(project_root: &Path, home: Option<&Path>, spec: &str) -> std::io::Result<()> {
    let project = project_root.join(".stepper");
    let dir = if project.is_dir() {
        project
    } else if let Some(home) = home {
        home.join(".stepper")
    } else {
        return Ok(());
    };
    stepper_config::scaffold::update_settings(&dir, |obj| {
        let arr = obj
            .entry("approvals")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let Some(arr) = arr.as_array_mut() {
            let exists = arr
                .iter()
                .any(|v| v.get("rule").and_then(|r| r.as_str()) == Some(spec));
            if !exists {
                arr.push(serde_json::json!({ "rule": spec, "scope": "project" }));
            }
        }
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn always_allow_spec_maps_each_variant() {
        assert_eq!(
            always_allow_spec(&Approval::Command {
                command: "cargo test".into(),
                outside_project: false
            }),
            "Bash(cargo test)"
        );
        assert_eq!(
            always_allow_spec(&Approval::OutsideProject {
                path: PathBuf::from("/etc/hosts"),
                action: "read".into()
            }),
            "Read(//etc/hosts)"
        );
        assert_eq!(
            always_allow_spec(&Approval::OutsideProject {
                path: PathBuf::from("/tmp/out"),
                action: "write".into()
            }),
            "Write(//tmp/out)"
        );
        assert_eq!(
            always_allow_spec(&Approval::Mcp {
                server: "fs".into(),
                tool: "read".into()
            }),
            "Mcp(fs, read)"
        );
    }

    #[tokio::test]
    async fn grant_always_folds_the_rule_live_and_persists_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().to_path_buf();
        std::fs::create_dir_all(project.join(".stepper")).unwrap();
        let rules = Arc::new(RwLock::new(RuleSet::from_lists(&[], &[], &[])));
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let approver = ChannelApprover {
            event_tx: tx,
            rules: rules.clone(),
            project_root: project.clone(),
            home: None,
        };

        approver.grant_always("Bash(cargo test)").await;

        // Live fold: the rule is now in the session allow set.
        assert!(
            rules.read().unwrap().allow.iter().any(|r| r.matches(
                "Bash",
                &stepper_permission::MatchTarget::Command("cargo test"),
                &project,
                None
            )),
            "the always-allow rule is folded into the live rules"
        );
        // Persisted: setting.json approvals carries it.
        let written = std::fs::read_to_string(project.join(".stepper/setting.json")).unwrap();
        assert!(written.contains("Bash(cargo test)"), "persisted: {written}");

        // Idempotent: a second grant of the same spec doesn't duplicate it.
        approver.grant_always("Bash(cargo test)").await;
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(project.join(".stepper/setting.json")).unwrap()).unwrap();
        assert_eq!(parsed["approvals"].as_array().unwrap().len(), 1, "no duplicate approval");
    }
}
