//! Routes edited files to language servers and returns their diagnostics. Servers
//! are started lazily on first matching edit and reused for the session.

use crate::catalog::ServerSpec;
use crate::client::LspClient;
use crate::diagnostic::report;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

const WAIT: Duration = Duration::from_secs(5);

/// Session-scoped pool of language-server connections keyed by server id.
pub struct LspManager {
    root: PathBuf,
    servers: Vec<ServerSpec>,
    /// `id → client` (a cached `None` marks a server that failed to start, so we
    /// don't keep retrying it every edit).
    clients: AsyncMutex<HashMap<String, Option<Arc<LspClient>>>>,
}

impl LspManager {
    pub fn new(root: PathBuf, servers: Vec<ServerSpec>) -> Self {
        LspManager {
            root,
            servers,
            clients: AsyncMutex::new(HashMap::new()),
        }
    }

    /// No configured/available servers — diagnostics are disabled.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    async fn ensure(&self, spec: &ServerSpec) -> Option<Arc<LspClient>> {
        let mut clients = self.clients.lock().await;
        match clients.get(&spec.id) {
            // A live cached client is reused; a dead one (server crashed mid
            // session) is evicted below and respawned, so diagnostics don't
            // silently vanish for the rest of the session.
            Some(Some(client)) if client.is_alive() => return Some(client.clone()),
            // A cached `None` marks a server that already failed to start —
            // don't hammer it every edit.
            Some(None) => return None,
            _ => {}
        }
        let client = LspClient::spawn(
            spec.id.clone(),
            &spec.command,
            &spec.env,
            &self.root,
            spec.initialization.clone(),
        )
        .await
        .ok()
        .map(Arc::new);
        clients.insert(spec.id.clone(), client.clone());
        client
    }

    /// Drop a cached client (used when its process turns out to be dead) so the
    /// next `ensure` respawns it rather than returning the cached `None`.
    async fn evict(&self, id: &str) {
        self.clients.lock().await.remove(id);
    }

    /// Open/sync the edited file with each matching server and return a combined
    /// errors-only diagnostics report (empty when clean or no server matches).
    /// `path` should be absolute.
    pub async fn diagnostics_after_edit(&self, path: &Path) -> String {
        if self.servers.is_empty() {
            return String::new();
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return String::new();
        };
        let matching: Vec<ServerSpec> = self
            .servers
            .iter()
            .filter(|s| s.handles(name))
            .cloned()
            .collect();
        if matching.is_empty() {
            return String::new();
        }
        let Ok(text) = tokio::fs::read_to_string(path).await else {
            return String::new();
        };
        let rel = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let mut blocks = Vec::new();
        for spec in &matching {
            let Some(client) = self.ensure(spec).await else {
                continue;
            };
            let after = Instant::now();
            let client = if client.open(path, &text).await.is_err() {
                // A write to a crashed server's stdin fails; evict it and try one
                // fresh spawn so this edit still gets diagnostics rather than a
                // false "clean" from a dead server.
                self.evict(&spec.id).await;
                match self.ensure(spec).await {
                    Some(fresh) if fresh.open(path, &text).await.is_ok() => fresh,
                    _ => continue,
                }
            } else {
                client
            };
            let diags = client.wait_diagnostics(path, after, WAIT).await;
            let block = report(&rel, &diags);
            if !block.is_empty() {
                blocks.push(block);
            }
        }
        blocks.join("\n\n")
    }

    /// Gracefully stop every started server (best effort).
    pub async fn shutdown(&self) {
        let clients = self.clients.lock().await;
        for client in clients.values().flatten() {
            client.shutdown().await;
        }
    }
}
