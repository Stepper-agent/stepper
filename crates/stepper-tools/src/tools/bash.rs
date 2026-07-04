use crate::context::{Approval, ToolCx};
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_OUTPUT: usize = 30_000;

/// Defense-in-depth under the permission engine: refuse to run a command if any
/// path-looking token resolves to a secret file — `cat ~/.ssh/id_rsa` must not
/// bypass the read_file guard. Delegates to the shared screen so the bash tool
/// and the background-process path stay identical. An untokenizable command
/// fails closed.
fn find_secret_path_token(command: &str, cx: &ToolCx) -> Result<Option<PathBuf>, ToolError> {
    crate::secret::find_secret_path_in_command(command, &cx.cwd, cx.home.as_deref())
        .map_err(|e| ToolError::Denied(format!("refusing to run command: {e}")))
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
        // permission-vetted command. With the opt-in OS sandbox enabled this argv
        // is rewritten to run through `/usr/bin/sandbox-exec`, confining writes to
        // the project's writable roots as a best-effort backstop under the gate.
        let mut argv = vec!["bash".to_string(), "-c".to_string(), a.command.clone()];
        if let Some(roots) = cx.sandbox_writable_roots.as_deref() {
            argv = crate::sandbox::confine_argv(argv, roots);
        }
        let (program, rest) = argv.split_first().expect("argv always has a program");
        let mut child = tokio::process::Command::new(program)
            .args(rest)
            .current_dir(&cx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn failed: {e}")))?;

        let mut stdout = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let mut out_buf: Vec<u8> = Vec::new();
        let mut err_buf: Vec<u8> = Vec::new();

        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        // Completion is the CHILD EXITING (`child.wait()`), NOT pipe EOF: a
        // command that backgrounds a subprocess (`./server & curl …`) leaves the
        // pipes open after bash exits, and the old `wait_with_output` stalled the
        // full timeout and then mis-reported a phantom timeout (same defect the
        // hook runner fixed). Pipes are drained concurrently, capped so a
        // firehose can't grow the buffers unbounded. On cancel or timeout the
        // futures (which borrow `child`) are dropped and `kill_on_drop`
        // terminates the process.
        let run = async {
            let pump = async {
                tokio::join!(
                    read_capped(&mut stdout, &mut out_buf, PIPE_CAP),
                    read_capped(&mut stderr_pipe, &mut err_buf, PIPE_CAP),
                );
            };
            tokio::pin!(pump);
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    // The child exited; give the drains a brief, BOUNDED moment
                    // to collect output buffered right before exit without
                    // re-stalling on a pipe an orphaned grandchild still holds.
                    let _ = tokio::time::timeout(Duration::from_millis(500), &mut pump).await;
                    Ok::<_, std::io::Error>(status)
                }
                _ = &mut pump => child.wait().await,
            }
        };
        let status = tokio::select! {
            _ = cx.cancel.cancelled() => {
                return Err(ToolError::Execution("command cancelled".into()));
            }
            result = tokio::time::timeout(timeout, run) => match result {
                Err(_) => return Err(ToolError::Execution(format!(
                    "command timed out after {}ms", timeout.as_millis()
                ))),
                Ok(Ok(status)) => status,
                Ok(Err(e)) => return Err(ToolError::Execution(e.to_string())),
            },
        };

        let mut body = String::new();
        body.push_str(&String::from_utf8_lossy(&out_buf));
        let stderr = String::from_utf8_lossy(&err_buf);
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
        if let Some(code) = status.code()
            && code != 0
        {
            body.push_str(&format!("\n[exit code {code}]"));
        }

        Ok(ToolResult {
            content: vec![stepper_provider::ToolContent::text(body)],
            is_error: !status.success(),
            truncated,
        })
    }
}

/// Per-pipe drain cap: one byte past the tool's output cap so the truncation
/// marker still trips when a pipe was cut, while excess bytes are read and
/// dropped (the writer side never blocks).
const PIPE_CAP: usize = MAX_OUTPUT + 1;

/// Drain a child pipe to EOF, keeping at most `max` bytes. Cancellation-safe:
/// if the enclosing future is dropped mid-read, bytes already in `buf` remain.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    buf: &mut Vec<u8>,
    max: usize,
) {
    use tokio::io::AsyncReadExt;
    let Some(p) = pipe.as_mut() else { return };
    let mut chunk = [0u8; 8192];
    loop {
        match p.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < max {
                    let room = max - buf.len();
                    buf.extend_from_slice(&chunk[..room.min(n)]);
                }
            }
        }
    }
}
