use crate::codex::oauth::{self, decode_id_token};
use crate::codex::{auth_http_timeout, CLIENT_ID, REFRESH_WINDOW_SECS, TOKEN_URL};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use stepper_provider::ProviderError;
use tokio::sync::Mutex;

/// The live ChatGPT-OAuth credentials. Tokens are `secrecy`-wrapped so they are
/// never accidentally logged; `Debug` is redacted.
pub struct CodexCredentials {
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    pub id_token: SecretString,
    pub account_id: String,
    pub expires_at: u64,
    pub last_refresh: u64,
}

impl fmt::Debug for CodexCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexCredentials")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("id_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("expires_at", &self.expires_at)
            .field("last_refresh", &self.last_refresh)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredJson {
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: String,
    expires_at: u64,
    last_refresh: u64,
}

impl CodexCredentials {
    fn to_stored(&self) -> StoredJson {
        StoredJson {
            access_token: self.access_token.expose_secret().to_string(),
            refresh_token: self.refresh_token.expose_secret().to_string(),
            id_token: self.id_token.expose_secret().to_string(),
            account_id: self.account_id.clone(),
            expires_at: self.expires_at,
            last_refresh: self.last_refresh,
        }
    }

    fn from_stored(s: StoredJson) -> Self {
        CodexCredentials {
            access_token: SecretString::from(s.access_token),
            refresh_token: SecretString::from(s.refresh_token),
            id_token: SecretString::from(s.id_token),
            account_id: s.account_id,
            expires_at: s.expires_at,
            last_refresh: s.last_refresh,
        }
    }
}

/// File-backed, auto-refreshing token store shared by clones (the same `Arc`).
/// The on-disk file is written `0600`.
#[derive(Clone)]
pub struct CodexTokenStore {
    inner: Arc<Mutex<CodexCredentials>>,
    path: Arc<PathBuf>,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

impl CodexTokenStore {
    /// `~/.stepper/codex-auth.json`.
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        home.join(".stepper").join("codex-auth.json")
    }

    pub fn load(path: impl Into<PathBuf>, client: reqwest::Client) -> Result<Self, ProviderError> {
        let path = path.into();
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| ProviderError::Auth(format!("no codex credentials at {path:?}: {e}")))?;
        let stored: StoredJson = serde_json::from_str(&raw)
            .map_err(|e| ProviderError::Auth(format!("corrupt codex credentials: {e}")))?;
        Ok(CodexTokenStore {
            inner: Arc::new(Mutex::new(CodexCredentials::from_stored(stored))),
            path: Arc::new(path),
            client,
        })
    }

    /// Construct from freshly-obtained credentials (used by the login flow) and
    /// persist them.
    pub fn create(
        path: impl Into<PathBuf>,
        client: reqwest::Client,
        creds: CodexCredentials,
    ) -> Result<Self, ProviderError> {
        let path = path.into();
        persist(&path, &creds)?;
        Ok(CodexTokenStore {
            inner: Arc::new(Mutex::new(creds)),
            path: Arc::new(path),
            client,
        })
    }

    /// Return a fresh `(access_token, account_id)`, refreshing proactively if the
    /// token expires within the 5-minute window.
    pub async fn bearer(&self) -> Result<(String, String), ProviderError> {
        let mut cred = self.inner.lock().await;
        if now_unix() + REFRESH_WINDOW_SECS >= cred.expires_at {
            self.refresh_locked(&mut cred).await?;
        }
        Ok((
            cred.access_token.expose_secret().to_string(),
            cred.account_id.clone(),
        ))
    }

    /// Force a refresh regardless of expiry (used on a reactive 401).
    pub async fn force_refresh(&self) -> Result<(), ProviderError> {
        let mut cred = self.inner.lock().await;
        self.refresh_locked(&mut cred).await
    }

    pub async fn account_id(&self) -> String {
        self.inner.lock().await.account_id.clone()
    }

    async fn refresh_locked(&self, cred: &mut CodexCredentials) -> Result<(), ProviderError> {
        let body = serde_json::json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": cred.refresh_token.expose_secret(),
        });
        let deadline = auth_http_timeout();
        // Belt over the client's own timeout: the injected client may not carry one.
        let refresh = async {
            let resp = self
                .client
                .post(TOKEN_URL)
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Auth(format!("codex token refresh failed: {e}")))?;

            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                return Err(ProviderError::Auth(format!(
                    "codex token refresh rejected (status {status}) — re-run `stepper auth login --codex`"
                )));
            }

            resp.json::<RefreshResponse>()
                .await
                .map_err(|e| ProviderError::Auth(format!("bad refresh response: {e}")))
        };
        let refreshed = tokio::time::timeout(deadline, refresh)
            .await
            .map_err(|_| {
                ProviderError::Auth(format!(
                    "codex token refresh timed out after {}ms",
                    deadline.as_millis()
                ))
            })??;

        if refreshed.access_token.trim().is_empty() {
            return Err(ProviderError::Auth(
                "refresh response had an empty access_token".into(),
            ));
        }

        // refresh_token rotates — carry the new one (or keep the old if omitted).
        let refresh_token = refreshed
            .refresh_token
            .unwrap_or_else(|| cred.refresh_token.expose_secret().to_string());

        // Prefer a fresh id_token; if it is present but malformed, keep the old
        // one rather than persisting garbage, and fall back to the access
        // token's exp. A *valid* id_token whose account differs from ours is a
        // possible token substitution — refuse it.
        let (id_token, account_id, expires_at) = match refreshed.id_token {
            Some(token) => match decode_id_token(&token) {
                Ok(claims) => {
                    if claims.account_id != cred.account_id {
                        return Err(ProviderError::Auth(
                            "account_id changed after refresh — possible token substitution; \
                             re-run `stepper auth login --codex`"
                                .into(),
                        ));
                    }
                    (token, claims.account_id, claims.exp)
                }
                Err(_) => (
                    cred.id_token.expose_secret().to_string(),
                    cred.account_id.clone(),
                    exp_from_access(&refreshed.access_token),
                ),
            },
            None => (
                cred.id_token.expose_secret().to_string(),
                cred.account_id.clone(),
                exp_from_access(&refreshed.access_token),
            ),
        };

        let next = CodexCredentials {
            access_token: SecretString::from(refreshed.access_token),
            refresh_token: SecretString::from(refresh_token),
            id_token: SecretString::from(id_token),
            account_id,
            expires_at,
            last_refresh: now_unix(),
        };

        // Persist BEFORE committing in-memory so disk never lags the rotated
        // refresh_token (a lost rotation => `refresh_token_reused` + re-login).
        persist(&self.path, &next)?;
        *cred = next;
        Ok(())
    }
}

fn exp_from_access(access_token: &str) -> u64 {
    oauth::jwt_exp(access_token).unwrap_or_else(|_| now_unix() + 3600)
}

/// Write the credential file with owner-only permissions from the very first
/// byte: a 0600 temp file in the same directory, then an atomic rename — there
/// is never a window where the tokens are group/world-readable or partially
/// written. Created parent directories are 0700.
pub(crate) fn persist(path: &Path, cred: &CodexCredentials) -> Result<(), ProviderError> {
    use std::io::Write;

    let json = serde_json::to_string_pretty(&cred.to_stored())
        .map_err(|e| ProviderError::Auth(format!("serialize credentials: {e}")))?;
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
            .map_err(|e| ProviderError::Auth(format!("create {parent:?}: {e}")))?;
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
        .map_err(|e| ProviderError::Auth(format!("create {tmp:?}: {e}")))?;
    file.write_all(json.as_bytes())
        .map_err(|e| ProviderError::Auth(format!("write {tmp:?}: {e}")))?;
    drop(file);

    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ProviderError::Auth(format!("rename {tmp:?} -> {path:?}: {e}"))
    })?;
    Ok(())
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
