use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use stepper_config::HookEntry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-pipe cap on captured hook output — a noisy/runaway hook must not be able to
/// buffer unbounded bytes into the agent's memory. Excess is drained but dropped.
const MAX_HOOK_OUTPUT: usize = 256 * 1024;

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
        cancel: &CancellationToken,
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
            match self.run_one(&entry.command, payload, cancel).await {
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

    async fn run_one(
        &self,
        command: &str,
        payload: &Value,
        cancel: &CancellationToken,
    ) -> std::io::Result<(i32, String)> {
        let mut child = tokio::process::Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        // Take the pipes OUT of the child so `child.wait()` can run alongside the
        // drains without borrow conflicts.
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let payload_bytes = payload.to_string().into_bytes();
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();

        // Race the turn's cancellation (Esc) against the run so a hung hook is
        // interruptible like the rest of the loop; on cancel we drop `child`
        // (kill_on_drop reaps bash) and let the action proceed (code 0 = Continue).
        let timed = tokio::time::timeout(HOOK_TIMEOUT, async {
            // Write stdin and drain both pipes CONCURRENTLY (so a hook that fills a
            // pipe can't deadlock the stdin write). Completion is the CHILD EXITING
            // (`child.wait()`), NOT pipe EOF: a hook that backgrounds a subprocess
            // leaves stdout/stderr open even after bash exits, so waiting for EOF
            // (the old `wait_with_output`) stalled the full timeout on every such
            // hook and then mis-reported it as a Block.
            let pump = async {
                tokio::join!(
                    async {
                        if let Some(mut s) = stdin {
                            let _ = s.write_all(&payload_bytes).await;
                            // drop closes stdin → the hook sees EOF
                        }
                    },
                    read_capped(&mut stdout, &mut out_buf, MAX_HOOK_OUTPUT),
                    read_capped(&mut stderr, &mut err_buf, MAX_HOOK_OUTPUT),
                );
            };
            tokio::pin!(pump);
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    // The child exited; give the drains a brief, BOUNDED moment to
                    // collect output buffered right before exit (the normal
                    // write-then-exit hook) without re-stalling on an orphaned pipe.
                    let _ = tokio::time::timeout(Duration::from_millis(500), &mut pump).await;
                    Ok::<_, std::io::Error>(status)
                }
                // Both pipes closed and stdin finished before the child reaped —
                // just collect the exit status.
                _ = &mut pump => Ok(child.wait().await?),
            }
        });

        let result = tokio::select! {
            _ = cancel.cancelled() => return Ok((0, "hook cancelled".into())),
            r = timed => r,
        };

        let status = match result {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Ok((1, "hook timed out".into())),
        };
        let code = status.code().unwrap_or(1);
        let mut text = String::from_utf8_lossy(&out_buf).into_owned();
        text.push_str(&String::from_utf8_lossy(&err_buf));
        Ok((code, text))
    }
}

/// Drain a child pipe to EOF, keeping at most `max` bytes (excess is read but
/// dropped so the writer side never blocks). Cancellation-safe: if the enclosing
/// future is dropped mid-read, the bytes already in `buf` are preserved.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    buf: &mut Vec<u8>,
    max: usize,
) {
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
    use super::{matcher_matches, HookHost};
    use serde_json::json;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    #[tokio::test(flavor = "multi_thread")]
    async fn hook_that_backgrounds_a_child_returns_at_bash_exit_not_pipe_eof() {
        // bash exits immediately but backgrounds a long sleep that inherits the
        // stdout/stderr pipes. The old `wait_with_output` blocked on pipe EOF (the
        // backgrounded child) for the full 30s timeout; we must return at bash's
        // own exit (~instant) with its real exit code, far under the timeout.
        let host = HookHost::empty(std::env::temp_dir());
        let started = Instant::now();
        let (code, out) = host
            .run_one("( sleep 30 ) & echo done; exit 0", &json!({}), &CancellationToken::new())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(8), "returned at bash exit, not the 30s pipe-EOF stall");
        assert_eq!(code, 0, "the real exit code, not the timeout's synthetic 1");
        assert!(out.contains("done"), "captured the pre-exit output: {out:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hook_output_is_capped_not_unbounded() {
        // A hook that floods stdout must not buffer unbounded memory; output is
        // capped (and the process still completes rather than deadlocking).
        let host = HookHost::empty(std::env::temp_dir());
        let (code, out) = host
            .run_one("head -c 2000000 /dev/zero | tr '\\0' 'x'; exit 0", &json!({}), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(code, 0);
        assert!(out.len() <= super::MAX_HOOK_OUTPUT, "captured output is capped: {} bytes", out.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_token_interrupts_a_hung_hook_without_blocking() {
        // A genuinely hung hook (sleep 30) must yield to Esc/cancel well before the
        // 30s timeout, and a cancelled hook does not block the action (code 0).
        let host = HookHost::empty(std::env::temp_dir());
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        let started = Instant::now();
        let (code, _) = host.run_one("sleep 30", &json!({}), &cancel).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "cancel interrupts before the 30s timeout");
        assert_eq!(code, 0, "a cancelled hook does not Block the action");
    }

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
