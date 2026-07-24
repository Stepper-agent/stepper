use crate::error::CoreError;
use crate::model::{ModelInfo, ModelRegistry};
use crate::ports::{ConnectedProvider, ProviderResolver};
use std::sync::RwLock;
use stepper_config::Config;
use stepper_protocol::{ModelChoiceView, ProviderChoiceView};
use stepper_provider::LlmProvider;
use stepper_providers::models::ModelEntry;
use stepper_providers::{CodexTokenStore, ProviderFactory, ProviderKind, ProviderMeta, ProviderSpec};

/// `ProviderResolver` driven by `setting.json` `providers`. Maps the config
/// `kind` (+ `auth`) onto a concrete adapter and applies the key precedence
/// already resolved by config.
pub struct ConfigProviderResolver {
    /// Behind a lock so `/connect` can register a provider mid-session (the
    /// live-injection path): `resolve`/`model_info` take a read guard, and the
    /// async `list_models` snapshots what it needs and drops the guard before any
    /// `.await` (an `std` guard must never cross an await point).
    config: RwLock<Config>,
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
            config: RwLock::new(config),
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
        // Resolve under the read guard, then drop it (`rp` is owned) before the
        // factory build.
        let rp = self.config.read().unwrap().resolve_provider(model_ref)?;
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

    fn provider_has_explicit_key(&self, provider: &str) -> bool {
        // An explicit literal key or a set `{env:VAR}`/`STEPPER_*` env resolves
        // non-None here and wins over the keyring (explicit > env > keyring).
        self.config
            .read()
            .unwrap()
            .resolve_provider(&format!("{provider}/_"))
            .map(|rp| rp.api_key.is_some())
            .unwrap_or(false)
    }

    fn model_info(&self, model_ref: &str) -> ModelInfo {
        // Bind first so the read guard drops before the registry/catalog work.
        let resolved = self.config.read().unwrap().resolve_provider(model_ref);
        match resolved {
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
                    // Re-assert the invariant after the override: an output cap can
                    // never reach the (now-narrowed) context window.
                    if info.context_window > 0 && info.max_output_tokens >= info.context_window {
                        info.max_output_tokens = 0;
                    }
                }
                // Per-model output cap / pricing overrides (config `models.<id>`):
                // trusted as explicit, so they win over catalog/registry figures.
                if let Some(mo) = rp.max_output_tokens {
                    info.max_output_tokens = mo;
                }
                if let Some(ip) = rp.input_per_mtok {
                    info.input_per_mtok = ip;
                }
                if let Some(op) = rp.output_per_mtok {
                    info.output_per_mtok = op;
                }
                // Re-assert the invariant after a per-model output override: an
                // output cap can never reach the context window (else the wire
                // `max_tokens` exceeds the model limit → provider 400, and the
                // compaction budget saturates to its floor).
                if info.context_window > 0 && info.max_output_tokens >= info.context_window {
                    info.max_output_tokens = 0;
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
        self.list_model_entries()
            .await
            .iter()
            .map(|e| ModelChoiceView { label: model_label(e), model_ref: e.model_ref.clone() })
            .collect()
    }

    async fn list_model_entries(&self) -> Vec<ModelEntry> {
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

        // Snapshot each provider's listing spec under the read guard, then drop
        // it before the network calls below (an std guard can't cross `.await`).
        let specs: Vec<(String, ProviderKind, String, Option<String>)> = {
            let config = self.config.read().unwrap();
            let mut names: Vec<&String> = config.settings.providers.keys().collect();
            names.sort();
            names
                .into_iter()
                .filter_map(|name| {
                    let rp = config.resolve_provider(&format!("{name}/_")).ok()?;
                    let kind = parse_kind(&rp.name, &rp.kind, rp.auth.as_deref()).ok()?;
                    if kind == ProviderKind::Codex {
                        return None;
                    }
                    let base = rp
                        .base_url
                        .clone()
                        .unwrap_or_else(|| default_base_url(kind).to_string());
                    Some((rp.name, kind, base, rp.api_key))
                })
                .collect()
        };

        let mut out = Vec::new();
        for (name, kind, base, api_key) in specs {
            let entries = stepper_providers::models::list_models(
                &client,
                &name,
                kind,
                &base,
                api_key.as_deref(),
                catalog,
            )
            .await;
            out.extend(entries);
        }
        out
    }

    /// The `/connect` seed: every provider in the models.dev catalog (reuse the
    /// one seeded at construction, else fetch once). Best-effort — empty when the
    /// catalog is unavailable.
    async fn list_providers(&self) -> Vec<ProviderChoiceView> {
        let client = self.factory.http_client();
        let fetched = if self.catalog.is_none() {
            stepper_providers::models::fetch_catalog(&client).await.ok()
        } else {
            None
        };
        let Some(catalog) = self.catalog.as_ref().or(fetched.as_ref()) else {
            return Vec::new();
        };
        catalog
            .provider_seeds()
            .into_iter()
            .map(|m| {
                let connectable = provider_connectable(m);
                ProviderChoiceView {
                    id: m.id.clone(),
                    label: provider_label(m, connectable),
                    connectable,
                }
            })
            .collect()
    }

    /// Register catalog provider `id` into the live config so this session can use
    /// it immediately. Derives the wire `kind` from the catalog's `npm` package
    /// and the base URL from its `api` field; the key is supplied separately (the
    /// caller prompts for it). Returns the derived fields for persistence.
    async fn connect_provider(&self, id: &str) -> Result<ConnectedProvider, CoreError> {
        let client = self.factory.http_client();
        let fetched = if self.catalog.is_none() {
            stepper_providers::models::fetch_catalog(&client).await.ok()
        } else {
            None
        };
        let catalog = self
            .catalog
            .as_ref()
            .or(fetched.as_ref())
            .ok_or_else(|| CoreError::Config("models.dev catalog unavailable".into()))?;
        let meta = catalog
            .provider_meta(id)
            .ok_or_else(|| CoreError::Config(format!("unknown provider '{id}'")))?;
        let kind = provider_kind(meta).to_string();
        // Derive the base URL. The catalog omits `api` for ~24 providers; for the
        // ones whose ai-sdk package bakes in the host (openai, groq, xai, mistral,
        // google …) we use a known-host table. The ONLY provider that needs no base
        // is the canonical `anthropic` (the factory default api.anthropic.com +
        // x-api-key is correct). Everything else with no resolvable base — incl.
        // anthropic-flavored CLOUD providers like google-vertex-anthropic (GCP auth,
        // NOT api.anthropic.com) — must be refused: silently defaulting would send
        // the user's key to the wrong host. (api-present anthropic proxies such as
        // freemodel/kimi/minimax take the first arm and keep their own base.)
        let base_url = match meta.api.clone() {
            Some(api) => Some(api),
            None if id == "anthropic" => None,
            None => match known_openai_compat_base(id) {
                Some(base) => Some(base.to_string()),
                None => {
                    return Err(CoreError::Config(format!(
                        "provider '{id}' has no API base URL in the models.dev catalog — \
                         add it manually (providers.{id}.baseUrl in setting.json)"
                    )))
                }
            },
        };
        // Merge into the live config: never clobber an existing provider's key /
        // model / auth / context overrides — only (re)set the wire kind and fill
        // the base URL when absent. No `.await` under the write guard.
        {
            let mut config = self.config.write().unwrap();
            let entry = config.settings.providers.entry(id.to_string()).or_default();
            // Only fill fields that are absent — never clobber a user's deliberate
            // config. A new entry (ProviderConfig::default) has kind=="" so it still
            // gets the catalog kind; an existing `openai-responses`/`codex` provider
            // keeps its dialect (overwriting it to openai-compat would break it).
            if entry.kind.is_empty() {
                entry.kind = kind.clone();
            }
            if entry.base_url.is_none() {
                entry.base_url = base_url.clone();
            }
        }
        Ok(ConnectedProvider { kind, base_url })
    }

    /// Register a user-typed provider into the live config. The kind and base
    /// come straight from the custom form, so they overwrite stale values —
    /// but an existing entry's key / model / context overrides are preserved
    /// (only the two fields the form owns are touched).
    fn connect_custom(
        &self,
        name: &str,
        kind: &str,
        base_url: Option<&str>,
    ) -> Result<(), CoreError> {
        let mut config = self.config.write().unwrap();
        let entry = config.settings.providers.entry(name.to_string()).or_default();
        entry.kind = kind.to_string();
        if let Some(base) = base_url {
            entry.base_url = Some(base.to_string());
        }
        Ok(())
    }
}

/// Whether `connect_provider` can resolve an API base for this catalog entry —
/// the SINGLE source of truth shared with the `/connect` picker so the rows it
/// offers exactly match the ones `connect_provider` accepts. Mirrors the
/// `base_url` decision in `connect_provider`: an explicit `api`, the canonical
/// `anthropic` (factory default), or a known openai-compat host. Everything else
/// (e.g. google-vertex-anthropic) is unconnectable without a manual `baseUrl`.
fn provider_connectable(meta: &ProviderMeta) -> bool {
    meta.api.is_some() || meta.id == "anthropic" || known_openai_compat_base(&meta.id).is_some()
}

/// Base URLs for well-known OpenAI-compatible providers whose models.dev entry
/// omits `api` (their ai-sdk package hard-codes the host). Only hosts we are
/// confident about — anything else is refused rather than guessed, so a key is
/// never sent to the wrong endpoint.
fn known_openai_compat_base(id: &str) -> Option<&'static str> {
    Some(match id {
        "openai" => "https://api.openai.com/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "xai" => "https://api.x.ai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "cerebras" => "https://api.cerebras.ai/v1",
        "togetherai" => "https://api.together.xyz/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        "google" => "https://generativelanguage.googleapis.com/v1beta/openai",
        _ => return None,
    })
}

/// Map a catalog provider onto one of our wire kinds. Only Anthropic has a
/// distinct dialect we implement natively; everything else speaks the
/// OpenAI-compatible API (the catalog's `api` base feeds `{base}/models` etc.).
fn provider_kind(meta: &ProviderMeta) -> &'static str {
    let npm = meta.npm.as_deref().unwrap_or("");
    if meta.id == "anthropic" || npm.contains("anthropic") {
        "anthropic"
    } else {
        "openai-compat"
    }
}

/// `id  ·  Display Name  ·  KEY_ENV_VAR` — the `/connect` picker row (searchable
/// by id or name; the env hint tells the user which key to paste). Unconnectable
/// providers get a trailing note so the dimmed row explains itself.
fn provider_label(meta: &ProviderMeta, connectable: bool) -> String {
    let mut s = format!("{}  ·  {}", meta.id, meta.name);
    if let Some(env) = meta.env.first() {
        s.push_str(&format!("  ·  {env}"));
    }
    if !connectable {
        s.push_str("  ·  (unsupported — set baseUrl manually)");
    }
    s
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
                models: Default::default(),
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
                models: Default::default(),
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
    fn model_info_applies_per_model_output_and_pricing_overrides() {
        // A per-model `models.<id>` entry overrides the catalog output cap + pricing.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        let models = std::collections::BTreeMap::from([(
            "some-model".to_string(),
            stepper_config::ModelOverride {
                context_window: Some(200_000),
                max_output_tokens: Some(8_000),
                input_per_mtok: Some(3.0),
                output_per_mtok: Some(15.0),
            },
        )]);
        config.settings.providers.insert(
            "acme".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: Some("http://localhost/v1".into()),
                api_key: None,
                auth: None,
                default_model: None,
                context_window: None,
                models,
            },
        );
        let resolver = ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            Some(catalog_with("some-model", 300_000, 7.0, 21.0)),
        );
        let info = resolver.model_info("acme/some-model");
        assert_eq!(info.context_window, 200_000, "per-model context beats catalog");
        assert_eq!(info.max_output_tokens, 8_000, "per-model output cap wins");
        assert_eq!(info.input_per_mtok, 3.0, "per-model pricing wins");
        assert_eq!(info.output_per_mtok, 15.0);
    }

    #[test]
    fn model_info_per_model_output_cap_cannot_exceed_context() {
        // A per-model maxOutputTokens >= context must clamp to 0 (provider-400 guard),
        // mirroring the provider-wide context override path.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        let models = std::collections::BTreeMap::from([(
            "tiny".to_string(),
            stepper_config::ModelOverride {
                context_window: Some(32_000),
                max_output_tokens: Some(64_000),
                input_per_mtok: None,
                output_per_mtok: None,
            },
        )]);
        config.settings.providers.insert(
            "acme".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: Some("http://localhost/v1".into()),
                api_key: None,
                auth: None,
                default_model: None,
                context_window: None,
                models,
            },
        );
        let resolver = ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            None,
        );
        let info = resolver.model_info("acme/tiny");
        assert_eq!(info.context_window, 32_000);
        assert_eq!(info.max_output_tokens, 0, "an output cap >= context is dropped to 0");
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
                models: Default::default(),
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

    #[test]
    fn provider_kind_maps_anthropic_else_openai_compat() {
        let anthropic = ProviderMeta {
            id: "anthropic".into(),
            npm: Some("@ai-sdk/anthropic".into()),
            ..Default::default()
        };
        assert_eq!(provider_kind(&anthropic), "anthropic", "by id");
        let by_npm = ProviderMeta {
            id: "claude-proxy".into(),
            npm: Some("@foo/anthropic-sdk".into()),
            ..Default::default()
        };
        assert_eq!(provider_kind(&by_npm), "anthropic", "by npm package");
        let openai = ProviderMeta {
            id: "openai".into(),
            npm: Some("@ai-sdk/openai".into()),
            ..Default::default()
        };
        assert_eq!(provider_kind(&openai), "openai-compat", "everything else is compat");
        let bare = ProviderMeta { id: "x".into(), ..Default::default() };
        assert_eq!(provider_kind(&bare), "openai-compat", "no npm => compat");
    }

    /// A catalog with provider-level `/connect` metadata for `acme` (+ anthropic).
    fn connect_catalog() -> stepper_providers::Catalog {
        stepper_providers::models::parse_catalog(&serde_json::json!({
            "acme": {
                "name": "Acme AI", "npm": "@ai-sdk/openai-compatible",
                "api": "https://api.acme.ai/v1", "env": ["ACME_API_KEY"],
                "models": { "acme-1": { "name": "Acme One", "limit": { "context": 200_000 } } }
            },
            "anthropic": {
                "id": "anthropic", "name": "Anthropic", "npm": "@ai-sdk/anthropic",
                "api": "https://api.anthropic.com", "env": ["ANTHROPIC_API_KEY"],
                "models": { "claude-x": { "name": "Claude X" } }
            }
        }))
    }

    fn resolver_for_connect(catalog: Option<stepper_providers::Catalog>) -> ConfigProviderResolver {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path()).unwrap();
        ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            catalog,
        )
    }

    #[tokio::test]
    async fn connect_provider_injects_into_live_config_and_resolves() {
        let resolver = resolver_for_connect(Some(connect_catalog()));
        // Not registered before connecting.
        assert!(
            resolver.config.read().unwrap().resolve_provider("acme/acme-1").is_err(),
            "acme is absent until connected"
        );
        let connected = resolver.connect_provider("acme").await.unwrap();
        assert_eq!(connected.kind, "openai-compat");
        assert_eq!(connected.base_url.as_deref(), Some("https://api.acme.ai/v1"));
        // Live: the freshly connected provider resolves this session, with the
        // catalog-derived kind + base URL (key comes later, from the keyring).
        let rp = resolver.config.read().unwrap().resolve_provider("acme/acme-1").unwrap();
        assert_eq!(rp.kind, "openai-compat");
        assert_eq!(rp.base_url.as_deref(), Some("https://api.acme.ai/v1"));
        assert!(rp.api_key.is_none(), "no key injected — that rides the keyring");
        // An unknown catalog provider is an error, not a silent no-op.
        assert!(resolver.connect_provider("nope").await.is_err());
    }

    #[test]
    fn connect_custom_overwrites_kind_and_base_but_preserves_other_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        // A pre-existing entry with a user key and default model: the custom
        // form owns kind/baseUrl (the user just typed them) but must not touch
        // the rest.
        config.settings.providers.insert(
            "local".into(),
            ProviderConfig {
                kind: "anthropic".into(),
                base_url: Some("https://old.example/v1".into()),
                api_key: Some("sk-keep".into()),
                auth: None,
                default_model: Some("local/m1".into()),
                context_window: Some(64_000),
                models: Default::default(),
            },
        );
        let resolver = ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            None,
        );
        resolver
            .connect_custom("local", "openai-compat", Some("https://localhost:11111/v1"))
            .unwrap();
        {
            let config = resolver.config.read().unwrap();
            let entry = &config.settings.providers["local"];
            assert_eq!(entry.kind, "openai-compat");
            assert_eq!(entry.base_url.as_deref(), Some("https://localhost:11111/v1"));
            assert_eq!(entry.api_key.as_deref(), Some("sk-keep"));
            assert_eq!(entry.default_model.as_deref(), Some("local/m1"));
            assert_eq!(entry.context_window, Some(64_000));
        }
        // A brand-new name registers from scratch and resolves live (keyless is
        // fine for a localhost openai-compat endpoint).
        resolver.connect_custom("fresh", "openai-compat", Some("http://localhost:8080/v1")).unwrap();
        let rp = resolver.config.read().unwrap().resolve_provider("fresh/some-model").unwrap();
        assert_eq!(rp.kind, "openai-compat");
        assert_eq!(rp.base_url.as_deref(), Some("http://localhost:8080/v1"));
    }

    #[test]
    fn provider_has_explicit_key_reflects_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.settings.providers.insert(
            "acme".into(),
            ProviderConfig {
                kind: "openai-compat".into(),
                base_url: Some("http://localhost/v1".into()),
                api_key: Some("sk-literal".into()),
                auth: None,
                default_model: None,
                context_window: None,
                models: Default::default(),
            },
        );
        let resolver = ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            None,
        );
        assert!(resolver.provider_has_explicit_key("acme"), "an explicit key shadows the keyring");
        assert!(!resolver.provider_has_explicit_key("nope"), "unknown provider: no explicit key");
    }

    #[tokio::test]
    async fn list_providers_returns_catalog_seed_sorted_with_key_hint() {
        let resolver = resolver_for_connect(Some(connect_catalog()));
        let providers = resolver.list_providers().await;
        let ids: Vec<&str> = providers.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["acme", "anthropic"], "seed is sorted by id");
        assert!(
            providers[0].label.contains("ACME_API_KEY"),
            "label hints the expected key env var: {}",
            providers[0].label
        );
    }

    #[tokio::test]
    async fn connect_provider_uses_known_base_for_apiless_compat_and_refuses_unknown() {
        // The real models.dev catalog omits `api` for ai-sdk-native providers like
        // groq; an unknown api-less provider must NOT silently route to OpenAI.
        let catalog = stepper_providers::models::parse_catalog(&serde_json::json!({
            "groq": { "name": "Groq", "npm": "@ai-sdk/groq", "env": ["GROQ_API_KEY"],
                      "models": { "llama-x": {} } },
            "obscure": { "name": "Obscure", "npm": "@ai-sdk/openai-compatible",
                         "env": ["OBSCURE_KEY"], "models": { "m": {} } },
            "anthropic": { "name": "Anthropic", "npm": "@ai-sdk/anthropic",
                           "env": ["ANTHROPIC_API_KEY"], "models": { "claude": {} } },
            "google-vertex-anthropic": { "name": "Vertex Anthropic",
                "npm": "@ai-sdk/google-vertex/anthropic",
                "env": ["GOOGLE_APPLICATION_CREDENTIALS"], "models": { "claude-v": {} } }
        }));
        let resolver = resolver_for_connect(Some(catalog));
        // A known provider gets its real host — never api.openai.com.
        let groq = resolver.connect_provider("groq").await.unwrap();
        assert_eq!(groq.kind, "openai-compat");
        assert_eq!(groq.base_url.as_deref(), Some("https://api.groq.com/openai/v1"));
        // The canonical anthropic provider needs no base (factory default is right).
        let an = resolver.connect_provider("anthropic").await.unwrap();
        assert_eq!(an.kind, "anthropic");
        assert_eq!(an.base_url, None);
        // An unknown api-less openai-compat provider is refused, not misrouted.
        let err = resolver.connect_provider("obscure").await.unwrap_err();
        assert!(err.to_string().contains("no API base URL"), "got: {err}");
        assert!(
            resolver.config.read().unwrap().resolve_provider("obscure/m").is_err(),
            "the refused provider was not injected"
        );
        // An anthropic-FLAVORED cloud provider (Vertex, GCP creds — not the
        // canonical api.anthropic.com x-api-key host) must also be refused rather
        // than defaulted to api.anthropic.com.
        let err2 = resolver.connect_provider("google-vertex-anthropic").await.unwrap_err();
        assert!(err2.to_string().contains("no API base URL"), "got: {err2}");
    }

    #[test]
    fn provider_connectable_mirrors_connect_base_resolution() {
        // api present.
        assert!(provider_connectable(&ProviderMeta {
            id: "acme".into(),
            api: Some("https://api.acme.ai/v1".into()),
            ..Default::default()
        }));
        // canonical anthropic (factory default base is correct).
        assert!(provider_connectable(&ProviderMeta { id: "anthropic".into(), ..Default::default() }));
        // known api-less openai-compat host.
        assert!(provider_connectable(&ProviderMeta { id: "groq".into(), ..Default::default() }));
        // api-less + unknown host + not anthropic → unconnectable.
        assert!(!provider_connectable(&ProviderMeta { id: "obscure".into(), ..Default::default() }));
        assert!(!provider_connectable(&ProviderMeta {
            id: "google-vertex-anthropic".into(),
            ..Default::default()
        }));
    }

    #[tokio::test]
    async fn list_providers_connectable_flag_matches_connect_acceptance() {
        // Same shape as the refuses-unknown catalog: a mix of connectable and not.
        let catalog = stepper_providers::models::parse_catalog(&serde_json::json!({
            "groq": { "name": "Groq", "npm": "@ai-sdk/groq", "env": ["GROQ_API_KEY"], "models": { "llama-x": {} } },
            "obscure": { "name": "Obscure", "npm": "@ai-sdk/openai-compatible", "env": ["OBSCURE_KEY"], "models": { "m": {} } },
            "anthropic": { "name": "Anthropic", "npm": "@ai-sdk/anthropic", "env": ["ANTHROPIC_API_KEY"], "models": { "claude": {} } },
            "google-vertex-anthropic": { "name": "Vertex Anthropic", "npm": "@ai-sdk/google-vertex/anthropic", "env": ["GOOGLE_APPLICATION_CREDENTIALS"], "models": { "claude-v": {} } }
        }));
        let resolver = resolver_for_connect(Some(catalog));
        let listed = resolver.list_providers().await;
        assert!(!listed.is_empty(), "the catalog seeds the picker");
        // Every row's `connectable` flag matches whether `connect_provider` accepts
        // it — the picker can't offer a row the resolver would only reject.
        for p in &listed {
            let accepted = resolver.connect_provider(&p.id).await.is_ok();
            assert_eq!(p.connectable, accepted, "flag vs acceptance mismatch for {}", p.id);
        }
        let vertex = listed.iter().find(|p| p.id == "google-vertex-anthropic").unwrap();
        assert!(!vertex.connectable, "vertex is unconnectable");
        assert!(vertex.label.contains("unsupported"), "dimmed row explains itself: {}", vertex.label);
    }

    #[tokio::test]
    async fn connect_provider_merges_without_clobbering_existing_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.settings.providers.insert(
            "acme".into(),
            ProviderConfig {
                // A deliberately non-compat dialect: reconnecting must NOT rewrite it
                // to the catalog-derived openai-compat (that would break it).
                kind: "openai-responses".into(),
                base_url: Some("https://my-proxy.internal/v1".into()),
                api_key: Some("{env:ACME_KEY}".into()),
                auth: Some("codex-oauth".into()),
                default_model: Some("acme-1".into()),
                context_window: Some(123_000),
                models: Default::default(),
            },
        );
        let resolver = ConfigProviderResolver::new(
            config,
            ProviderFactory::new().unwrap(),
            ModelRegistry::builtin(),
            None,
            Some(connect_catalog()),
        );
        resolver.connect_provider("acme").await.unwrap();
        let cfg = resolver.config.read().unwrap();
        let p = cfg.settings.providers.get("acme").unwrap();
        // Every existing override survives the reconnect (kind/auth/key/model/ctx/base).
        assert_eq!(p.kind, "openai-responses", "an existing wire kind is preserved");
        assert_eq!(p.auth.as_deref(), Some("codex-oauth"));
        assert_eq!(p.api_key.as_deref(), Some("{env:ACME_KEY}"));
        assert_eq!(p.default_model.as_deref(), Some("acme-1"));
        assert_eq!(p.context_window, Some(123_000));
        assert_eq!(
            p.base_url.as_deref(),
            Some("https://my-proxy.internal/v1"),
            "an existing base override is preserved, not overwritten by the catalog"
        );
    }
}
