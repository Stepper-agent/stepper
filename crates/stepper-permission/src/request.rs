use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a tool wants to do, normalized to the dimension the permission engine
/// reasons about (a command, a path, or an MCP call).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PermissionRequest {
    /// A (possibly compound) shell command.
    Bash(String),
    Read(PathBuf),
    Write(PathBuf),
    Edit(PathBuf),
    WebFetch(String),
    Mcp { server: String, tool: String },
    /// Any other tool, matched by name + a free-form argument.
    Other { tool: String, arg: String },
}

impl PermissionRequest {
    /// The capability name used in rule specifiers (`Bash`, `Read`, …).
    pub fn tool(&self) -> &str {
        match self {
            PermissionRequest::Bash(_) => "Bash",
            PermissionRequest::Read(_) => "Read",
            PermissionRequest::Write(_) => "Write",
            PermissionRequest::Edit(_) => "Edit",
            PermissionRequest::WebFetch(_) => "WebFetch",
            PermissionRequest::Mcp { .. } => "Mcp",
            PermissionRequest::Other { tool, .. } => tool,
        }
    }

    pub fn is_read_only(&self) -> bool {
        matches!(self, PermissionRequest::Read(_))
    }

    /// Whether this touches the filesystem at a path (drives outside-project
    /// checks).
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            PermissionRequest::Read(p)
            | PermissionRequest::Write(p)
            | PermissionRequest::Edit(p) => Some(p),
            _ => None,
        }
    }

    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            PermissionRequest::Bash(_)
                | PermissionRequest::Write(_)
                | PermissionRequest::Edit(_)
                | PermissionRequest::WebFetch(_)
                | PermissionRequest::Mcp { .. }
        )
    }
}

/// The startup/mode of the session — sets the *default* before explicit rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// In-project auto-approved; outside-project asks.
    Auto,
    /// Read-only; mutations are denied/asked.
    Plan,
    /// File edits auto-approved; bash/outside evaluated normally.
    AcceptEdits,
    /// Read-only tools allowed without a prompt; everything else asks.
    Default,
    /// Anything that would ask is auto-DENIED (CI-safe headless posture).
    DontAsk,
    /// Anything that would ask is allowed; explicit deny rules still deny.
    /// Only reachable via `--dangerously-skip-permissions`.
    Bypass,
}

/// The engine's verdict. `deny` > `ask` > `allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

impl Decision {
    /// The more restrictive of two decisions (`Deny` > `Ask` > `Allow`).
    pub fn restrict(self, other: Decision) -> Decision {
        self.max(other)
    }
}

impl PartialOrd for Decision {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decision {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(d: &Decision) -> u8 {
            match d {
                Decision::Allow => 0,
                Decision::Ask => 1,
                Decision::Deny => 2,
            }
        }
        rank(self).cmp(&rank(other))
    }
}
