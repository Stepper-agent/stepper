use std::collections::HashMap;
use stepper_provider::Usage;

/// Per-model metadata needed for the context% gauge, the cost footer, and the
/// request output cap.
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    pub context_window: u64,
    /// Hard output cap forwarded as the request `max_tokens` (0 = leave the
    /// provider default in place).
    pub max_output_tokens: u64,
    /// USD per million tokens. Local models are 0.
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_write_per_mtok: f64,
    /// Whether the figures are real or a fallback estimate (UI marks estimates).
    pub estimated: bool,
}

impl ModelInfo {
    pub fn cost(&self, usage: &Usage) -> f64 {
        let m = 1_000_000.0;
        (usage.input as f64 * self.input_per_mtok
            + usage.output as f64 * self.output_per_mtok
            + usage.cache_read as f64 * self.cache_read_per_mtok
            + usage.cache_write as f64 * self.cache_write_per_mtok)
            / m
    }

    /// Overlay any present `CatalogMeta` numeric field (context window, output
    /// cap, input/output price per Mtok) onto these figures. The catalog carries
    /// no cache rates, so the base cache rates are kept — a builtin's hand-tuned
    /// rates survive and a catalog-only model keeps its 0.0 estimate. `estimated`
    /// is cleared only when a real context/price figure was actually applied (an
    /// all-`None` catalog entry must not relabel an estimate as authoritative).
    pub(crate) fn overlaid_with(mut self, meta: &stepper_providers::CatalogMeta) -> ModelInfo {
        let mut applied = false;
        if let Some(ctx) = meta.context_window {
            self.context_window = ctx;
            applied = true;
        }
        if let Some(out) = meta.max_output_tokens {
            self.max_output_tokens = out;
        }
        if let Some(input) = meta.input_per_mtok {
            self.input_per_mtok = input;
            applied = true;
        }
        if let Some(output) = meta.output_per_mtok {
            self.output_per_mtok = output;
            applied = true;
        }
        if applied {
            self.estimated = false;
        }
        self
    }
}

/// Short model refs accepted in place of full ids (`--model anthropic/opus`).
const ALIASES: &[(&str, &str)] = &[
    ("opus", "claude-opus-4-8"),
    ("sonnet", "claude-sonnet-4-6"),
    ("haiku", "claude-haiku-4-5"),
];

/// model-id → `ModelInfo`. Local providers report zero cost; unknown models get
/// an estimated fallback (never a silent zero).
#[derive(Debug, Clone)]
pub struct ModelRegistry {
    table: HashMap<String, ModelInfo>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl ModelRegistry {
    pub fn builtin() -> Self {
        let mut table = HashMap::new();
        let mut add = |name: &str, ctx: u64, max_out: u64, inp: f64, out: f64, cr: f64, cw: f64| {
            table.insert(
                name.to_string(),
                ModelInfo {
                    context_window: ctx,
                    max_output_tokens: max_out,
                    input_per_mtok: inp,
                    output_per_mtok: out,
                    cache_read_per_mtok: cr,
                    cache_write_per_mtok: cw,
                    estimated: false,
                },
            );
        };
        // Anthropic — current ids. Cache write is the 5-minute-TTL rate (1.25x
        // input), cache read 0.1x. The `[1m]` entries are the opt-in long-context
        // variants with the >200k pricing tier; Opus 4.6+ serves 1M natively at
        // standard pricing.
        add("claude-opus-4-8", 1_000_000, 128_000, 5.0, 25.0, 0.5, 6.25);
        add("claude-opus-4-6", 1_000_000, 128_000, 5.0, 25.0, 0.5, 6.25);
        add("claude-sonnet-4-6", 200_000, 64_000, 3.0, 15.0, 0.3, 3.75);
        add("claude-sonnet-4-6[1m]", 1_000_000, 64_000, 6.0, 22.5, 0.6, 7.5);
        add("claude-haiku-4-5", 200_000, 64_000, 1.0, 5.0, 0.1, 1.25);
        // OpenAI — cached input is billed at the cache-read rate, writes are free.
        add("gpt-5", 400_000, 128_000, 1.25, 10.0, 0.125, 0.0);
        add("gpt-5-mini", 400_000, 128_000, 0.25, 2.0, 0.025, 0.0);
        add("gpt-5-codex", 400_000, 128_000, 1.25, 10.0, 0.125, 0.0);
        add("o3", 200_000, 100_000, 2.0, 8.0, 0.5, 0.0);
        add("o4-mini", 200_000, 100_000, 1.1, 4.4, 0.275, 0.0);
        // Local (cost 0); context windows are typical defaults.
        add("qwen3-coder", 256_000, 65_536, 0.0, 0.0, 0.0, 0.0);
        add("qwen3-coder:480b", 256_000, 65_536, 0.0, 0.0, 0.0, 0.0);
        add("deepseek-coder-v2", 128_000, 8_192, 0.0, 0.0, 0.0, 0.0);
        ModelRegistry { table }
    }

    /// Look up by model id: alias → exact → deterministic longest-prefix →
    /// flagged estimate. Prefix matching only goes forward (a dated variant
    /// resolves to its base entry); a short query never matches a longer id.
    pub fn lookup(&self, provider: &str, model: &str) -> ModelInfo {
        let model = ALIASES
            .iter()
            .find(|(alias, _)| *alias == model)
            .map(|(_, id)| *id)
            .unwrap_or(model);
        if let Some(info) = self.table.get(model) {
            return *info;
        }
        // Among entries that prefix `model`, two distinct names of equal length
        // cannot both be prefixes, so max-by-length has a unique winner
        // regardless of map iteration order.
        if let Some(info) = self
            .table
            .iter()
            .filter(|(name, _)| model.starts_with(name.as_str()))
            .max_by_key(|(name, _)| name.len())
            .map(|(_, info)| *info)
        {
            return info;
        }
        let local = is_local(provider);
        ModelInfo {
            context_window: 128_000,
            max_output_tokens: 8_192,
            input_per_mtok: if local { 0.0 } else { 3.0 },
            output_per_mtok: if local { 0.0 } else { 15.0 },
            cache_read_per_mtok: 0.0,
            cache_write_per_mtok: 0.0,
            estimated: true,
        }
    }
}

fn is_local(provider: &str) -> bool {
    matches!(provider, "omlx" | "mlx" | "ollama" | "ollama-cloud" | "local")
}
