use crate::error::CoreError;
use crate::model::{ModelInfo, ModelRegistry};
use crate::ports::ProviderResolver;
use stepper_config::Config;
use stepper_protocol::ModelChoiceView;
use stepper_provider::LlmProvider;
use stepper_providers::models::ModelEntry;
use stepper_providers::{CodexTokenStore, ProviderFactory, ProviderKind, ProviderSpec};

/// `ProviderResolver` driven by `setting.json` `providers`. Maps the config
/// `kind` (+ `auth`) onto a concrete adapter and applies the key precedence
/// already resolved by config.
pub struct ConfigProviderResolver {
    config: Config,
    factory: ProviderFactory,
    registry: ModelRegistry,
    codex_store: Option<CodexTokenStore>,
    /// models.dev catalog seeded once at construction (best-effort; `None` when
    /// the fetch failed). Consulted before the builtin registry estimate so
    /// unknown-but-cataloged models get real context/pricing.
    catalog: Option<stepper_providers::Catalog>,
}

impl ConfigProviderResolver {
    pub fn new(
        config: Config,
        factory: ProviderFactory,
        registry: ModelRegistry,
        codex_store: Option<CodexTokenStore>,
        catalog: Option<stepper_providers::Catalog>,
    ) -> Self {
        ConfigProviderResolver {
            config,
            factory,
            registry,
            codex_store,
            catalog,
        }
    }
}

#[async_trait::async_trait]
impl ProviderResolver for ConfigProviderResolver {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
        let rp = self.config.resolve_provider(model_ref)?;
        let kind = parse_kind(&rp.name, &rp.kind, rp.auth.as_deref())?;

        let mut spec = ProviderSpec::new(kind, rp.name.clone(), rp.model.clone());
        if let Some(base) = &rp.base_url {
            spec = spec.with_base_url(base.clone());
        }
        if let Some(key) = &rp.api_key {
            spec = spec.with_api_key(key.clone());
        }
        if kind == ProviderKind::Codex {
            let store = self.codex_store.clone().ok_or_else(|| {
                CoreError::Config(
                    "codex provider configured but not logged in — run `stepper auth login --codex`"
                        .into(),
                )
            })?;
            spec = spec.with_codex_store(store);
        }

        self.factory.build(spec).map_err(CoreError::from)
    }

    fn model_info(&self, model_ref: &str) -> ModelInfo {
        match self.config.resolve_provider(model_ref) {
            Ok(rp) => {
                let mut info = self.registry.lookup(&rp.name, &rp.model);
                // Catalog figures override the builtin estimate; an explicit
                // `context_window` on the provider still wins below.
                if let Some(catalog) = &self.catalog
                    && let Some(meta) = catalog.meta(&rp.name, &rp.model)
                {
                    info = info.overlaid_with(meta);
                }
                if let Some(ctx) = rp.context_window {
                    info.context_window = ctx;
                    info.estimated = false;
                }
                info
            }
            Err(_) => self.registry.lookup("", model_ref),
        }
    }

    /// Discover selectable models across every configured provider: fetch the
    /// models.dev catalog once, then list each provider's live endpoint merged
    /// with that catalog. Codex (OAuth) and unknown-kind providers are skipped.
    /// Best-effort — an unreachable endpoint falls back to the catalog list, and
    /// a failed catalog still returns whatever the live endpoints reported.
    async fn list_models(&self) -> Vec<ModelChoiceView> {
        // The redirect-following client — the auth client's `redirect: none`
        // would turn a CDN/host 3xx into a failed catalog fetch.
        let client = self.factory.http_client();
        // Reuse the catalog seeded at construction; only fetch here when it
        // wasn't (e.g. construction-time offline), so the picker doesn't
        // re-download the ~2.3MB document every `/models`.
        let fetched = if self.catalog.is_none() {
            match stepper_providers::models::fetch_catalog(&client).await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("models.dev catalog fetch failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        let catalog = self.catalog.as_ref().or(fetched.as_ref());

        let mut names: Vec<&String> = self.config.settings.providers.keys().collect();
        names.sort();

        let mut out = Vec::new();
        for name in names {
            let Ok(rp) = self.config.resolve_provider(&format!("{name}/_")) else {
                continue;
            };
            let Ok(kind) = parse_kind(&rp.name, &rp.kind, rp.auth.as_deref()) else {
                continue;
            };
            if kind == ProviderKind::Codex {
                continue;
            }
            let base = rp
                .base_url
                .clone()
                .unwrap_or_else(|| default_base_url(kind).to_string());
            let entries = stepper_providers::models::list_models(
                &client,
                &rp.name,
                kind,
                &base,
                rp.api_key.as_deref(),
                catalog,
            )
            .await;
            out.extend(entries.iter().map(|e| ModelChoiceView {
                label: model_label(e),
                model_ref: e.model_ref.clone(),
            }));
        }
        out
    }
}

/// The base URL the factory would default to for a provider that didn't set one
/// (kept in sync with `ProviderFactory::build`). Codex is never passed here.
fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "https://api.anthropic.com",
        _ => "https://api.openai.com/v1",
    }
}

/// `provider/model-id · 200k · $5.00/$25.00` — ref plus whatever catalog metadata
/// is known (context window, input/output price per Mtok).
fn model_label(e: &ModelEntry) -> String {
    let mut s = e.model_ref.clone();
    if let Some(ctx) = e.context_window {
        s.push_str(&format!("  ·  {}", human_tokens(ctx)));
    }
    if let (Some(input), Some(output)) = (e.input_per_mtok, e.output_per_mtok) {
        s.push_str(&format!("  ·  ${input:.2}/${output:.2}"));
    }
    s
}

fn human_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// An unknown kind is an error rather than an OpenAI-compat fallback: a typo'd
/// kind must fail loudly instead of silently sending the key down the compat
/// path (`stepper config --validate` reports the same set).
fn parse_kind(provider: &str, kind: &str, auth: Option<&str>) -> Result<ProviderKind, CoreError> {
    match kind {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "openai-responses" => {
            if auth == Some("codex-oauth") {
                Ok(ProviderKind::Codex)
            } else {
                Ok(ProviderKind::OpenAiResponses)
            }
        }
        "codex" => Ok(ProviderKind::Codex),
        "openai-compat" => Ok(ProviderKind::OpenAiCompat),
        other => Err(CoreError::Config(format!(
            "provider '{provider}' has unknown kind '{other}' (expected one of: {})",
            stepper_config::PROVIDER_KINDS.join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stepper_config::{Config, ProviderConfig};
    use stepper_providers::ProviderFactory;

    fn resolver_with(ctx: Option<u64>) -> ConfigProviderResolver {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.settings.providers.insert(
            "acme".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: Some("http://localhost/v1".into()),
                api_key: None,
                auth: None,
                default_model: None,
                context_window: ctx,
            },
        );
        ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            None,
        )
    }

    /// Resolver with an `acme` openai-compat provider and an injected catalog.
    fn resolver_with_catalog(catalog: stepper_providers::Catalog, ctx: Option<u64>) -> ConfigProviderResolver {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.settings.providers.insert(
            "acme".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: Some("http://localhost/v1".into()),
                api_key: None,
                auth: None,
                default_model: None,
                context_window: ctx,
            },
        );
        ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            Some(catalog),
        )
    }

    fn catalog_with(model: &str, ctx: u64, input: f64, output: f64) -> stepper_providers::Catalog {
        stepper_providers::models::parse_catalog(&serde_json::json!({
            "acme": { "models": { model: {
                "name": model,
                "limit": { "context": ctx, "output": 32_000 },
                "cost": { "input": input, "output": output }
            } } }
        }))
    }

    #[test]
    fn model_info_prefers_catalog_when_present() {
        let info = resolver_with_catalog(catalog_with("some-model", 300_000, 7.0, 21.0), None)
            .model_info("acme/some-model");
        assert_eq!(info.context_window, 300_000);
        assert_eq!(info.input_per_mtok, 7.0);
        assert_eq!(info.output_per_mtok, 21.0);
        assert_eq!(info.max_output_tokens, 32_000);
        assert!(!info.estimated, "catalog figures are authoritative");
    }

    #[test]
    fn model_info_explicit_override_still_wins_over_catalog() {
        let info = resolver_with_catalog(catalog_with("some-model", 300_000, 7.0, 21.0), Some(50_000))
            .model_info("acme/some-model");
        assert_eq!(info.context_window, 50_000, "provider context_window beats the catalog");
        assert_eq!(info.input_per_mtok, 7.0, "pricing still comes from the catalog");
        assert!(!info.estimated);
    }

    #[test]
    fn model_info_falls_back_to_registry_estimate_when_catalog_misses() {
        let info = resolver_with_catalog(catalog_with("other-model", 300_000, 7.0, 21.0), None)
            .model_info("acme/some-unknown-model");
        assert_eq!(info.context_window, 128_000, "no catalog entry -> registry estimate");
        assert!(info.estimated);
    }

    #[test]
    fn model_info_applies_context_window_override_for_unknown_model() {
        let info = resolver_with(Some(64_000)).model_info("acme/some-unknown-model");
        assert_eq!(info.context_window, 64_000);
        assert!(!info.estimated, "an explicit override is authoritative, not an estimate");
    }

    #[test]
    fn model_info_without_override_falls_back_to_registry_estimate() {
        let info = resolver_with(None).model_info("acme/some-unknown-model");
        assert_eq!(info.context_window, 128_000);
        assert!(info.estimated);
    }

    #[test]
    fn parse_kind_maps_known_kinds() {
        assert_eq!(parse_kind("p", "openai-compat", None).unwrap(), ProviderKind::OpenAiCompat);
        assert_eq!(parse_kind("p", "anthropic", None).unwrap(), ProviderKind::Anthropic);
        assert_eq!(
            parse_kind("p", "openai-responses", None).unwrap(),
            ProviderKind::OpenAiResponses
        );
        assert_eq!(
            parse_kind("p", "openai-responses", Some("codex-oauth")).unwrap(),
            ProviderKind::Codex
        );
        assert_eq!(parse_kind("p", "codex", None).unwrap(), ProviderKind::Codex);
    }

    #[test]
    fn parse_kind_rejects_unknown_kind() {
        let err = parse_kind("acme", "openai-compt", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown kind 'openai-compt'"), "got: {msg}");
        assert!(msg.contains("openai-compat"), "lists known kinds: {msg}");
    }

    #[test]
    fn resolve_errors_on_unknown_kind_instead_of_compat_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.settings.providers.insert(
            "typo".into(),
            ProviderConfig {
                kind: "openai-compatt".into(),
                base_url: Some("http://localhost/v1".into()),
                api_key: None,
                auth: None,
                default_model: None,
                context_window: None,
            },
        );
        let resolver = ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            None,
        );
        let err = match resolver.resolve("typo/m") {
            Err(e) => e,
            Ok(_) => panic!("expected an unknown-kind error, got a provider"),
        };
        assert!(err.to_string().contains("unknown kind"), "got: {err}");
    }
}
