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

        // Write stdin while draining stdout/stderr CONCURRENTLY, all under the
        // timeout: a hook that fills its stdout without reading stdin would
        // otherwise deadlock the blocking write (which the timeout never covered).
        let stdin = child.stdin.take();
        let payload_bytes = payload.to_string().into_bytes();
        let writer = async move {
            if let Some(mut s) = stdin {
                let _ = s.write_all(&payload_bytes).await;
                // dropping `s` closes the pipe so the hook sees EOF
            }
        };
        let output = match tokio::time::timeout(HOOK_TIMEOUT, async {
            let (_, out) = tokio::join!(writer, child.wait_with_output());
            out
        })
        .await
        {
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
    match target {
        // Tool events: exact (case-insensitive) match only — a `bash` matcher
        // must not catch `bash_profile`. Use `*` for match-all.
        Some(t) => t.eq_ignore_ascii_case(matcher),
        // Lifecycle events (SessionStart/Stop) have no tool to match against, so a
        // matcher is meaningless there — the hook fires regardless (a stray
        // non-`*` matcher used to silently disable the hook).
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::matcher_matches;

    #[test]
    fn matcher_gates_tool_events_but_not_lifecycle_events() {
        // Tool events: exact (case-insensitive) match.
        assert!(matcher_matches("bash", Some("bash")));
        assert!(matcher_matches("Bash", Some("bash")));
        assert!(!matcher_matches("bash", Some("bash_profile")));
        assert!(matcher_matches("*", Some("anything")));
        // Lifecycle events have no tool target — a stray matcher must not silently
        // disable the hook.
        assert!(matcher_matches("anything", None));
        assert!(matcher_matches("*", None));
        assert!(matcher_matches("", None));
    }
}
