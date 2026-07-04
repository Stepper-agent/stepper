use crate::context::{Approval, ReadGate, ToolCx};
use crate::secret::is_secret_path_resolved;
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use globset::Glob;
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};
use tokio_util::sync::CancellationToken;

const MAX_MATCHES: usize = 200;

pub struct Grep {
    spec: ToolSpec,
}
pub struct GlobTool {
    spec: ToolSpec,
}
pub struct ListDir {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

impl Default for Grep {
    fn default() -> Self {
        Grep {
            spec: ToolSpec {
                name: "grep".into(),
                description: "Regex search across files (gitignore-aware). Returns matching \
                              `path:line: text` lines."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "path": {"type": "string", "description": "dir or file to search (default cwd)"}
                    },
                    "required": ["pattern"]
                }),
                read_only: true,
                parallel_safe: true,
            },
        }
    }
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: GrepArgs = parse_args(args)?;
        let base = a.path.as_deref().map(|p| cx.resolve(p)).unwrap_or(cx.cwd.clone());
        cx.gate(
            PermissionRequest::Read(base.clone()),
            Approval::OutsideProject {
                path: base.clone(),
                action: "search".into(),
            },
        )
        .await?;
        let regex =
            Regex::new(&a.pattern).map_err(|e| ToolError::InvalidArgs(format!("bad regex: {e}")))?;

        let gate = cx.read_gate();
        let cancel = cx.cancel.clone();
        let result = tokio::task::spawn_blocking(move || grep_walk(&base, &regex, &gate, &cancel))
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult::text(result))
    }
}

/// The grep/glob/list walk: gitignore-aware (build dirs like `target/` stay out)
/// but dotfiles such as `.github/` or `.gitignore` are visible to the agent;
/// only the `.git` dir itself is pruned. Secret files are filtered by callers.
fn project_walker(base: &Path) -> WalkBuilder {
    let mut w = WalkBuilder::new(base);
    w.hidden(false).filter_entry(|e| e.file_name() != ".git");
    w
}

fn grep_walk(base: &Path, regex: &Regex, gate: &ReadGate, cancel: &CancellationToken) -> String {
    let mut out = Vec::new();
    for entry in project_walker(base).build().flatten() {
        // The blocking walk can't be aborted, so observe the turn's cancel token
        // cooperatively — Esc during a large-tree search returns promptly instead
        // of running to completion.
        if cancel.is_cancelled() {
            break;
        }
        if out.len() >= MAX_MATCHES {
            out.push("… [more matches truncated]".to_string());
            break;
        }
        let path = entry.path();
        if !path.is_file() || is_secret_path_resolved(path) || gate.denies(path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                out.push(format!("{}:{}: {}", path.display(), i + 1, line.trim_end()));
                if out.len() >= MAX_MATCHES {
                    break;
                }
            }
        }
    }
    if out.is_empty() {
        "no matches".to_string()
    } else {
        out.join("\n")
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        GlobTool {
            spec: ToolSpec {
                name: "glob".into(),
                description: "Find files matching a glob pattern (gitignore-aware).".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "e.g. **/*.rs"},
                        "path": {"type": "string"}
                    },
                    "required": ["pattern"]
                }),
                read_only: true,
                parallel_safe: true,
            },
        }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: GlobArgs = parse_args(args)?;
        let base = a.path.as_deref().map(|p| cx.resolve(p)).unwrap_or(cx.cwd.clone());
        cx.gate(
            PermissionRequest::Read(base.clone()),
            Approval::OutsideProject {
                path: base.clone(),
                action: "glob".into(),
            },
        )
        .await?;
        let glob = Glob::new(&a.pattern)
            .map_err(|e| ToolError::InvalidArgs(format!("bad glob: {e}")))?
            .compile_matcher();

        let gate = cx.read_gate();
        let cancel = cx.cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for entry in project_walker(&base).build().flatten() {
                if cancel.is_cancelled() {
                    break;
                }
                let path = entry.path();
                if is_secret_path_resolved(path) || gate.denies(path) {
                    continue;
                }
                let rel = path.strip_prefix(&base).unwrap_or(path);
                if path.is_file() && (glob.is_match(rel) || glob.is_match(path)) {
                    out.push(path.display().to_string());
                    if out.len() >= MAX_MATCHES {
                        break;
                    }
                }
            }
            if out.is_empty() {
                "no files matched".to_string()
            } else {
                out.join("\n")
            }
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult::text(result))
    }
}

impl Default for ListDir {
    fn default() -> Self {
        ListDir {
            spec: ToolSpec {
                name: "list_dir".into(),
                description: "List the immediate entries of a directory (gitignore-aware).".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "path": {"type": "string"} }
                }),
                read_only: true,
                parallel_safe: true,
            },
        }
    }
}

#[async_trait]
impl Tool for ListDir {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: ListArgs = parse_args(args)?;
        let base = a.path.as_deref().map(|p| cx.resolve(p)).unwrap_or(cx.cwd.clone());
        cx.gate(
            PermissionRequest::Read(base.clone()),
            Approval::OutsideProject {
                path: base.clone(),
                action: "list_dir".into(),
            },
        )
        .await?;

        let gate = cx.read_gate();
        let result = tokio::task::spawn_blocking(move || {
            let mut entries = Vec::new();
            for entry in project_walker(&base)
                .max_depth(Some(1))
                .build()
                .flatten()
            {
                if entry.path() == base
                    || is_secret_path_resolved(entry.path())
                    || gate.denies(entry.path())
                {
                    continue;
                }
                let suffix = if entry.path().is_dir() { "/" } else { "" };
                entries.push(format!(
                    "{}{suffix}",
                    entry.file_name().to_string_lossy()
                ));
            }
            entries.sort();
            if entries.is_empty() {
                "empty".to_string()
            } else {
                entries.join("\n")
            }
        })
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult::text(result))
    }
}
