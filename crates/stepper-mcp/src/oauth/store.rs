use crate::error::McpError;
use rmcp::transport::auth::{AuthError, CredentialStore, StoredCredentials};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Serializes every read-modify-write of the shared `mcp-auth.json` within this
/// process, so a token refresh on one server can't clobber another's entry. (A
/// second stepper process racing the same file is an accepted v1 limitation.)
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// The default token store path: `~/.stepper/mcp-auth.json` (0600), mirroring the
/// Codex token file. Returns `None` when `HOME` is unset — secrets must never be
/// written to a CWD-relative path (where they could be git-committed or synced).
pub fn default_store_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".stepper").join("mcp-auth.json"))
}

/// A `CredentialStore` backing one MCP server's OAuth tokens in the shared
/// `mcp-auth.json` map (keyed by server name). Persists rmcp's `StoredCredentials`
/// (client registration + token response) to a 0600 file — no OS keyring, so it
/// works uniformly on Linux where the keyring backend is non-persistent.
pub struct FileCredentialStore {
    path: PathBuf,
    server: String,
}

impl FileCredentialStore {
    pub fn new(path: impl Into<PathBuf>, server: impl Into<String>) -> Self {
        FileCredentialStore { path: path.into(), server: server.into() }
    }

    /// Whether this server has stored credentials (a CLI `status` probe).
    pub fn has_credentials(&self) -> bool {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        read_map(&self.path).contains_key(&self.server)
    }
}

fn read_map(path: &Path) -> BTreeMap<String, StoredCredentials> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    }
}

fn write_map(path: &Path, map: &BTreeMap<String, StoredCredentials>) -> Result<(), McpError> {
    use std::io::Write;
    let json = serde_json::to_string_pretty(map)
        .map_err(|e| McpError::Auth(format!("serialize mcp credentials: {e}")))?;
    if let Some(parent) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .map_err(|e| McpError::Auth(format!("create {parent:?}: {e}")))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| McpError::Auth(format!("create {tmp:?}: {e}")))?;
    file.write_all(json.as_bytes())
        .map_err(|e| McpError::Auth(format!("write {tmp:?}: {e}")))?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        McpError::Auth(format!("rename {tmp:?} -> {path:?}: {e}"))
    })
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Ok(read_map(&self.path).get(&self.server).cloned())
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = read_map(&self.path);
        map.insert(self.server.clone(), credentials);
        write_map(&self.path, &map).map_err(|e| AuthError::InternalError(e.to_string()))
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = read_map(&self.path);
        if map.remove(&self.server).is_none() {
            return Ok(());
        }
        if map.is_empty() {
            let _ = std::fs::remove_file(&self.path);
            return Ok(());
        }
        write_map(&self.path, &map).map_err(|e| AuthError::InternalError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::auth::StoredCredentials;

    fn creds(client_id: &str) -> StoredCredentials {
        StoredCredentials::new(client_id.to_string(), None, vec!["read".into()], Some(1))
    }

    #[tokio::test]
    async fn save_load_clear_round_trip_per_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        let a = FileCredentialStore::new(&path, "alpha");
        let b = FileCredentialStore::new(&path, "beta");

        assert!(a.load().await.unwrap().is_none());
        a.save(creds("client-a")).await.unwrap();
        b.save(creds("client-b")).await.unwrap();
        assert_eq!(a.load().await.unwrap().unwrap().client_id, "client-a");
        assert_eq!(b.load().await.unwrap().unwrap().client_id, "client-b");
        assert!(a.has_credentials() && b.has_credentials());

        // Clearing one server leaves the other intact.
        a.clear().await.unwrap();
        assert!(a.load().await.unwrap().is_none());
        assert_eq!(b.load().await.unwrap().unwrap().client_id, "client-b");

        // Clearing the last server removes the file.
        b.clear().await.unwrap();
        assert!(!path.exists(), "an empty store deletes the file");
        // Clear on empty is a no-op.
        a.clear().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_store_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("mcp-auth.json");
        FileCredentialStore::new(&path, "s").save(creds("c")).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be private");
    }
}
