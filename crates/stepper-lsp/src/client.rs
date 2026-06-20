//! A single language-server connection: spawn, `initialize`/`initialized`
//! handshake, document sync (`didOpen`/`didChange`), and collection of
//! server-pushed `textDocument/publishDiagnostics`. Diagnostics are gathered via
//! the **push** model (the model used by rust-analyzer, gopls, pyright,
//! typescript-language-server, …); the newer pull model
//! (`textDocument/diagnostic`) is a follow-up.

use crate::diagnostic::Diagnostic;
use crate::language::language_id;
use crate::protocol::{read_message, write_message};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::BufReader;
use tokio::process::{ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex, Notify};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(45);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// After a fresh publish lands, wait briefly for a possible second push (servers
/// often emit syntax then semantic diagnostics) before returning.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn language server '{0}': {1}")]
    Spawn(String, std::io::Error),
    #[error("language server io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("language server '{0}' did not respond to initialize in time")]
    InitializeTimeout(String),
    #[error("language server '{0}' closed the connection")]
    Closed(String),
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

struct State {
    diagnostics: Mutex<HashMap<PathBuf, Vec<Diagnostic>>>,
    published_at: Mutex<HashMap<PathBuf, Instant>>,
    notify: Notify,
}

/// A live connection to one language server rooted at a workspace directory.
pub struct LspClient {
    server_id: String,
    root: PathBuf,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Pending,
    state: Arc<State>,
    /// Open documents → their last version (so re-touch sends `didChange`).
    open: Mutex<HashMap<PathBuf, i64>>,
    _child: tokio::process::Child,
}

impl LspClient {
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Spawn `command` (argv) rooted at `root` and complete the LSP handshake.
    pub async fn spawn(
        server_id: impl Into<String>,
        command: &[String],
        env: &[(String, String)],
        root: &Path,
        initialization: Option<Value>,
    ) -> Result<LspClient, LspError> {
        let server_id = server_id.into();
        let (program, args) = command
            .split_first()
            .ok_or_else(|| LspError::Spawn(server_id.clone(), std::io::Error::other("empty command")))?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| LspError::Spawn(server_id.clone(), e))?;

        let stdin = Arc::new(AsyncMutex::new(child.stdin.take().expect("piped stdin")));
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new(State {
            diagnostics: Mutex::new(HashMap::new()),
            published_at: Mutex::new(HashMap::new()),
            notify: Notify::new(),
        });

        // Reader task: dispatch responses, server→client requests, and pushed
        // diagnostics until the server closes stdout.
        {
            let pending = pending.clone();
            let state = state.clone();
            let stdin = stdin.clone();
            let server_id = server_id.clone();
            let root = root.to_path_buf();
            let initialization = initialization.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                while let Ok(Some(msg)) = read_message(&mut reader).await {
                    handle_incoming(&msg, &pending, &state, &stdin, &server_id, &root, &initialization)
                        .await;
                }
                // EOF: fail any in-flight requests so callers don't hang.
                pending.lock().unwrap().clear();
            });
        }

        let client = LspClient {
            server_id,
            root: root.to_path_buf(),
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            state,
            open: Mutex::new(HashMap::new()),
            _child: child,
        };

        client.initialize(initialization).await?;
        Ok(client)
    }

    async fn initialize(&self, initialization: Option<Value>) -> Result<(), LspError> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": path_to_uri(&self.root),
            "workspaceFolders": [{ "name": "workspace", "uri": path_to_uri(&self.root) }],
            "initializationOptions": initialization.clone().unwrap_or(Value::Null),
            "capabilities": {
                "workspace": { "configuration": true, "workspaceFolders": true },
                "textDocument": {
                    "synchronization": { "didOpen": true, "didChange": true },
                    "publishDiagnostics": { "versionSupport": false }
                }
            }
        });
        match tokio::time::timeout(INITIALIZE_TIMEOUT, self.request("initialize", params)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(LspError::InitializeTimeout(self.server_id.clone())),
        }
        self.notify("initialized", json!({})).await?;
        if let Some(init) = initialization {
            self.notify("workspace/didChangeConfiguration", json!({ "settings": init }))
                .await?;
        }
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        {
            let mut stdin = self.stdin.lock().await;
            write_message(&mut *stdin, &msg).await?;
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(LspError::Closed(self.server_id.clone())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(LspError::Closed(self.server_id.clone()))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut stdin = self.stdin.lock().await;
        write_message(&mut *stdin, &msg).await?;
        Ok(())
    }

    /// Open or re-sync a document with the given text (read from disk by the
    /// caller). First touch sends `didOpen`; later touches send `didChange`.
    pub async fn open(&self, path: &Path, text: &str) -> Result<(), LspError> {
        let prior = self.open.lock().unwrap().get(path).copied();
        match prior {
            Some(version) => {
                let next = version + 1;
                self.open.lock().unwrap().insert(path.to_path_buf(), next);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": path_to_uri(path), "version": next },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await
            }
            None => {
                // A fresh open: drop any stale diagnostics so the next push is clean.
                self.state.diagnostics.lock().unwrap().remove(path);
                self.open.lock().unwrap().insert(path.to_path_buf(), 0);
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": path_to_uri(path),
                            "languageId": language_id(path),
                            "version": 0,
                            "text": text
                        }
                    }),
                )
                .await
            }
        }
    }

    /// Wait (up to `timeout`) for a `publishDiagnostics` for `path` that arrived
    /// after `after`, then return that file's diagnostics. On timeout, returns
    /// whatever is currently known (possibly empty).
    pub async fn wait_diagnostics(
        &self,
        path: &Path,
        after: Instant,
        timeout: Duration,
    ) -> Vec<Diagnostic> {
        let start = Instant::now();
        loop {
            // Register interest BEFORE checking, so a publish racing in between
            // check and await still wakes us (no lost notification).
            let notified = self.state.notify.notified();
            let fresh = self
                .state
                .published_at
                .lock()
                .unwrap()
                .get(path)
                .is_some_and(|at| *at >= after);
            if fresh {
                let remaining = timeout.saturating_sub(start.elapsed());
                tokio::time::sleep(SETTLE.min(remaining)).await;
                return self.current(path);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return self.current(path);
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => return self.current(path),
            }
        }
    }

    fn current(&self, path: &Path) -> Vec<Diagnostic> {
        self.state
            .diagnostics
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    /// Best-effort graceful shutdown (`shutdown` + `exit`); the child is killed on
    /// drop regardless.
    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
    }
}

async fn handle_incoming(
    msg: &Value,
    pending: &Pending,
    state: &Arc<State>,
    stdin: &Arc<AsyncMutex<ChildStdin>>,
    _server_id: &str,
    root: &Path,
    initialization: &Option<Value>,
) {
    // Response to one of our requests.
    if let Some(id) = msg.get("id").and_then(Value::as_i64)
        && (msg.get("result").is_some() || msg.get("error").is_some())
        && msg.get("method").is_none()
    {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(msg.clone());
        }
        return;
    }

    // Server→client request: must be answered with the same id.
    if let (Some(id), Some(method)) = (msg.get("id"), msg.get("method").and_then(Value::as_str)) {
        let result = match method {
            "workspace/configuration" => {
                let items = msg
                    .get("params")
                    .and_then(|p| p.get("items"))
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0);
                // Reply with the (single) initialization blob per requested section,
                // or null — enough to satisfy servers that gate on configuration.
                Value::Array(vec![
                    initialization.clone().unwrap_or(Value::Null);
                    items.max(1)
                ])
            }
            "workspace/workspaceFolders" => {
                json!([{ "name": "workspace", "uri": path_to_uri(root) }])
            }
            // registerCapability / progress / refresh and anything else: ack.
            _ => Value::Null,
        };
        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let mut guard = stdin.lock().await;
        let _ = write_message(&mut *guard, &reply).await;
        return;
    }

    // Notification.
    if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
        let Some(params) = msg.get("params") else { return };
        let Some(path) = params.get("uri").and_then(Value::as_str).and_then(uri_to_path) else {
            return;
        };
        let diags: Vec<Diagnostic> = params
            .get("diagnostics")
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .unwrap_or_default();
        state.diagnostics.lock().unwrap().insert(path.clone(), diags);
        state.published_at.lock().unwrap().insert(path, Instant::now());
        state.notify.notify_waiters();
    }
}

fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'/' | b':' | b'-' | b'.' | b'_' | b'~' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips_a_path_with_spaces() {
        let p = PathBuf::from("/tmp/a dir/file.rs");
        let uri = path_to_uri(&p);
        assert!(uri.starts_with("file:///tmp/a%20dir/"));
        assert_eq!(uri_to_path(&uri), Some(p));
    }
}
