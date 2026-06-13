use crate::context::{Approval, ToolCx};
use crate::secret::{is_secret_path, is_secret_path_resolved};
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT: usize = 30_000;

/// Defense-in-depth under the permission engine: tokenize the command
/// (quote-aware) and refuse to run it at all if any path-looking token resolves
/// to a secret file — `cat ~/.ssh/id_rsa` must not bypass the read_file guard.
/// An untokenizable command fails closed.
fn find_secret_path_token(command: &str, cx: &ToolCx) -> Result<Option<PathBuf>, ToolError> {
    let tokens = shell_words::split(command).map_err(|e| {
        ToolError::Denied(format!(
            "refusing to run command that cannot be tokenized for secret-path screening: {e}"
        ))
    })?;
    for token in tokens {
        let looks_like_path =
            token.contains('/') || token.starts_with('~') || is_secret_path(Path::new(&token));
        if !looks_like_path {
            continue;
        }
        let expanded = match (token.strip_prefix("~/"), cx.home.as_deref()) {
            (Some(rest), Some(home)) => home.join(rest),
            _ if token == "~" && cx.home.is_some() => cx.home.clone().unwrap(),
            _ => PathBuf::from(&token),
        };
        let abs = if expanded.is_absolute() {
            expanded
        } else {
            cx.cwd.join(expanded)
        };
        if is_secret_path_resolved(&abs) {
            return Ok(Some(abs));
        }
    }
    Ok(None)
}

pub struct Bash {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Default for Bash {
    fn default() -> Self {
        Bash {
            spec: ToolSpec {
                name: "bash".into(),
                description: "Run a shell command in the project working directory. Compound \
                              commands are permission-gated per component."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout_ms": {"type": "integer"}
                    },
                    "required": ["command"]
                }),
                read_only: false,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: Args = parse_args(args)?;
        if let Some(secret) = find_secret_path_token(&a.command, cx)? {
            return Err(ToolError::Denied(format!(
                "refusing to run command touching secret file {}",
                secret.display()
            )));
        }
        cx.gate(
            PermissionRequest::Bash(a.command.clone()),
            Approval::Command {
                command: a.command.clone(),
                outside_project: false,
            },
        )
        .await?;

        // `bash -c` (not `-lc`): don't source the user's login profile into a
        // permission-vetted command.
        let child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&a.command)
            .current_dir(&cx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn failed: {e}")))?;

        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        // On cancel or timeout the future (which owns `child`) is dropped, and
        // `kill_on_drop` terminates the process.
        let output = tokio::select! {
            _ = cx.cancel.cancelled() => {
                return Err(ToolError::Execution("command cancelled".into()));
            }
            result = tokio::time::timeout(timeout, child.wait_with_output()) => match result {
                Err(_) => return Err(ToolError::Execution(format!(
                    "command timed out after {}ms", timeout.as_millis()
                ))),
                Ok(Ok(output)) => output,
                Ok(Err(e)) => return Err(ToolError::Execution(e.to_string())),
            },
        };

        let mut body = String::new();
        body.push_str(&String::from_utf8_lossy(&output.stdout));
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&stderr);
        }
        let truncated = body.len() > MAX_OUTPUT;
        if truncated {
            crate::truncate_on_char_boundary(&mut body, MAX_OUTPUT);
            body.push_str("\n… [output truncated]");
        }
        if let Some(code) = output.status.code()
            && code != 0
        {
            body.push_str(&format!("\n[exit code {code}]"));
        }

        Ok(ToolResult {
            content: vec![stepper_provider::ToolContent::text(body)],
            is_error: !output.status.success(),
            truncated,
        })
    }
}
