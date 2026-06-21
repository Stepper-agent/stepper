use crate::bridge::claim_namespaced_name;
use crate::error::McpError;
use crate::tool::McpTool;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use stepper_config::{McpServerConfig, ProxyConfig};
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

/// The connect/handshake timeout for one server: its per-server `timeout` (ms)
/// if set, else the global default.
fn server_timeout(cfg: &McpServerConfig) -> Duration {
    cfg.timeout.map(Duration::from_millis).unwrap_or_else(connect_timeout)
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
    pub async fn connect(
        servers: &BTreeMap<String, McpServerConfig>,
        base_dir: &Path,
        proxy: Option<&ProxyConfig>,
    ) -> Self {
        let mut manager = McpManager::empty();
        let mut taken = HashMap::new();

        // Phase 1: connect + list tools for every enabled server CONCURRENTLY, so a
        // slow/hung server no longer serializes the others' startup (each stays
        // bounded by its own per-server timeout).
        type ConnectResult =
            Result<(RunningService<RoleClient, ()>, Peer<RoleClient>, Vec<rmcp::model::Tool>), McpError>;
        let mut set: tokio::task::JoinSet<(String, ConnectResult)> = tokio::task::JoinSet::new();
        for (name, cfg) in servers {
            // A server disabled in config stays defined but isn't connected.
            if cfg.enabled == Some(false) {
                continue;
            }
            let (name, cfg) = (name.clone(), cfg.clone());
            let base_dir = base_dir.to_path_buf();
            let proxy = proxy.cloned();
            set.spawn(async move {
                let r = connect_and_list(&name, &cfg, &base_dir, proxy.as_ref()).await;
                (name, r)
            });
        }
        let mut results: HashMap<String, ConnectResult> = HashMap::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((name, r)) = joined {
                results.insert(name, r);
            }
        }

        // Phase 2: namespace + register in deterministic (BTreeMap) order, so the
        // tool-name collision resolution (`taken`) is identical regardless of which
        // server happened to connect first.
        for name in servers.keys() {
            match results.remove(name) {
                Some(Ok((service, peer, mcp_tools))) => {
                    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
                    for t in mcp_tools {
                        let Some(unique) = claim_namespaced_name(&mut taken, name, &t.name) else {
                            continue;
                        };
                        tools.push(
                            Arc::new(McpTool::with_name(name, t, peer.clone(), unique)) as Arc<dyn Tool>,
                        );
                    }
                    for tool in &tools {
                        manager.origins.push((tool.name().to_string(), name.clone()));
                    }
                    manager.services.push(service);
                    manager.tools.extend(tools);
                }
                Some(Err(e)) => eprintln!("mcp: server '{name}' unavailable: {e}"),
                None => {} // disabled (never spawned) or the task was dropped
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

    /// Resources advertised by every connected server, as `uri  name` lines.
    /// Best-effort: a server that errors, paginates oddly, or advertises none
    /// simply contributes nothing (the MCP resources capability is optional).
    pub async fn list_all_resources(&self) -> Vec<String> {
        let mut out = Vec::new();
        for svc in &self.services {
            // Bound each call: a server that connected but won't answer this
            // (optional) request must not hang `stepper mcp get` indefinitely.
            if let Ok(Ok(resources)) =
                tokio::time::timeout(connect_timeout(), svc.list_all_resources()).await
            {
                for r in resources {
                    out.push(format!("{}  {}", r.uri, r.name));
                }
            }
        }
        out
    }

    /// Prompts advertised by every connected server, as `name — description`
    /// lines (description omitted when absent). Best-effort, like resources.
    pub async fn list_all_prompts(&self) -> Vec<String> {
        let mut out = Vec::new();
        for svc in &self.services {
            // Bound each call (see `list_all_resources`): a connected-but-silent
            // server must not freeze `stepper mcp get`.
            if let Ok(Ok(prompts)) =
                tokio::time::timeout(connect_timeout(), svc.list_all_prompts()).await
            {
                for p in prompts {
                    match p.description {
                        Some(d) if !d.is_empty() => out.push(format!("{} — {d}", p.name)),
                        _ => out.push(p.name),
                    }
                }
            }
        }
        out
    }

    pub async fn shutdown(self) {
        for service in self.services {
            let _ = service.cancel().await;
        }
    }
}

/// Connect one server and list its raw tools — WITHOUT namespacing (which needs
/// the shared `taken` map and so must stay sequential in `connect`'s phase 2).
/// Split out so the slow, independent connect+list work runs concurrently.
async fn connect_and_list(
    name: &str,
    cfg: &McpServerConfig,
    base_dir: &Path,
    proxy: Option<&ProxyConfig>,
) -> Result<(RunningService<RoleClient, ()>, Peer<RoleClient>, Vec<rmcp::model::Tool>), McpError> {
    let timeout = server_timeout(cfg);
    let service = match cfg.transport.as_deref() {
        Some("http") | Some("streamable-http") => connect_http(name, cfg, timeout, proxy).await?,
        _ => connect_stdio(cfg, base_dir, timeout).await?,
    };

    let mcp_tools = tokio::time::timeout(timeout, service.list_all_tools())
        .await
        .map_err(|_| McpError::Connect("list_tools timed out".into()))?
        .map_err(|e| McpError::Connect(format!("list_tools: {e}")))?;
    let peer = service.peer().clone();
    Ok((service, peer, mcp_tools))
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
    base_dir: &Path,
    timeout: Duration,
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
    // Per-server working directory (relative paths resolve against the project root).
    if let Some(cwd) = &cfg.cwd {
        let dir = if Path::new(cwd).is_absolute() {
            PathBuf::from(cwd)
        } else {
            base_dir.join(cwd)
        };
        cmd.current_dir(dir);
    }
    let (transport, stderr) = TokioChildProcess::builder(cmd)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| McpError::Connect(e.to_string()))?;
    let tail = StderrTail::drain(stderr);
    match tokio::time::timeout(timeout, ().serve(transport)).await {
        Err(_) => Err(with_stderr_context("handshake timed out".into(), &tail).await),
        Ok(Err(e)) => Err(with_stderr_context(e.to_string(), &tail).await),
        Ok(Ok(service)) => Ok(service),
    }
}

async fn connect_http(
    name: &str,
    cfg: &McpServerConfig,
    timeout: Duration,
    proxy: Option<&ProxyConfig>,
) -> Result<RunningService<RoleClient, ()>, McpError> {
    let url = cfg
        .url
        .clone()
        .ok_or_else(|| McpError::Config("http server needs a `url`".into()))?;
    let (auth_header, custom_headers) = split_headers(&cfg.headers);
    let mut config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
        url.clone(),
    );
    config.auth_header = auth_header;
    config.custom_headers = custom_headers;
    // Build the http client ourselves (instead of rmcp's default) so a private
    // CA bundle is honored. `pool_max_idle_per_host(0)` replicates rmcp's
    // `default_http_client` tuning (it disables idle-connection pooling to avoid
    // ~40ms TCP Delayed-ACK stalls on Linux) so the swap stays behavior-preserving
    // apart from the added CA.
    let client = apply_proxy(apply_extra_ca(reqwest::Client::builder()), proxy)
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| McpError::Connect(e.to_string()))?;
    // An OAuth-enabled server uses a bearer-injecting/refreshing AuthClient when it
    // has stored creds; with none, fail clearly instead of firing an unauthorized
    // request. Plain servers keep the bare client (existing behavior). The OAuth
    // load does network metadata discovery, so it's bounded by the SAME per-server
    // timeout as the handshake — a hung auth server can't stall startup.
    let served = if crate::oauth::is_oauth_enabled(cfg) {
        let loaded = tokio::time::timeout(timeout, crate::oauth::load_auth_client(name, &url, client))
            .await
            .map_err(|_| McpError::Connect(format!("OAuth init for '{name}' timed out")))??;
        match loaded {
            Some(auth_client) => {
                let transport =
                    rmcp::transport::StreamableHttpClientTransport::with_client(auth_client, config);
                tokio::time::timeout(timeout, ().serve(transport)).await
            }
            None => {
                return Err(McpError::Connect(format!(
                    "server '{name}' needs OAuth — run: stepper mcp auth {name}"
                )))
            }
        }
    } else {
        let transport = rmcp::transport::StreamableHttpClientTransport::with_client(client, config);
        tokio::time::timeout(timeout, ().serve(transport)).await
    };
    served
        .map_err(|_| McpError::Connect("handshake timed out".into()))?
        .map_err(|e| McpError::Connect(e.to_string()))
}

/// Merge a private CA bundle (`STEPPER_EXTRA_CA_CERTS`, falling back to
/// `NODE_EXTRA_CA_CERTS`) into the platform trust store so an http MCP server
/// behind a corporate proxy / self-signed TLS validates. Additive (system roots
/// still apply) and fail-open: unset/unreadable/unparsable leaves the builder
/// untouched. Proxy needs no code: reqwest honors `HTTP(S)_PROXY`/`NO_PROXY`.
fn apply_extra_ca(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let Some(path) =
        std::env::var_os("STEPPER_EXTRA_CA_CERTS").or_else(|| std::env::var_os("NODE_EXTRA_CA_CERTS"))
    else {
        return builder;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!("extra CA certificates: cannot read {path:?}: {err}");
            return builder;
        }
    };
    let certs = match reqwest::Certificate::from_pem_bundle(&bytes) {
        Ok(certs) => certs,
        Err(err) => {
            tracing::warn!("extra CA certificates: cannot parse {path:?}: {err}");
            return builder;
        }
    };
    // rustls-platform-verifier can merge EXTRA roots only on these targets; on any
    // other target a non-empty root set makes `.build()` error, so stay on system
    // trust there — keeping the fail-open total (a valid cert never aborts startup).
    #[cfg(any(all(unix, not(target_os = "android")), target_os = "windows"))]
    {
        builder.tls_certs_merge(certs)
    }
    #[cfg(not(any(all(unix, not(target_os = "android")), target_os = "windows")))]
    {
        let _ = certs;
        builder
    }
}

/// Apply an explicit proxy from config to an http MCP client. `None`/inactive
/// leaves the builder on reqwest's `HTTP(S)_PROXY`/`NO_PROXY` env default; an
/// explicit proxy replaces the env proxy; `disabled: true` forces a direct
/// connection. Fail-open: a malformed proxy URL is logged and skipped.
fn apply_proxy(mut builder: reqwest::ClientBuilder, proxy: Option<&ProxyConfig>) -> reqwest::ClientBuilder {
    let Some(proxy) = proxy.filter(|p| p.is_active()) else {
        return builder;
    };
    if proxy.disabled {
        return builder.no_proxy();
    }
    let no_proxy = || proxy.no_proxy.as_deref().and_then(reqwest::NoProxy::from_string);
    // Scheme-specific proxies before the catch-all `all` — reqwest uses the first
    // matching one, so `all` must come last or it shadows `http`/`https`.
    if let Some(url) = proxy.http.as_deref() {
        match reqwest::Proxy::http(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `http` proxy {url:?}: {e}"),
        }
    }
    if let Some(url) = proxy.https.as_deref() {
        match reqwest::Proxy::https(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `https` proxy {url:?}: {e}"),
        }
    }
    if let Some(url) = proxy.all.as_deref() {
        match reqwest::Proxy::all(url) {
            Ok(p) => builder = builder.proxy(p.no_proxy(no_proxy())),
            Err(e) => tracing::warn!("proxy: ignoring invalid `all` proxy {url:?}: {e}"),
        }
    }
    builder
}

/// Split configured headers into rmcp's `auth_header` (the bearer TOKEN) and
/// `custom_headers` (everything else). An invalid name/value is logged and
/// skipped so one bad entry never drops the whole connection.
fn split_headers(
    headers: &BTreeMap<String, String>,
) -> (Option<String>, HashMap<HeaderName, HeaderValue>) {
    let mut auth = None;
    let mut custom = HashMap::new();
    for (key, value) in headers {
        if key.eq_ignore_ascii_case("authorization") {
            // rmcp applies `auth_header` via reqwest `.bearer_auth()`, which
            // prepends "Bearer ", so a configured `Bearer <token>` value must
            // have its scheme stripped first — otherwise the wire carries
            // "Bearer Bearer <token>" and auth fails. A bare token is passed
            // through (rmcp adds the scheme). The HTTP auth scheme is
            // case-insensitive (RFC 7235), so match any casing of "bearer ".
            let token = match value.split_at_checked(7) {
                Some((head, rest)) if head.eq_ignore_ascii_case("bearer ") => rest,
                _ => value,
            }
            .trim();
            match HeaderValue::from_str(token) {
                Ok(_) => auth = Some(token.to_string()),
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
        // The "Bearer " scheme is stripped — rmcp re-adds it via bearer_auth, so
        // storing the full value would double-prefix the wire header.
        assert_eq!(auth.as_deref(), Some("secret"));
        assert_eq!(
            custom.get(&HeaderName::from_static("x-tenant")).map(|v| v.to_str().unwrap()),
            Some("acme")
        );
        assert!(!custom.contains_key(&HeaderName::from_static("authorization")));
    }

    #[test]
    fn split_headers_keeps_a_bare_token_for_rmcp_to_add_the_scheme() {
        let mut headers = BTreeMap::new();
        headers.insert("authorization".into(), "sk-raw-token".into());
        let (auth, _) = split_headers(&headers);
        assert_eq!(auth.as_deref(), Some("sk-raw-token"));
    }

    #[test]
    fn extra_ca_falls_open_so_http_mcp_still_builds() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("STEPPER_EXTRA_CA_CERTS");
        unsafe { std::env::set_var("STEPPER_EXTRA_CA_CERTS", "/no/such/ca.pem") };
        assert!(
            apply_extra_ca(reqwest::Client::builder()).build().is_ok(),
            "a missing extra-CA path must fail open, not break the http transport"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("STEPPER_EXTRA_CA_CERTS", v),
                None => std::env::remove_var("STEPPER_EXTRA_CA_CERTS"),
            }
        }
    }

    #[test]
    fn apply_proxy_none_disabled_explicit_and_garbage_all_build() {
        let ok = |b: reqwest::ClientBuilder| b.build().is_ok();
        assert!(ok(apply_proxy(reqwest::Client::builder(), None)));
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&ProxyConfig::default()))));
        let disabled = ProxyConfig { disabled: true, ..Default::default() };
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&disabled))));
        let explicit = ProxyConfig { all: Some("http://127.0.0.1:8080".into()), ..Default::default() };
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&explicit))));
        let garbage = ProxyConfig { http: Some("not a url".into()), ..Default::default() };
        assert!(ok(apply_proxy(reqwest::Client::builder(), Some(&garbage))));
    }

    #[test]
    fn split_headers_strips_the_bearer_scheme_case_insensitively() {
        // The HTTP auth scheme is case-insensitive (RFC 7235); every casing must
        // be stripped so rmcp's bearer_auth does not produce a double prefix.
        for raw in ["Bearer tok", "bearer tok", "BEARER tok", "bEaReR tok"] {
            let mut headers = BTreeMap::new();
            headers.insert("Authorization".into(), raw.to_string());
            let (auth, _) = split_headers(&headers);
            assert_eq!(auth.as_deref(), Some("tok"), "stripped scheme from {raw:?}");
        }
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
    async fn oauth_http_server_without_creds_fails_with_an_actionable_message() {
        // OAuth-enabled http server + no stored tokens => connect_http short-circuits
        // BEFORE any network, telling the user to authenticate. The unique name keeps
        // the real ~/.stepper/mcp-auth.json from accidentally satisfying it.
        let cfg = McpServerConfig {
            transport: Some("http".into()),
            url: Some("https://example.invalid/mcp".into()),
            oauth: Some(stepper_config::McpOAuthConfig::default()),
            ..McpServerConfig::default()
        };
        let err = connect_http("stepper-test-oauth-no-creds-zzz", &cfg, connect_timeout(), None)
            .await
            .expect_err("an oauth server with no creds must fail to connect");
        let msg = err.to_string();
        assert!(
            msg.contains("needs OAuth") && msg.contains("stepper mcp auth"),
            "got: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_failure_surfaces_the_child_stderr_tail() {
        let err = connect_stdio(&sh_server("echo deadbeef-stderr-context >&2; exit 7"), Path::new("."), connect_timeout())
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
        let err = connect_stdio(&sh_server(script), Path::new("."), connect_timeout())
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
        let err = connect_stdio(&sh_server("exit 3"), Path::new("."), connect_timeout())
            .await
            .expect_err("a silent immediate exit must fail to connect");
        assert!(
            !err.to_string().contains("server stderr"),
            "no stderr output must not fabricate context, got: {err}"
        );
    }

    #[test]
    fn server_timeout_prefers_per_server_over_global() {
        let mut cfg = McpServerConfig::default();
        assert_eq!(server_timeout(&cfg), connect_timeout(), "no per-server → global default");
        cfg.timeout = Some(2500);
        assert_eq!(server_timeout(&cfg), Duration::from_millis(2500));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_server_is_skipped_and_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let mut cfg = sh_server(&format!("touch '{}'; exit 0", marker.display()));
        cfg.enabled = Some(false);
        let servers = BTreeMap::from([("off".to_string(), cfg)]);
        let mgr = McpManager::connect(&servers, Path::new("."), None).await;
        assert!(mgr.is_empty(), "a disabled server is not connected");
        assert!(!marker.exists(), "a disabled server's command must not run");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stdio_server_runs_in_its_configured_cwd() {
        let dir = tempfile::tempdir().unwrap();
        // `touch ran-here` (a relative path) lands in the server's cwd. The MCP
        // handshake still fails (sh isn't an MCP server), but the command ran.
        let mut cfg = sh_server("touch ran-here; exit 0");
        cfg.cwd = Some(dir.path().to_string_lossy().into_owned());
        let servers = BTreeMap::from([("p".to_string(), cfg)]);
        let _ = McpManager::connect(&servers, Path::new("."), None).await;
        assert!(dir.path().join("ran-here").exists(), "the stdio server ran in its cwd");
    }
}
