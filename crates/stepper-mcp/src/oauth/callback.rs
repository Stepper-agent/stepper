use crate::error::McpError;
use std::time::{Duration, Instant};
use url::Url;

/// Default + fallback local redirect ports. Not OpenAI's Codex-pinned 1455/1457
/// (those are allow-listed by OpenAI and don't apply to arbitrary MCP servers).
pub const DEFAULT_PORT: u16 = 33418;
const FALLBACK_PORT: u16 = 33419;
/// The redirect path the local listener answers on.
pub const CALLBACK_PATH: &str = "/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// A bound localhost redirect listener plus the `http://127.0.0.1:<port>/callback`
/// URI to register as the OAuth redirect.
pub struct CallbackServer {
    server: tiny_http::Server,
    pub redirect_uri: String,
}

/// Bind the redirect listener on `preferred` (then `+1`), or any free port if
/// neither is available, so a stale process can't block sign-in.
pub fn bind(preferred: Option<u16>) -> Result<CallbackServer, McpError> {
    let first = preferred.unwrap_or(DEFAULT_PORT);
    let candidates = [first, first.wrapping_add(1), FALLBACK_PORT, 0];
    for port in candidates {
        if let Ok(server) = tiny_http::Server::http(("127.0.0.1", port)) {
            let bound = server
                .server_addr()
                .to_ip()
                .map(|a| a.port())
                .unwrap_or(port);
            return Ok(CallbackServer {
                server,
                redirect_uri: format!("http://127.0.0.1:{bound}{CALLBACK_PATH}"),
            });
        }
    }
    Err(McpError::Auth(
        "could not bind an OAuth callback listener on 127.0.0.1".into(),
    ))
}

/// Bind the listener to match an explicit `redirectUri` override. The IdP will
/// redirect the browser to exactly this URI, so the host must be loopback, the
/// path must be the one this listener answers (`/callback`), the port must be
/// explicit, and the bind must hit THAT port (no fallback) — anything else means
/// the browser lands where nothing is listening and the flow dead-hangs.
pub fn bind_redirect(redirect_uri: &str) -> Result<CallbackServer, McpError> {
    let parsed = Url::parse(redirect_uri)
        .map_err(|e| McpError::Auth(format!("invalid redirectUri '{redirect_uri}': {e}")))?;
    if !matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost")) {
        return Err(McpError::Auth(format!(
            "redirectUri must be loopback (127.0.0.1 or localhost), got '{redirect_uri}'"
        )));
    }
    if parsed.path() != CALLBACK_PATH {
        return Err(McpError::Auth(format!(
            "redirectUri path must be {CALLBACK_PATH} (the listener only answers there), got '{}'",
            parsed.path()
        )));
    }
    let port = parsed.port().ok_or_else(|| {
        McpError::Auth(format!("redirectUri must include an explicit port, got '{redirect_uri}'"))
    })?;
    let server = tiny_http::Server::http(("127.0.0.1", port)).map_err(|e| {
        McpError::Auth(format!("could not bind the redirectUri port 127.0.0.1:{port}: {e}"))
    })?;
    Ok(CallbackServer { server, redirect_uri: redirect_uri.to_string() })
}

/// Block (off the async runtime) until the browser hits the redirect, returning
/// the `(code, state)` query pair. Times out after 5 minutes.
pub async fn wait(server: CallbackServer) -> Result<(String, String), McpError> {
    let CallbackServer { server, .. } = server;
    let join = tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
        let deadline = Instant::now() + CALLBACK_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for the browser callback".into());
            }
            match server.recv_timeout(remaining.min(Duration::from_secs(1))) {
                Ok(Some(req)) => {
                    if !req.url().starts_with(CALLBACK_PATH) {
                        let _ = req.respond(tiny_http::Response::from_string(
                            "stepper mcp auth: waiting for the OAuth callback…",
                        ));
                        continue;
                    }
                    let result = parse_callback(req.url());
                    let body = match &result {
                        Ok(_) => "<html><body style=\"font-family:sans-serif\"><h3>stepper</h3>\
                            <p>MCP sign-in complete. You can close this tab.</p></body></html>",
                        Err(_) => "<html><body style=\"font-family:sans-serif\"><h3>stepper</h3>\
                            <p>MCP sign-in failed. Check the terminal.</p></body></html>",
                    };
                    let header = "Content-Type: text/html"
                        .parse::<tiny_http::Header>()
                        .expect("valid header");
                    let _ = req.respond(tiny_http::Response::from_string(body).with_header(header));
                    return result;
                }
                Ok(None) => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    });
    join.await
        .map_err(|e| McpError::Auth(format!("callback task failed: {e}")))?
        .map_err(McpError::Auth)
}

fn parse_callback(url: &str) -> Result<(String, String), String> {
    let parsed = Url::parse(&format!("http://127.0.0.1{url}")).map_err(|e| e.to_string())?;
    let mut code = None;
    let mut state = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => return Err(format!("authorization error: {v}")),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err("callback missing code or state".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_and_state() {
        let (code, state) = parse_callback("/callback?code=abc&state=xyz").unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn surfaces_the_authorization_error() {
        let err = parse_callback("/callback?error=access_denied").unwrap_err();
        assert!(err.contains("access_denied"));
    }

    #[test]
    fn rejects_a_callback_missing_state() {
        assert!(parse_callback("/callback?code=abc").is_err());
    }

    #[test]
    fn binds_and_reports_a_loopback_redirect_uri() {
        let server = bind(Some(0)).unwrap();
        assert!(server.redirect_uri.starts_with("http://127.0.0.1:"));
        assert!(server.redirect_uri.ends_with("/callback"));
    }

    #[test]
    fn bind_redirect_rejects_non_loopback_wrong_path_and_missing_port() {
        assert!(bind_redirect("https://evil.example.com:443/callback").is_err());
        assert!(bind_redirect("http://127.0.0.1:33418/oauth/cb").is_err());
        assert!(bind_redirect("http://127.0.0.1/callback").is_err(), "needs an explicit port");
        assert!(bind_redirect("not a url").is_err());
    }

    #[test]
    fn bind_redirect_binds_a_valid_loopback_override() {
        // Find a free port, then assert bind_redirect binds exactly it and echoes
        // the override verbatim.
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let uri = format!("http://127.0.0.1:{port}/callback");
        let server = bind_redirect(&uri).unwrap();
        assert_eq!(server.redirect_uri, uri);
    }
}
