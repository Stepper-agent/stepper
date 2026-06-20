//! MCP server OAuth (remote/http servers). rmcp's `auth` feature does all the
//! protocol work — RFC 8414 metadata discovery, RFC 9728 protected-resource
//! metadata, RFC 7591 Dynamic Client Registration, PKCE, token exchange, and
//! refresh. stepper supplies the glue: a 0600-file credential store, a localhost
//! redirect listener, and CLI verbs (`stepper mcp auth|logout|status`).

mod callback;
mod store;

use crate::error::McpError;
use rmcp::transport::auth::{AuthClient, AuthorizationManager, OAuthClientConfig};
use stepper_config::{McpOAuthConfig, McpServerConfig};
use store::{default_store_path, FileCredentialStore};

fn auth_err(e: impl std::fmt::Display) -> McpError {
    McpError::Auth(e.to_string())
}

/// Whether a server should use OAuth. The `type` check matches `connect_one`'s
/// transport dispatch EXACTLY (only `http`/`streamable-http` reach `connect_http`),
/// so a server that is authed here is the same one connect/status treat as OAuth —
/// no divergence where a server is authed but never uses its tokens.
pub fn is_oauth_enabled(cfg: &McpServerConfig) -> bool {
    matches!(cfg.transport.as_deref(), Some("http") | Some("streamable-http"))
        && cfg.url.is_some()
        && cfg.oauth.as_ref().is_some_and(|o| !o.disabled)
}

/// Run the interactive browser authorization for one server: discover metadata,
/// register (or configure) the client, open the browser to the authorize URL,
/// catch the redirect on a localhost listener, and exchange the code for tokens
/// (rmcp persists them to the file store). Blocks up to 5 minutes on the browser.
pub async fn authenticate(
    server: &str,
    cfg: &McpOAuthConfig,
    url: &str,
    http_client: reqwest::Client,
) -> Result<(), McpError> {
    let store_path = default_store_path()
        .ok_or_else(|| McpError::Auth("HOME is not set — cannot store MCP OAuth tokens".into()))?;
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| McpError::Auth(format!("init OAuth for '{server}': {e}")))?;
    manager.set_credential_store(FileCredentialStore::new(&store_path, server));
    manager.with_client(http_client).map_err(auth_err)?;
    let metadata = manager
        .discover_metadata()
        .await
        .map_err(|e| McpError::Auth(format!("OAuth discovery failed for '{server}': {e}")))?;
    manager.set_metadata(metadata);

    // Bind the redirect listener BEFORE building the authorize URL (it stays open
    // across the browser round-trip). A `redirectUri` override dictates the exact
    // port the IdP will redirect to, so the listener must bind THAT port (no
    // fallback) — otherwise the browser lands on a dead port and the flow hangs.
    let listener = match &cfg.redirect_uri {
        Some(uri) => callback::bind_redirect(uri)?,
        None => callback::bind(cfg.callback_port)?,
    };
    let redirect_uri = cfg.redirect_uri.clone().unwrap_or_else(|| listener.redirect_uri.clone());
    let scope_refs: Vec<&str> = cfg.scope.iter().map(String::as_str).collect();

    if let Some(client_id) = &cfg.client_id {
        // Pre-registered client (public PKCE, or confidential with a secret).
        let mut client_cfg = OAuthClientConfig::new(client_id.clone(), redirect_uri.clone());
        if let Some(secret) = &cfg.client_secret {
            client_cfg = client_cfg.with_client_secret(secret.clone());
        }
        if !cfg.scope.is_empty() {
            client_cfg = client_cfg.with_scopes(cfg.scope.clone());
        }
        manager.configure_client(client_cfg).map_err(auth_err)?;
    } else {
        // Dynamic Client Registration (RFC 7591).
        manager
            .register_client("stepper", &redirect_uri, &scope_refs)
            .await
            .map_err(|e| McpError::Auth(format!("client registration failed for '{server}': {e}")))?;
    }

    let authorize_url = manager.get_authorization_url(&scope_refs).await.map_err(auth_err)?;
    if open::that(&authorize_url).is_err() {
        println!("Open this URL in a browser to authorize '{server}':\n  {authorize_url}");
    } else {
        println!("Opened your browser to authorize '{server}'. Waiting for the callback…");
    }

    let (code, state) = callback::wait(listener).await?;
    manager
        .exchange_code_for_token(&code, &state)
        .await
        .map_err(|e| McpError::Auth(format!("token exchange failed for '{server}': {e}")))?;
    Ok(())
}

/// Build an `AuthClient` (bearer-injecting + auto-refreshing reqwest wrapper) for
/// a server that already has stored credentials, else `Ok(None)` so the caller
/// connects unauthenticated. The same `http_client` (with any extra CA) is reused.
pub async fn load_auth_client(
    server: &str,
    url: &str,
    http_client: reqwest::Client,
) -> Result<Option<AuthClient<reqwest::Client>>, McpError> {
    let Some(store_path) = default_store_path() else {
        return Ok(None);
    };
    if !FileCredentialStore::new(&store_path, server).has_credentials() {
        return Ok(None);
    }
    let mut manager = AuthorizationManager::new(url)
        .await
        .map_err(|e| McpError::Auth(format!("init OAuth for '{server}': {e}")))?;
    manager.set_credential_store(FileCredentialStore::new(&store_path, server));
    manager.with_client(http_client.clone()).map_err(auth_err)?;
    // Loads the token + reconstructs metadata/client so a later refresh works.
    let ready = manager
        .initialize_from_store()
        .await
        .map_err(|e| McpError::Auth(format!("load OAuth creds for '{server}': {e}")))?;
    if !ready {
        return Ok(None);
    }
    Ok(Some(AuthClient::new(http_client, manager)))
}

/// Drop a server's stored credentials (`stepper mcp logout`). Returns whether
/// anything was removed.
pub async fn logout(server: &str) -> Result<bool, McpError> {
    use rmcp::transport::auth::CredentialStore;
    let Some(store_path) = default_store_path() else {
        return Ok(false);
    };
    let store = FileCredentialStore::new(store_path, server);
    let had = store.has_credentials();
    store.clear().await.map_err(auth_err)?;
    Ok(had)
}

/// One server's OAuth status for `stepper mcp status`.
pub struct McpOAuthStatus {
    pub server: String,
    pub authenticated: bool,
}

/// OAuth status for every OAuth-capable configured server.
pub fn status(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> Vec<McpOAuthStatus> {
    let store_path = default_store_path();
    servers
        .iter()
        .filter(|(_, cfg)| is_oauth_enabled(cfg))
        .map(|(name, _)| McpOAuthStatus {
            server: name.clone(),
            authenticated: store_path
                .as_ref()
                .is_some_and(|p| FileCredentialStore::new(p, name).has_credentials()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_server(url: &str) -> McpServerConfig {
        McpServerConfig {
            transport: Some("http".into()),
            url: Some(url.into()),
            ..McpServerConfig::default()
        }
    }

    #[test]
    fn is_oauth_enabled_gates_on_http_url_and_not_disabled() {
        let mut s = http_server("https://x");
        assert!(!is_oauth_enabled(&s), "no oauth block = off");
        s.oauth = Some(McpOAuthConfig::default());
        assert!(is_oauth_enabled(&s), "oauth present on http = on");
        s.oauth = Some(McpOAuthConfig { disabled: true, ..McpOAuthConfig::default() });
        assert!(!is_oauth_enabled(&s), "disabled = off");
        // A stdio server with an oauth block is still off (no url / has command).
        let mut stdio = McpServerConfig { command: Some("x".into()), ..McpServerConfig::default() };
        stdio.oauth = Some(McpOAuthConfig::default());
        assert!(!is_oauth_enabled(&stdio));
    }

    #[test]
    fn status_lists_only_oauth_capable_servers() {
        let mut servers = std::collections::BTreeMap::new();
        servers.insert("plain".to_string(), http_server("https://a"));
        let mut oauthed = http_server("https://b");
        oauthed.oauth = Some(McpOAuthConfig::default());
        servers.insert("oauthed".to_string(), oauthed);
        let st = status(&servers);
        // Only the oauth-capable server is listed (the authenticated flag reads the
        // real token file, so it is not asserted here — see store.rs round-trip).
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].server, "oauthed");
    }
}
