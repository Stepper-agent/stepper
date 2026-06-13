use crate::bridge::claim_namespaced_name;
use crate::error::McpError;
use crate::tool::McpTool;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use stepper_config::McpServerConfig;
use stepper_tools::Tool;
use tokio::io::AsyncReadExt;

/// Bounded in-memory tail of a stdio server's stderr — enough for connect error
/// context without letting a chatty server balloon memory.
const STDERR_TAIL_BYTES: usize = 4096;

/// How long a failed connect waits for the stderr drain to hit EOF before
/// snapshotting the tail (an already-exited child flushes completely).
const STDERR_EOF_GRACE_MS: u64 = 200;

/// Per-server connect/handshake deadline. A server that spawns/accepts but never
/// completes the MCP handshake would otherwise hang the whole CLI at startup.
/// Override with `STEPPER_MCP_CONNECT_TIMEOUT_MS` (Claude Code's `MCP_TIMEOUT`).
fn connect_timeout() -> Duration {
    let ms = std::env::var("STEPPER_MCP_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10_000);
    Duration::from_millis(ms)
}

/// Owns the live MCP client connections for a session and exposes their tools as
/// native `Tool`s. The connections stay open for the manager's lifetime — drop
/// it (or call `shutdown`) to close them.
pub struct McpManager {
    services: Vec<RunningService<RoleClient, ()>>,
    tools: Vec<Arc<dyn Tool>>,
    /// Which server each namespaced tool came from (for per-layer `mcp.allow`).
    origins: Vec<(String, String)>,
}

impl McpManager {
    pub fn empty() -> Self {
        McpManager {
            services: Vec::new(),
            tools: Vec::new(),
            origins: Vec::new(),
        }
    }

    /// Connect every configured server. A server that fails to connect is logged
    /// and skipped — it never takes down the agent.
    pub async fn connect(servers: &BTreeMap<String, McpServerConfig>) -> Self {
        let mut manager = McpManager::empty();
        let mut taken = HashMap::new();
        for (name, cfg) in servers {
            match connect_one(name, cfg, &mut taken).await {
                Ok((service, tools)) => {
                    for tool in &tools {
                        manager
                            .origins
                            .push((tool.name().to_string(), name.clone()));
                    }
                    manager.services.push(service);
                    manager.tools.extend(tools);
                }
                Err(e) => eprintln!("mcp: server '{name}' unavailable: {e}"),
            }
        }
        manager
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// The server a namespaced tool belongs to.
    pub fn server_of(&self, tool_name: &str) -> Option<&str> {
        self.origins
            .iter()
            .find(|(n, _)| n == tool_name)
            .map(|(_, s)| s.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub async fn shutdown(self) {
        for service in self.services {
            let _ = service.cancel().await;
        }
    }
}

async fn connect_one(
    name: &str,
    cfg: &McpServerConfig,
    taken: &mut HashMap<String, (String, String)>,
) -> Result<(RunningService<RoleClient, ()>, Vec<Arc<dyn Tool>>), McpError> {
    let service = match cfg.transport.as_deref() {
        Some("http") | Some("streamable-http") => connect_http(cfg).await?,
        _ => connect_stdio(cfg).await?,
    };

    let mcp_tools = tokio::time::timeout(connect_timeout(), service.list_all_tools())
        .await
        .map_err(|_| McpError::Connect("list_tools timed out".into()))?
        .map_err(|e| McpError::Connect(format!("list_tools: {e}")))?;
    let peer = service.peer().clone();
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for t in mcp_tools {
        let Some(unique) = claim_namespaced_name(taken, name, &t.name) else {
            continue;
        };
        tools.push(Arc::new(McpTool::with_name(name, t, peer.clone(), unique)) as Arc<dyn Tool>);
    }
    Ok((service, tools))
}

/// A bounded tail of a child's piped stderr, drained by a background task so a
/// chatty server can neither corrupt the TUI viewport (inherit) nor block on a
/// full pipe — and the last lines are available as connect error context.
struct StderrTail {
    buf: Arc<tokio::sync::Mutex<Vec<u8>>>,
    eof: tokio::sync::watch::Receiver<bool>,
}

impl StderrTail {
    fn drain(stderr: Option<tokio::process::ChildStderr>) -> Self {
        let buf = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (eof_tx, eof) = tokio::sync::watch::channel(stderr.is_none());
        if let Some(mut stderr) = stderr {
            let sink = buf.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 1024];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut tail = sink.lock().await;
                            tail.extend_from_slice(&chunk[..n]);
                            let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                            tail.drain(..excess);
                        }
                    }
                }
                let _ = eof_tx.send(true);
            });
        }
        StderrTail { buf, eof }
    }

    async fn collect(&self) -> String {
        let mut eof = self.eof.clone();
        let _ = tokio::time::timeout(
            Duration::from_millis(STDERR_EOF_GRACE_MS),
            eof.wait_for(|done| *done),
        )
        .await;
        let tail = self.buf.lock().await;
        String::from_utf8_lossy(&tail).trim().to_string()
    }
}

async fn with_stderr_context(base: String, tail: &StderrTail) -> McpError {
    let stderr = tail.collect().await;
    if stderr.is_empty() {
        McpError::Connect(base)
    } else {
        McpError::Connect(format!("{base}; server stderr: {stderr}"))
    }
}

async fn connect_stdio(
    cfg: &McpServerConfig,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    let command = cfg
        .command
        .clone()
        .ok_or_else(|| McpError::Config("stdio server needs a `command`".into()))?;
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(&cfg.args);
    for (key, value) in &cfg.env {
        cmd.env(key, value);
    }
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| McpError::Connect(e.to_string()))?;
    let tail = StderrTail::drain(stderr);
    match tokio::time::timeout(connect_timeout(), ().serve(transport)).await {
        Err(_) => Err(with_stderr_context("handshake timed out".into(), &tail).await),
        Ok(Err(e)) => Err(with_stderr_context(e.to_string(), &tail).await),
        Ok(Ok(service)) => Ok(service),
    }
}

async fn connect_http(
    cfg: &McpServerConfig,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    let url = cfg
        .url
        .clone()
        .ok_or_else(|| McpError::Config("http server needs a `url`".into()))?;
    let (auth_header, custom_headers) = split_headers(&cfg.headers);
    let mut config =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url);
    config.auth_header = auth_header;
    config.custom_headers = custom_headers;
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
    tokio::time::timeout(connect_timeout(), ().serve(transport))
        .await
        .map_err(|_| McpError::Connect("handshake timed out".into()))?
        .map_err(|e| McpError::Connect(e.to_string()))
}

/// Split configured headers into rmcp's `auth_header` (the `Authorization`
/// value, which rmcp sends specially) and `custom_headers` (everything else).
/// An invalid name/value is logged and skipped so one bad entry never drops the
/// whole connection.
fn split_headers(
    headers: &BTreeMap<String, String>,
) -> (Option<String>, HashMap<HeaderName, HeaderValue>) {
    let mut auth = None;
    let mut custom = HashMap::new();
    for (key, value) in headers {
        if key.eq_ignore_ascii_case("authorization") {
            match HeaderValue::from_str(value) {
                Ok(_) => auth = Some(value.clone()),
                Err(_) => eprintln!("mcp: skipping invalid http header '{key}'"),
            }
            continue;
        }
        match (
            key.parse::<HeaderName>(),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(val)) => {
                custom.insert(name, val);
            }
            _ => eprintln!("mcp: skipping invalid http header '{key}'"),
        }
    }
    (auth, custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_headers_routes_authorization_and_keeps_custom() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer secret".into());
        headers.insert("X-Tenant".into(), "acme".into());
        let (auth, custom) = split_headers(&headers);
        assert_eq!(auth.as_deref(), Some("Bearer secret"));
        assert_eq!(
            custom.get(&HeaderName::from_static("x-tenant")).map(|v| v.to_str().unwrap()),
            Some("acme")
        );
        assert!(!custom.contains_key(&HeaderName::from_static("authorization")));
    }

    #[test]
    fn split_headers_skips_invalid_entries_without_dropping_the_rest() {
        let mut headers = BTreeMap::new();
        headers.insert("Bad Header Name".into(), "x".into());
        headers.insert("X-Ok".into(), "fine".into());
        let (auth, custom) = split_headers(&headers);
        assert!(auth.is_none());
        assert_eq!(custom.len(), 1, "the invalid header is skipped, the good one kept");
        assert!(custom.contains_key(&HeaderName::from_static("x-ok")));
    }

    #[test]
    fn no_headers_yields_no_auth_and_empty_custom() {
        let (auth, custom) = split_headers(&BTreeMap::new());
        assert!(auth.is_none());
        assert!(custom.is_empty());
    }

    #[test]
    fn split_headers_rejects_a_malformed_authorization_value() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".into(),
            "Bearer ok\r\nX-Injected: evil".into(),
        );
        headers.insert("X-Ok".into(), "fine".into());
        let (auth, custom) = split_headers(&headers);
        assert!(auth.is_none(), "a CRLF-bearing Authorization must be skipped");
        assert!(custom.contains_key(&HeaderName::from_static("x-ok")));
    }

    fn sh_server(script: &str) -> McpServerConfig {
        McpServerConfig {
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), script.to_string()],
            ..McpServerConfig::default()
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_failure_surfaces_the_child_stderr_tail() {
        let err = connect_stdio(&sh_server("echo deadbeef-stderr-context >&2; exit 7"))
            .await
            .expect_err("a child that exits without speaking MCP must fail to connect");
        let message = err.to_string();
        assert!(
            message.contains("server stderr: ") && message.contains("deadbeef-stderr-context"),
            "connect error must carry the piped stderr tail, got: {message}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chatty_stderr_is_drained_into_a_bounded_tail() {
        let script = "head -c 200000 /dev/zero | tr '\\0' x >&2; exit 1";
        let err = connect_stdio(&sh_server(script))
            .await
            .expect_err("the child exits, so the handshake must fail");
        let message = err.to_string();
        assert!(
            message.len() < STDERR_TAIL_BYTES + 200,
            "200kB of stderr must be capped to the tail, got {} bytes",
            message.len()
        );
        assert!(
            message.contains("server stderr: ") && message.ends_with('x'),
            "the bounded tail keeps the most recent stderr, got: {message}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quiet_connect_failure_has_no_stderr_context() {
        let err = connect_stdio(&sh_server("exit 3"))
            .await
            .expect_err("a silent immediate exit must fail to connect");
        assert!(
            !err.to_string().contains("server stderr"),
            "no stderr output must not fabricate context, got: {err}"
        );
    }
}
