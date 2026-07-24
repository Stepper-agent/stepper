use crate::error::CoreError;
use crate::model::ModelInfo;
use stepper_protocol::{ModelChoiceView, ProviderChoiceView};
use stepper_provider::LlmProvider;
use stepper_providers::models::ModelEntry;

/// What `connect_provider` derived for a freshly added provider, so the caller
/// can persist it to `setting.json` (the resolver already injected it live, into
/// its own in-memory config, for the rest of the session).
#[derive(Debug, Clone)]
pub struct ConnectedProvider {
    pub kind: String,
    pub base_url: Option<String>,
}

/// An explicit update to one optional provider field: leave it alone, clear
/// it, or set it. The connect flows need all three — e.g. the ChatGPT-OAuth
/// route must CLEAR a stale platform base URL (or requests go to the wrong
/// host), while the bearer routes must NOT touch a deliberately configured
/// proxy base.
#[derive(Debug, Clone, Copy)]
pub enum FieldUpdate<'a> {
    Keep,
    Clear,
    Set(&'a str),
}

/// Turns a `provider/model-id` reference into a live provider + its metadata.
/// Implemented by `ConfigProviderResolver` (config-driven) or by the CLI's
/// convention router when there is no `.stepper/`.
#[async_trait::async_trait]
pub trait ProviderResolver: Send + Sync {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError>;
    fn model_info(&self, model_ref: &str) -> ModelInfo;
    /// List selectable models across the configured providers — each provider's
    /// live list endpoint merged with the models.dev catalog. Defaults to none so
    /// convention/test resolvers need not implement discovery.
    async fn list_models(&self) -> Vec<ModelChoiceView> {
        Vec::new()
    }
    /// The same configured-provider models as [`Self::list_models`], but as raw
    /// catalog entries (context window / pricing intact) for the `stepper models`
    /// CLI. Defaults to none so convention/test resolvers need not implement it.
    async fn list_model_entries(&self) -> Vec<ModelEntry> {
        Vec::new()
    }
    /// The `/connect` provider seed: every provider in the models.dev catalog.
    /// Defaults to none so convention/test resolvers need not implement it.
    async fn list_providers(&self) -> Vec<ProviderChoiceView> {
        Vec::new()
    }
    /// Register the catalog provider `id` (deriving its wire kind + base URL) into
    /// the live config so `/models` and `/model` see it this session. Returns the
    /// derived fields for persistence. Defaults to an error (no catalog).
    async fn connect_provider(&self, _id: &str) -> Result<ConnectedProvider, CoreError> {
        Err(CoreError::Config(
            "this session has no provider catalog to connect from".into(),
        ))
    }
    /// Register a user-defined provider (the `/connect` custom form, or an
    /// auth-method switch) into the live config: set its wire `kind` and apply
    /// the base-URL/auth updates. Unlike [`Self::connect_provider`] this
    /// overwrites what the user just chose explicitly, while leaving
    /// key/model/context overrides alone. Defaults to an error for
    /// convention/test resolvers with no live config.
    fn connect_custom(
        &self,
        _name: &str,
        _kind: &str,
        _base_url: FieldUpdate<'_>,
        _auth: FieldUpdate<'_>,
    ) -> Result<(), CoreError> {
        Err(CoreError::Config(
            "this session cannot register custom providers".into(),
        ))
    }

    /// The configured wire `kind` of `provider` (may be empty for a bare
    /// entry), or `None` when it is not configured. Lets the auth-method menu
    /// keep a bearer-capable kind instead of blindly rewriting it. Defaults to
    /// `None`.
    fn provider_kind(&self, _provider: &str) -> Option<String> {
        None
    }

    /// The names of every provider registered in the live config, for the
    /// `/models` picker to surface configured providers whose model listing
    /// came back empty (unreachable / needs a key) instead of silently hiding
    /// them. Defaults to none.
    fn configured_providers(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether `provider` resolves an explicit/env API key that takes precedence
    /// over the OS keyring (so a key just stored in the keyring would be shadowed).
    /// Advisory only — used to warn after `/login`. Defaults to false.
    fn provider_has_explicit_key(&self, _provider: &str) -> bool {
        false
    }

    /// Whether `provider` is already registered in the live config (configured
    /// or connected this session). Lets `/connect` route an already-configured,
    /// non-catalog provider (a custom one) to the key prompt instead of failing
    /// the catalog lookup. Defaults to false.
    fn provider_configured(&self, _provider: &str) -> bool {
        false
    }
}
