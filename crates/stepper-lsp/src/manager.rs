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
        if let Some(slot) = clients.get(&spec.id) {
            return slot.clone();
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
            if client.open(path, &text).await.is_err() {
                continue;
            }
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
