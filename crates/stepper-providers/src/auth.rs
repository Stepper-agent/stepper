use crate::codex::CodexTokenStore;
use secrecy::SecretString;

/// Keyring service name under which provider API keys are stored
/// (`Entry::new("stepper", "<provider>")`).
const KEYRING_SERVICE: &str = "stepper";

/// How an adapter authenticates each request.
#[derive(Clone)]
pub enum AuthSource {
    /// No credentials — localhost oMLX / mlx servers.
    None,
    /// A bearer / `x-api-key` secret.
    ApiKey(SecretString),
    /// ChatGPT OAuth: the adapter calls `bearer()` per request to get a
    /// freshly-refreshed `(access_token, account_id)`.
    Codex(CodexTokenStore),
}

/// Resolve an API key with the precedence `explicit cfg > env > OS keyring`. The
/// env var is `STEPPER_<PROVIDER>_API_KEY` (provider uppercased, `-`→`_`); the
/// keyring entry is `Entry::new("stepper", "<provider>")`. Returns `None` when no
/// key is found (caller decides if that is fatal).
pub fn resolve_key(provider: &str, explicit: Option<&str>) -> Option<SecretString> {
    resolve_key_with(provider, explicit, key_from_keyring)
}

/// The precedence logic, with the keyring tier injected so it can be tested
/// without touching the real OS keystore.
fn resolve_key_with(
    provider: &str,
    explicit: Option<&str>,
    keyring: impl Fn(&str) -> Option<SecretString>,
) -> Option<SecretString> {
    if let Some(k) = explicit {
        let k = k.trim();
        if k == "none" {
            return None;
        }
        if !k.is_empty() {
            return Some(SecretString::from(k.to_string()));
        }
    }

    let env_var = format!(
        "STEPPER_{}_API_KEY",
        provider.to_ascii_uppercase().replace('-', "_")
    );
    if let Ok(v) = std::env::var(&env_var) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(SecretString::from(v.to_string()));
        }
    }

    keyring(provider)
}

/// Read a provider's API key from the OS keyring (Keychain / secret-service /
/// Credential Manager). Any error (no entry, locked, unsupported) yields `None`
/// so it is a soft fallback.
pub fn key_from_keyring(provider: &str) -> Option<SecretString> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, provider).ok()?;
    let secret = entry.get_password().ok()?;
    let secret = secret.trim();
    if secret.is_empty() {
        None
    } else {
        Some(SecretString::from(secret.to_string()))
    }
}

/// Store a provider's API key in the OS keyring (used by `stepper auth set-key`).
pub fn store_key_in_keyring(provider: &str, key: &str) -> keyring::Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, provider)?.set_password(key)
}

/// Remove a provider's API key from the OS keyring (`stepper auth delete-key`).
pub fn delete_key_from_keyring(provider: &str) -> keyring::Result<()> {
    keyring::Entry::new(KEYRING_SERVICE, provider)?.delete_credential()
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn from_keyring(_p: &str) -> Option<SecretString> {
        Some(SecretString::from("sk-keyring".to_string()))
    }

    #[test]
    fn explicit_key_beats_everything() {
        let k = resolve_key_with("acme", Some("sk-explicit"), from_keyring).unwrap();
        assert_eq!(k.expose_secret(), "sk-explicit");
    }

    #[test]
    fn explicit_none_short_circuits_to_no_key() {
        assert!(resolve_key_with("acme", Some("none"), from_keyring).is_none());
    }

    #[test]
    fn env_beats_keyring() {
        let var = "STEPPER_KRENVTEST_API_KEY";
        // SAFETY: a unique env var set/removed within this single test.
        unsafe { std::env::set_var(var, "sk-env") };
        let k = resolve_key_with("krenvtest", None, from_keyring).unwrap();
        unsafe { std::env::remove_var(var) };
        assert_eq!(k.expose_secret(), "sk-env");
    }

    #[test]
    fn keyring_is_the_last_fallback() {
        // A provider with no explicit key and no env var falls through to keyring.
        let k = resolve_key_with("kr-fallback-unset-provider", None, from_keyring).unwrap();
        assert_eq!(k.expose_secret(), "sk-keyring");
    }

    #[test]
    fn no_source_yields_none() {
        let k = resolve_key_with("kr-none-unset-provider", None, |_| None);
        assert!(k.is_none());
    }
}
