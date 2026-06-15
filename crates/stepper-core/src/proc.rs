//! Background processes for `!cmd &` — the shell-view subsystem. A `!`-shell
//! command ending in `&` is spawned detached from the turn loop (no 120s
//! timeout, so `bun dev` survives), its stdout/stderr stream to the TUI as
//! `ProcessOutput`, and it stays killable from the shell view (Down key) via a
//! per-process `CancellationToken`.

use std::path::PathBuf;
use stepper_protocol::{AppEvent, NoticeLevel};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Spawn `command` as a tracked background process. Emits `ProcessStarted`, then
/// a `ProcessOutput` per stdout/stderr line, then `ProcessExited`. Cancelling
/// `token` (a `KillProcess` from the shell view) terminates it. The same
/// secret-file screen the bash tool applies still runs (background is `!`-shell,
/// which is user-initiated/permissionless but never allowed to touch secrets).
pub fn spawn_background(
    id: u64,
    command: String,
    cwd: PathBuf,
    home: Option<PathBuf>,
    tx: mpsc::Sender<AppEvent>,
    token: CancellationToken,
) {
    tokio::spawn(async move {
        match stepper_tools::secret::find_secret_path_in_command(&command, &cwd, home.as_deref()) {
            Ok(Some(path)) => {
                warn(
                    &tx,
                    format!(
                        "refusing background command touching secret file {}",
                        path.display()
                    ),
                )
                .await;
                return;
            }
            Ok(None) => {}
            Err(e) => {
                warn(&tx, format!("shell: {e}")).await;
                return;
            }
        }

        let mut child = match Command::new("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn(&tx, format!("shell: spawn failed: {e}")).await;
                return;
            }
        };

        let _ = tx
            .send(AppEvent::ProcessStarted { id, command: command.clone() })
            .await;

        // Stream stdout and stderr line-by-line into the shell console.
        if let Some(out) = child.stdout.take() {
            spawn_line_reader(id, out, tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_line_reader(id, err, tx.clone());
        }

        let code = tokio::select! {
            _ = token.cancelled() => {
                let _ = child.kill().await;
                None
            }
            status = child.wait() => status.ok().and_then(|s| s.code()),
        };
        let _ = tx.send(AppEvent::ProcessExited { id, code }).await;
    });
}

/// Forward each line of a child stream to the TUI as `ProcessOutput`.
fn spawn_line_reader<R>(id: u64, reader: R, tx: mpsc::Sender<AppEvent>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(AppEvent::ProcessOutput { id, line }).await.is_err() {
                break;
            }
        }
    });
}

async fn warn(tx: &mpsc::Sender<AppEvent>, text: String) {
    let _ = tx
        .send(AppEvent::Notice {
            level: NoticeLevel::Warn,
            text,
        })
        .await;
}

/// Split a `!`-shell command into `(inner, is_background)`: a trailing `&`
/// (the shell background operator) marks a background spawn and is stripped.
pub fn parse_background(command: &str) -> (String, bool) {
    let trimmed = command.trim_end();
    match trimmed.strip_suffix('&') {
        Some(inner) => (inner.trim().to_string(), true),
        None => (trimmed.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_background_detects_trailing_ampersand() {
        assert_eq!(parse_background("bun dev &"), ("bun dev".into(), true));
        assert_eq!(parse_background("  npm run watch  &  "), ("npm run watch".into(), true));
        assert_eq!(parse_background("ls -la"), ("ls -la".into(), false));
        // a non-trailing `&` (e.g. `a & b`) is not the background marker here.
        assert_eq!(parse_background("a & b"), ("a & b".into(), false));
    }
}
