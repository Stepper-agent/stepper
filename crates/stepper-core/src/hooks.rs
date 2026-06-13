use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use stepper_config::HookEntry;
use tokio::io::AsyncWriteExt;

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether a lifecycle hook lets the action proceed.
#[derive(Debug, Clone)]
pub enum HookDecision {
    Continue,
    /// A `PreToolUse` hook blocked the tool; the string is the reason fed back to
    /// the model.
    Block(String),
}

/// Runs configured lifecycle hooks (`SessionStart`, `PreToolUse`, `PostToolUse`,
/// `Stop`, …). Each hook is a shell command; a non-zero exit from a `PreToolUse`
/// hook blocks the tool.
pub struct HookHost {
    hooks: BTreeMap<String, Vec<HookEntry>>,
    cwd: PathBuf,
}

impl HookHost {
    pub fn new(hooks: BTreeMap<String, Vec<HookEntry>>, cwd: PathBuf) -> Self {
        HookHost { hooks, cwd }
    }

    pub fn empty(cwd: PathBuf) -> Self {
        HookHost {
            hooks: BTreeMap::new(),
            cwd,
        }
    }

    pub fn has_any(&self) -> bool {
        self.hooks.values().any(|v| !v.is_empty())
    }

    /// Run every hook registered for `event` whose matcher matches
    /// `matcher_target`, feeding `payload` as JSON on stdin. The first non-zero
    /// exit becomes a `Block`.
    pub async fn run(
        &self,
        event: &str,
        matcher_target: Option<&str>,
        payload: &Value,
    ) -> HookDecision {
        let Some(entries) = self.hooks.get(event) else {
            return HookDecision::Continue;
        };
        for entry in entries {
            if let Some(matcher) = &entry.matcher
                && !matcher_matches(matcher, matcher_target)
            {
                continue;
            }
            match self.run_one(&entry.command, payload).await {
                Ok((code, out)) if code != 0 => {
                    let reason = if out.trim().is_empty() {
                        format!("blocked by {event} hook (exit {code})")
                    } else {
                        out.trim().to_string()
                    };
                    return HookDecision::Block(reason);
                }
                _ => {}
            }
        }
        HookDecision::Continue
    }

    async fn run_one(&self, command: &str, payload: &Value) -> std::io::Result<(i32, String)> {
        let mut child = tokio::process::Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
            drop(stdin);
        }

        let output = match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            _ => return Ok((1, "hook timed out".into())),
        };
        let code = output.status.code().unwrap_or(1);
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((code, text))
    }
}

fn matcher_matches(matcher: &str, target: Option<&str>) -> bool {
    if matcher == "*" || matcher.is_empty() {
        return true;
    }
    // Exact (case-insensitive) match only — a `bash` matcher must not catch
    // `bash_profile`. Use `*` for match-all.
    target.is_some_and(|t| t.eq_ignore_ascii_case(matcher))
}
