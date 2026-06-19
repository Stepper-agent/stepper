//! Live model discovery. Two sources, merged: a provider's own model-list
//! endpoint (`/v1/models`-style) tells us which models a key can actually reach,
//! and the [models.dev](https://models.dev) catalog enriches each with metadata
//! (display name, context window, pricing). Powers the `/models` picker and the
//! first-run onboarding so the menu is discovered, not hardcoded.
//!
//! Both fetches degrade gracefully: if the provider endpoint fails (no key yet,
//! offline, local server down) we fall back to the catalog's model list for that
//! provider, and missing catalog metadata just leaves the optional fields `None`.

use crate::error;
use crate::factory::ProviderKind;
use std::collections::HashMap;
use std::time::Duration;
use stepper_provider::ProviderError;

/// models.dev catalog endpoint (a single JSON document of every known provider).
pub const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Per-request deadline for the discovery calls, so a slow or hung endpoint
/// can't wedge the picker. 30s by default (the models.dev catalog is ~2.3MB, so
/// a tight bound fails on slow links); override with `STEPPER_MODEL_FETCH_TIMEOUT_SECS`.
fn fetch_timeout() -> Duration {
    let secs = std::env::var("STEPPER_MODEL_FETCH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(30);
    Duration::from_secs(secs)
}

/// One selectable model, merged from a provider's live list and (when present)
/// the models.dev catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    /// `provider/model-id` — exactly what `--model` / `/model` accepts.
    pub model_ref: String,
    /// The model id without the provider prefix.
    pub id: String,
    /// Catalog display name, or the id when the catalog has no entry.
    pub display_name: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
}

/// Catalog metadata for one model (the subset we surface).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogMeta {
    pub display_name: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
    /// Whether the model supports tool/function calling — i.e. is usable as an
    /// agent (vs an embedding/TTS/image model). Drives the first-run picker.
    pub tool_call: bool,
    /// `YYYY-MM-DD` release date when the catalog has one; used to order the
    /// first-run picker newest-first.
    pub release_date: Option<String>,
}

/// Provider-level metadata from the catalog (the fields `/connect` seeds from):
/// the id/display name plus the hints that map onto a `setting.json` provider —
/// `npm` (the SDK package, which tells us the wire dialect), `api` (the base
/// URL), and `env` (the API-key environment variable name(s)).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderMeta {
    pub id: String,
    pub name: String,
    pub npm: Option<String>,
    pub api: Option<String>,
    pub env: Vec<String>,
}

/// The parsed models.dev catalog: per-model metadata keyed by provider, plus the
/// provider-level metadata (`/connect` seed).
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    by_provider: HashMap<String, HashMap<String, CatalogMeta>>,
    providers: HashMap<String, ProviderMeta>,
}

impl Catalog {
    /// Catalog entries for a provider, trying a couple of well-known aliases
    /// (our `ollama-cloud` is models.dev's `ollama`, `codex` is `openai`).
    fn models_for(&self, provider: &str) -> Option<&HashMap<String, CatalogMeta>> {
        self.by_provider
            .get(provider)
            .or_else(|| self.by_provider.get(catalog_alias(provider)))
    }

    /// Catalog metadata for one `provider`/`model_id`, alias-aware. `None` when
    /// the provider or model id is not in the catalog.
    pub fn meta(&self, provider: &str, model_id: &str) -> Option<&CatalogMeta> {
        self.models_for(provider).and_then(|m| m.get(model_id))
    }

    /// Provider-level metadata for `id` (exact, no aliasing — `/connect` writes
    /// the catalog's own id). `None` when the catalog has no such provider.
    pub fn provider_meta(&self, id: &str) -> Option<&ProviderMeta> {
        self.providers.get(id)
    }

    /// Every catalog provider, sorted by id — the `/connect` picker seed.
    pub fn provider_seeds(&self) -> Vec<&ProviderMeta> {
        let mut seeds: Vec<&ProviderMeta> = self.providers.values().collect();
        seeds.sort_by(|a, b| a.id.cmp(&b.id));
        seeds
    }
}

fn catalog_alias(provider: &str) -> &str {
    match provider {
        "ollama-cloud" => "ollama",
        "codex" => "openai",
        other => other,
    }
}

/// Fetch and parse the models.dev catalog. A failure is returned to the caller,
/// which can still list models from the provider endpoint alone.
pub async fn fetch_catalog(client: &reqwest::Client) -> Result<Catalog, ProviderError> {
    let resp = client
        .get(MODELS_DEV_URL)
        .timeout(fetch_timeout())
        .send()
        .await
        .map_err(error::transport)?;
    let status = resp.status();
    let body = resp.text().await.map_err(error::transport)?;
    if !status.is_success() {
        return Err(error::api_error_from_body(status.as_u16(), &body));
    }
    let root: serde_json::Value = serde_json::from_str(&body).map_err(error::decode)?;
    Ok(parse_catalog(&root))
}

/// Parse the models.dev document shape:
/// `{ "<provider>": { "models": { "<id>": { name, limit:{context,output}, cost:{input,output} } } } }`.
pub fn parse_catalog(root: &serde_json::Value) -> Catalog {
    let mut by_provider = HashMap::new();
    let mut providers_meta = HashMap::new();
    let Some(providers) = root.as_object() else {
        return Catalog { by_provider, providers: providers_meta };
    };
    for (provider_id, provider) in providers {
        let Some(models) = provider.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        let mut entries = HashMap::new();
        for (model_id, model) in models {
            entries.insert(model_id.clone(), parse_catalog_meta(model));
        }
        if !entries.is_empty() {
            by_provider.insert(provider_id.clone(), entries);
            providers_meta.insert(provider_id.clone(), parse_provider_meta(provider_id, provider));
        }
    }
    Catalog { by_provider, providers: providers_meta }
}

/// Pull the provider-level fields off one models.dev provider object. Falls back
/// to the map key for the id/name when the object omits them.
fn parse_provider_meta(provider_id: &str, provider: &serde_json::Value) -> ProviderMeta {
    ProviderMeta {
        id: provider.get("id").and_then(|v| v.as_str()).unwrap_or(provider_id).to_string(),
        name: provider.get("name").and_then(|v| v.as_str()).unwrap_or(provider_id).to_string(),
        npm: provider.get("npm").and_then(|v| v.as_str()).map(String::from),
        api: provider.get("api").and_then(|v| v.as_str()).map(String::from),
        env: provider
            .get("env")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    }
}

fn parse_catalog_meta(model: &serde_json::Value) -> CatalogMeta {
    CatalogMeta {
        display_name: model.get("name").and_then(|v| v.as_str()).map(String::from),
        context_window: model.pointer("/limit/context").and_then(|v| v.as_u64()),
        max_output_tokens: model.pointer("/limit/output").and_then(|v| v.as_u64()),
        input_per_mtok: model.pointer("/cost/input").and_then(|v| v.as_f64()),
        output_per_mtok: model.pointer("/cost/output").and_then(|v| v.as_f64()),
        tool_call: model.get("tool_call").and_then(|v| v.as_bool()).unwrap_or(false),
        release_date: model.get("release_date").and_then(|v| v.as_str()).map(String::from),
    }
}

/// Fetch the model ids a provider serves from its list endpoint. Returns the raw
/// ids in the order the provider reports them. `codex` (ChatGPT OAuth) has no
/// usable list endpoint and yields an empty list.
pub async fn fetch_provider_models(
    client: &reqwest::Client,
    kind: ProviderKind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<String>, ProviderError> {
    if kind == ProviderKind::Codex {
        return Ok(Vec::new());
    }
    let base = base_url.trim_end_matches('/');
    // Anthropic lists at `/v1/models` (x-api-key + version); the OpenAI-compatible
    // providers (OpenAI, Ollama, oMLX, Responses) list at `{base}/models` (bearer).
    let mut rb = match kind {
        ProviderKind::Anthropic => client
            .get(format!("{base}/v1/models"))
            .header("anthropic-version", crate::wire::anthropic::version()),
        _ => client.get(format!("{base}/models")),
    };
    rb = rb.timeout(fetch_timeout());
    if let Some(key) = api_key.map(str::trim).filter(|k| !k.is_empty()) {
        rb = match kind {
            ProviderKind::Anthropic => rb.header("x-api-key", key),
            _ => rb.bearer_auth(key),
        };
    }
    let resp = rb.send().await.map_err(error::transport)?;
    let status = resp.status();
    let body = resp.text().await.map_err(error::transport)?;
    if !status.is_success() {
        return Err(error::api_error_from_body(status.as_u16(), &body));
    }
    let root: serde_json::Value = serde_json::from_str(&body).map_err(error::decode)?;
    Ok(parse_model_list(&root))
}

/// Pull model ids out of a `{ "data": [ { "id": "..." }, ... ] }` envelope
/// (OpenAI + Anthropic both use it); falls back to a bare `[ { "id" } ]` array.
pub fn parse_model_list(root: &serde_json::Value) -> Vec<String> {
    let items = root
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| root.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| item.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

/// List a provider's selectable models, merging the live endpoint (availability)
/// with the catalog (metadata). When the endpoint is unreachable the catalog's
/// model set for that provider is used so the picker is never empty for a known
/// provider.
pub async fn list_models(
    client: &reqwest::Client,
    provider: &str,
    kind: ProviderKind,
    base_url: &str,
    api_key: Option<&str>,
    catalog: Option<&Catalog>,
) -> Vec<ModelEntry> {
    let catalog_models = catalog.and_then(|c| c.models_for(provider));
    let live = match fetch_provider_models(client, kind, base_url, api_key).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("model list fetch failed for provider '{provider}': {e}");
            Vec::new()
        }
    };

    let ids: Vec<String> = if !live.is_empty() {
        live
    } else {
        let mut ids: Vec<String> = catalog_models
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        ids.sort();
        ids
    };

    ids.into_iter()
        .map(|id| {
            let meta = catalog_models.and_then(|m| m.get(&id));
            ModelEntry {
                model_ref: format!("{provider}/{id}"),
                display_name: meta
                    .and_then(|m| m.display_name.clone())
                    .unwrap_or_else(|| id.clone()),
                context_window: meta.and_then(|m| m.context_window),
                max_output_tokens: meta.and_then(|m| m.max_output_tokens),
                input_per_mtok: meta.and_then(|m| m.input_per_mtok),
                output_per_mtok: meta.and_then(|m| m.output_per_mtok),
                id,
            }
        })
        .collect()
}

/// Agent-capable (`tool_call`) catalog models for `provider`, newest-first — for
/// the first-run picker, where no provider keys are configured yet (so a live
/// `/v1/models` call would just 401) and embeddings/TTS/image models are noise.
/// Empty when the catalog has no agent models for the provider.
pub fn onboarding_models(catalog: &Catalog, provider: &str) -> Vec<ModelEntry> {
    let Some(models) = catalog.models_for(provider) else {
        return Vec::new();
    };
    let mut pairs: Vec<(&String, &CatalogMeta)> =
        models.iter().filter(|(_, m)| m.tool_call).collect();
    // Newest release first; id as a stable tiebreaker (undated models sort last).
    pairs.sort_by(|(id_a, a), (id_b, b)| {
        b.release_date.cmp(&a.release_date).then_with(|| id_a.cmp(id_b))
    });
    pairs
        .into_iter()
        .map(|(id, meta)| ModelEntry {
            model_ref: format!("{provider}/{id}"),
            display_name: meta.display_name.clone().unwrap_or_else(|| id.clone()),
            context_window: meta.context_window,
            max_output_tokens: meta.max_output_tokens,
            input_per_mtok: meta.input_per_mtok,
            output_per_mtok: meta.output_per_mtok,
            id: id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn onboarding_models_filters_to_tool_call_newest_first() {
        let mut by_provider = HashMap::new();
        let mut anthropic = HashMap::new();
        // Newer agent model, older agent model, and a non-agent (embedding) model.
        anthropic.insert("claude-new".to_string(), CatalogMeta { tool_call: true, release_date: Some("2026-01-01".into()), context_window: Some(200_000), ..Default::default() });
        anthropic.insert("claude-old".to_string(), CatalogMeta { tool_call: true, release_date: Some("2024-01-01".into()), ..Default::default() });
        anthropic.insert("embed-1".to_string(), CatalogMeta { tool_call: false, ..Default::default() });
        by_provider.insert("anthropic".to_string(), anthropic);
        let catalog = Catalog { by_provider, providers: HashMap::new() };

        let got = onboarding_models(&catalog, "anthropic");
        assert_eq!(got.len(), 2, "the non-tool_call model is excluded");
        assert_eq!(got[0].model_ref, "anthropic/claude-new", "newest release first");
        assert_eq!(got[1].model_ref, "anthropic/claude-old");
        assert_eq!(got[0].context_window, Some(200_000));
        assert!(onboarding_models(&catalog, "openai").is_empty());
    }

    #[test]
    fn parse_catalog_extracts_provider_models_and_meta() {
        let root = serde_json::json!({
            "anthropic": {
                "models": {
                    "claude-opus-4-5": {
                        "name": "Claude Opus 4.5",
                        "limit": { "context": 200000, "output": 64000 },
                        "cost": { "input": 5.0, "output": 25.0 }
                    }
                }
            },
            "novendor": { "doc": "no models key here" }
        });
        let catalog = parse_catalog(&root);
        let meta = catalog
            .models_for("anthropic")
            .and_then(|m| m.get("claude-opus-4-5"))
            .unwrap();
        assert_eq!(meta.display_name.as_deref(), Some("Claude Opus 4.5"));
        assert_eq!(meta.context_window, Some(200000));
        assert_eq!(meta.max_output_tokens, Some(64000));
        assert_eq!(meta.input_per_mtok, Some(5.0));
        assert!(catalog.models_for("novendor").is_none(), "no models => skipped");
    }

    #[test]
    fn parse_catalog_seeds_provider_meta_for_connect() {
        let root = serde_json::json!({
            "anthropic": {
                "id": "anthropic", "name": "Anthropic", "npm": "@ai-sdk/anthropic",
                "api": "https://api.anthropic.com", "env": ["ANTHROPIC_API_KEY"],
                "models": { "claude-x": { "name": "Claude X" } }
            },
            "acme": {
                "name": "Acme AI", "npm": "@ai-sdk/openai-compatible",
                "api": "https://api.acme.ai/v1", "env": ["ACME_API_KEY", "ACME_TOKEN"],
                "models": { "acme-1": { "name": "Acme One" } }
            },
            "novendor": { "doc": "no models => not seeded" }
        });
        let catalog = parse_catalog(&root);
        // Sorted by id, model-less providers excluded.
        let seeds: Vec<&str> = catalog.provider_seeds().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(seeds, vec!["acme", "anthropic"]);
        let acme = catalog.provider_meta("acme").unwrap();
        // `id` falls back to the map key when the object omits it.
        assert_eq!(acme.id, "acme");
        assert_eq!(acme.name, "Acme AI");
        assert_eq!(acme.npm.as_deref(), Some("@ai-sdk/openai-compatible"));
        assert_eq!(acme.api.as_deref(), Some("https://api.acme.ai/v1"));
        assert_eq!(acme.env, vec!["ACME_API_KEY".to_string(), "ACME_TOKEN".to_string()]);
        assert!(catalog.provider_meta("novendor").is_none());
    }

    #[test]
    fn catalog_meta_looks_up_alias_aware_and_misses_cleanly() {
        let catalog = parse_catalog(&serde_json::json!({
            "anthropic": { "models": { "claude-opus-4-5": {
                "name": "Claude Opus 4.5", "limit": { "context": 200000 }
            } } },
            "ollama": { "models": { "qwen3-coder": { "name": "Qwen3 Coder" } } }
        }));
        let meta = catalog.meta("anthropic", "claude-opus-4-5").unwrap();
        assert_eq!(meta.context_window, Some(200000));
        // alias: our `ollama-cloud` resolves to models.dev's `ollama`.
        assert!(catalog.meta("ollama-cloud", "qwen3-coder").is_some());
        assert!(catalog.meta("anthropic", "no-such-model").is_none());
        assert!(catalog.meta("no-such-provider", "x").is_none());
    }

    #[test]
    fn catalog_alias_maps_ollama_cloud_and_codex() {
        let root = serde_json::json!({
            "ollama": { "models": { "qwen3-coder": { "name": "Qwen3 Coder" } } }
        });
        let catalog = parse_catalog(&root);
        assert!(catalog.models_for("ollama-cloud").is_some(), "ollama-cloud -> ollama");
    }

    #[test]
    fn parse_model_list_reads_data_envelope_and_bare_array() {
        let env = serde_json::json!({ "data": [ { "id": "gpt-5" }, { "id": "o3" } ] });
        assert_eq!(parse_model_list(&env), vec!["gpt-5", "o3"]);
        let bare = serde_json::json!([ { "id": "x" } ]);
        assert_eq!(parse_model_list(&bare), vec!["x"]);
        let empty = serde_json::json!({ "object": "list" });
        assert!(parse_model_list(&empty).is_empty());
    }

    #[tokio::test]
    async fn fetch_provider_models_openai_compat_sends_bearer_and_parses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "id": "gpt-5" }, { "id": "gpt-5-mini" } ]
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let ids = fetch_provider_models(
            &client,
            ProviderKind::OpenAiCompat,
            &format!("{}/v1", server.uri()),
            Some("sk-test"),
        )
        .await
        .unwrap();
        assert_eq!(ids, vec!["gpt-5", "gpt-5-mini"]);
    }

    #[tokio::test]
    async fn fetch_provider_models_anthropic_sends_version_and_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "ant-key"))
            .and(header("anthropic-version", crate::wire::anthropic::version()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "id": "claude-opus-4-8", "display_name": "Claude Opus 4.8" } ]
            })))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let ids = fetch_provider_models(
            &client,
            ProviderKind::Anthropic,
            &server.uri(),
            Some("ant-key"),
        )
        .await
        .unwrap();
        assert_eq!(ids, vec!["claude-opus-4-8"]);
    }

    #[tokio::test]
    async fn list_models_merges_live_ids_with_catalog_meta() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [ { "id": "claude-opus-4-5" } ]
            })))
            .mount(&server)
            .await;
        let catalog = parse_catalog(&serde_json::json!({
            "anthropic": { "models": { "claude-opus-4-5": {
                "name": "Claude Opus 4.5", "limit": { "context": 200000 }, "cost": { "input": 5.0 }
            } } }
        }));
        let client = reqwest::Client::new();
        let entries = list_models(
            &client,
            "anthropic",
            ProviderKind::OpenAiCompat,
            &format!("{}/v1", server.uri()),
            None,
            Some(&catalog),
        )
        .await;
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.model_ref, "anthropic/claude-opus-4-5");
        assert_eq!(e.display_name, "Claude Opus 4.5");
        assert_eq!(e.context_window, Some(200000));
    }

    #[tokio::test]
    async fn list_models_falls_back_to_catalog_when_endpoint_fails() {
        // Point at a dead port: the live fetch errors, so the catalog list is used.
        let catalog = parse_catalog(&serde_json::json!({
            "openai": { "models": {
                "gpt-5": { "name": "GPT-5" },
                "o3": { "name": "o3" }
            } }
        }));
        let client = reqwest::Client::new();
        let entries = list_models(
            &client,
            "openai",
            ProviderKind::OpenAiCompat,
            "http://127.0.0.1:1/v1",
            None,
            Some(&catalog),
        )
        .await;
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5", "o3"], "catalog ids, sorted, when live fails");
    }
}
