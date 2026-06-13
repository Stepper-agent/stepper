//! ModelRegistry lookup + ModelInfo cost arithmetic: known models price per-token
//! from their table entry (cache read AND write priced separately), aliases map
//! to the current ids, lookup is alias → exact → deterministic longest-prefix
//! (never reverse), and unknown models fall back to a flagged estimate.

use stepper_core::{ModelInfo, ModelRegistry};
use stepper_provider::Usage;

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

#[test]
fn cost_is_tokens_times_per_million_rates_including_cache_write() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("anthropic", "claude-opus-4-8");
    assert!(!info.estimated);
    assert_eq!(info.context_window, 1_000_000);
    assert_eq!(info.max_output_tokens, 128_000);

    let cost = info.cost(&usage(1_000_000, 1_000_000, 1_000_000, 1_000_000));
    let expected = 5.0 + 25.0 + 0.5 + 6.25;
    assert!((cost - expected).abs() < 1e-9, "got {cost}, want {expected}");

    let partial = info.cost(&usage(500_000, 0, 0, 0));
    assert!((partial - 2.5).abs() < 1e-9, "half a million input tokens: {partial}");
}

#[test]
fn zero_usage_is_zero_cost() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("openai", "gpt-5");
    assert_eq!(info.cost(&Usage::default()), 0.0);
}

#[test]
fn aliases_resolve_to_the_current_ids() {
    let reg = ModelRegistry::builtin();

    let opus = reg.lookup("anthropic", "opus");
    assert!(!opus.estimated);
    assert_eq!(opus.input_per_mtok, 5.0);
    assert_eq!(opus.max_output_tokens, 128_000);

    let sonnet = reg.lookup("anthropic", "sonnet");
    assert!(!sonnet.estimated);
    assert_eq!(sonnet.input_per_mtok, 3.0);
    assert_eq!(sonnet.context_window, 200_000);

    let haiku = reg.lookup("anthropic", "haiku");
    assert!(!haiku.estimated);
    assert_eq!(haiku.input_per_mtok, 1.0);
}

#[test]
fn long_context_variant_is_a_distinct_entry() {
    let reg = ModelRegistry::builtin();
    let one_m = reg.lookup("anthropic", "claude-sonnet-4-6[1m]");
    assert!(!one_m.estimated);
    assert_eq!(one_m.context_window, 1_000_000);
    assert_eq!(one_m.input_per_mtok, 6.0);

    let base = reg.lookup("anthropic", "claude-sonnet-4-6");
    assert_eq!(base.context_window, 200_000);
    assert_eq!(base.input_per_mtok, 3.0);
}

#[test]
fn dated_variant_resolves_to_its_base_entry_by_longest_prefix() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("anthropic", "claude-sonnet-4-6-20261114");
    assert!(!info.estimated, "a dated variant resolves via forward prefix match");
    assert_eq!(info.input_per_mtok, 3.0);
    assert_eq!(info.output_per_mtok, 15.0);
}

#[test]
fn longest_prefix_wins_over_a_shorter_nested_id() {
    let reg = ModelRegistry::builtin();
    // Both `qwen3-coder` and `qwen3-coder:480b` prefix this id; the longer
    // entry must win deterministically.
    let info = reg.lookup("ollama-cloud", "qwen3-coder:480b-cloud");
    assert!(!info.estimated);
    assert_eq!(info.context_window, 256_000);
}

#[test]
fn short_query_never_reverse_matches_a_longer_id() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("anthropic", "claude");
    assert!(
        info.estimated,
        "'claude' must not reverse-match claude-opus/sonnet/haiku entries"
    );
    assert_eq!(info.context_window, 128_000);
    assert_eq!(info.max_output_tokens, 8_192);
}

#[test]
fn local_models_are_free_even_at_high_volume() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("ollama", "qwen3-coder");
    assert!(!info.estimated);
    assert_eq!(info.input_per_mtok, 0.0);
    assert_eq!(info.output_per_mtok, 0.0);
    assert_eq!(info.cost(&usage(5_000_000, 5_000_000, 5_000_000, 5_000_000)), 0.0);
}

#[test]
fn unknown_remote_model_falls_back_to_a_flagged_estimate() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("anthropic", "claude-future-99");
    assert!(info.estimated, "unknown model must be marked estimated");
    assert_eq!(info.context_window, 128_000);

    let cost = info.cost(&usage(1_000_000, 1_000_000, 0, 0));
    assert!(cost > 0.0, "unknown remote model is not a silent zero: {cost}");
    let expected = info.input_per_mtok + info.output_per_mtok;
    assert!((cost - expected).abs() < 1e-9);
}

#[test]
fn unknown_local_model_estimate_is_free() {
    let reg = ModelRegistry::builtin();
    let info = reg.lookup("omlx", "some-unlisted-local-model");
    assert!(info.estimated);
    assert_eq!(info.input_per_mtok, 0.0);
    assert_eq!(info.output_per_mtok, 0.0);
    assert_eq!(info.cost(&usage(2_000_000, 2_000_000, 0, 0)), 0.0);
}

#[test]
fn cache_read_and_write_priced_separately_from_input() {
    let info = ModelInfo {
        context_window: 100,
        max_output_tokens: 0,
        input_per_mtok: 10.0,
        output_per_mtok: 20.0,
        cache_read_per_mtok: 1.0,
        cache_write_per_mtok: 2.0,
        estimated: false,
    };
    let cost = info.cost(&usage(1_000_000, 1_000_000, 1_000_000, 1_000_000));
    assert!((cost - 33.0).abs() < 1e-9, "10 + 20 + 1 read + 2 write: {cost}");
}
