use crate::error::CoreError;
use crate::model::{ModelInfo, ModelRegistry};
use crate::ports::ProviderResolver;
use stepper_config::Config;
use stepper_provider::LlmProvider;
use stepper_providers::{CodexTokenStore, ProviderFactory, ProviderKind, ProviderSpec};

/// `ProviderResolver` driven by `setting.json` `providers`. Maps the config
/// `kind` (+ `auth`) onto a concrete adapter and applies the key precedence
/// already resolved by config.
pub struct ConfigProviderResolver {
    config: Config,
    factory: ProviderFactory,
    registry: ModelRegistry,
    codex_store: Option<CodexTokenStore>,
}

impl ConfigProviderResolver {
    pub fn new(
        config: Config,
        factory: ProviderFactory,
        registry: ModelRegistry,
        codex_store: Option<CodexTokenStore>,
    ) -> Self {
        ConfigProviderResolver {
            config,
            factory,
            registry,
            codex_store,
        }
    }
}

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
                if let Some(ctx) = rp.context_window {
                    info.context_window = ctx;
                    info.estimated = false;
                }
                info
            }
            Err(_) => self.registry.lookup("", model_ref),
        }
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
        )
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
        );
        let err = match resolver.resolve("typo/m") {
            Err(e) => e,
            Ok(_) => panic!("expected an unknown-kind error, got a provider"),
        };
        assert!(err.to_string().contains("unknown kind"), "got: {err}");
    }
}
