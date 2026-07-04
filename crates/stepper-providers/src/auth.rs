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

/// Resolve an API key with the precedence `explicit cfg > STEPPER_<P>_API_KEY >
/// well-known provider env (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …) > OS
/// keyring`. The well-known tier lets someone who already exported the standard
/// vendor variable (e.g. migrating from Claude Code) run keyless. Returns `None`
/// when no key is found (caller decides if that is fatal).
pub fn resolve_key(provider: &str, explicit: Option<&str>) -> Option<SecretString> {
    resolve_key_with(provider, explicit, key_from_keyring)
}

/// Standard vendor API-key env vars, keyed by provider id. Checked after the
/// stepper-namespaced var so an explicit `STEPPER_<P>_API_KEY` still wins.
fn well_known_env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        "google" | "google-generative-ai" => Some("GEMINI_API_KEY"),
        "cohere" => Some("COHERE_API_KEY"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "cerebras" => Some("CEREBRAS_API_KEY"),
        "togetherai" => Some("TOGETHER_API_KEY"),
        "fireworks" => Some("FIREWORKS_API_KEY"),
        _ => None,
    }
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

    let stepper_var = format!(
        "STEPPER_{}_API_KEY",
        provider.to_ascii_uppercase().replace('-', "_")
    );
    let candidates = [Some(stepper_var.as_str()), well_known_env_var(provider)];
    for var in candidates.into_iter().flatten() {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(SecretString::from(v.to_string()));
            }
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
    fn well_known_vendor_env_is_recognized_after_the_stepper_var() {
        // A migrator's exported ANTHROPIC_API_KEY resolves without STEPPER_ prefix.
        let var = "ANTHROPIC_API_KEY";
        let prior = std::env::var(var).ok();
        // SAFETY: single-threaded test scope; restored below.
        unsafe { std::env::set_var(var, "sk-vendor") };
        let k = resolve_key_with("anthropic", None, |_| None).unwrap();
        assert_eq!(k.expose_secret(), "sk-vendor");
        // The stepper-namespaced var still takes precedence.
        unsafe { std::env::set_var("STEPPER_ANTHROPIC_API_KEY", "sk-stepper") };
        let k = resolve_key_with("anthropic", None, |_| None).unwrap();
        assert_eq!(k.expose_secret(), "sk-stepper");
        unsafe { std::env::remove_var("STEPPER_ANTHROPIC_API_KEY") };
        match prior {
            Some(v) => unsafe { std::env::set_var(var, v) },
            None => unsafe { std::env::remove_var(var) },
        }
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
