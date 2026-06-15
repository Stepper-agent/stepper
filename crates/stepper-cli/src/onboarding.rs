//! First-run guided setup. When `stepper` is launched interactively in a
//! directory with no `.stepper/` config, offer to scaffold one by picking a
//! default model and a permission mode. An existing config, `--no-init` /
//! `STEPPER_NO_INIT`, a non-interactive stdin (piped or headless `-p`), or the
//! user pressing Ctrl-D all skip it silently and leave the working dir untouched.

use std::io::{IsTerminal, Write};
use std::path::Path;
use stepper_providers::{fetch_catalog, onboarding_models, ModelEntry, ProviderFactory};

/// Newest models shown per provider in the first-run picker (the rest are still
/// reachable by typing a `provider/model-id`).
const MODELS_PER_PROVIDER: usize = 6;

/// Recommended default written for a blank/unrecognized model answer.
const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-6";

/// Providers the first-run picker discovers models for (the ones stepper
/// auto-configures with a known base URL — pick one, add a key, done). The
/// models.dev catalog aliases `ollama-cloud` to `ollama` internally.
const ONBOARDING_PROVIDERS: &[&str] = &["anthropic", "openai", "ollama-cloud"];

/// Offline/fetch-failed fallback menu `(provider/model-id, one-line description)`.
const MODEL_MENU: &[(&str, &str)] = &[
    ("anthropic/claude-sonnet-4-6", "balanced — recommended"),
    ("anthropic/claude-opus-4-8", "most capable"),
    ("ollama-cloud/qwen3-coder", "open weights"),
];

/// Selectable permission modes `(setting.json value, one-line description)`. The
/// first entry is the default.
const MODE_MENU: &[(&str, &str)] = &[
    (
        "accept-edits",
        "auto-apply edits, ask before risky commands",
    ),
    ("plan", "read-only planning first"),
    ("auto", "run tools without prompting"),
    ("default", "ask before edits and commands"),
];

/// Offer first-run setup only when there is no project config, the user has not
/// opted out, and stdin is an interactive terminal (so a piped or headless run
/// never blocks on a prompt).
pub(crate) fn should_offer(project_exists: bool, no_init: bool, is_tty: bool) -> bool {
    !project_exists && !no_init && is_tty
}

/// Resolve a model prompt answer to a concrete `provider/model-id`: a 1-based
/// number into `menu`, or an explicit `provider/model-id`. A custom id must be a
/// single `provider/model` of printable ASCII without quotes — this rejects
/// whitespace, control, and invisible/zero-width unicode (so the written id is
/// sane and the JSON stays valid). Blank or unrecognized input → [`DEFAULT_MODEL`].
pub(crate) fn resolve_model_choice(input: &str, menu: &[String]) -> String {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>()
        && let Some(model) = menu.get(n.wrapping_sub(1))
    {
        return model.clone();
    }
    if trimmed.contains('/')
        && !trimmed.contains('"')
        && trimmed.chars().all(|c| c.is_ascii_graphic())
    {
        return trimmed.to_string();
    }
    DEFAULT_MODEL.to_string()
}

/// One provider's discovered models: the newest `MODELS_PER_PROVIDER` shown, plus
/// how many more agent models exist (so the picker can hint at the rest).
struct ProviderModels {
    provider: &'static str,
    shown: Vec<ModelEntry>,
    more: usize,
}

/// Discover selectable models from the models.dev catalog (no provider key
/// needed), grouped by provider in `ONBOARDING_PROVIDERS` order, newest-first and
/// capped. Empty on any failure (offline, catalog down) so the caller falls back
/// to the static menu.
async fn discover_models() -> Vec<ProviderModels> {
    let Ok(factory) = ProviderFactory::new() else {
        return Vec::new();
    };
    let client = factory.http_client();
    let Ok(catalog) = fetch_catalog(&client).await else {
        return Vec::new();
    };
    ONBOARDING_PROVIDERS
        .iter()
        .filter_map(|&provider| {
            let mut models = onboarding_models(&catalog, provider);
            if models.is_empty() {
                return None;
            }
            let more = models.len().saturating_sub(MODELS_PER_PROVIDER);
            models.truncate(MODELS_PER_PROVIDER);
            Some(ProviderModels { provider, shown: models, more })
        })
        .collect()
}

/// A one-line label for a discovered model: `provider/id  ·  context`.
fn model_label(entry: &ModelEntry) -> String {
    match entry.context_window {
        Some(ctx) if ctx >= 1000 => format!("{}  ·  {}K ctx", entry.model_ref, ctx / 1000),
        _ => entry.model_ref.clone(),
    }
}

/// Print the model menu (discovered if available, else the static fallback) and
/// return the parallel list of `provider/model-id` refs a number selects.
fn print_model_menu(discovered: &[ProviderModels]) -> Vec<String> {
    if discovered.is_empty() {
        eprintln!("Default model:");
        for (i, (model, desc)) in MODEL_MENU.iter().enumerate() {
            eprintln!("  {}) {model}  ({desc})", i + 1);
        }
        return MODEL_MENU.iter().map(|(m, _)| m.to_string()).collect();
    }
    eprintln!("Default model (discovered from models.dev — or type any provider/model-id):");
    let mut refs = Vec::new();
    for group in discovered {
        eprintln!("  {}:", group.provider);
        for entry in &group.shown {
            refs.push(entry.model_ref.clone());
            eprintln!("  {:>3}) {}", refs.len(), model_label(entry));
        }
        if group.more > 0 {
            eprintln!("       … +{} more (type the {}/<id>)", group.more, group.provider);
        }
    }
    refs
}

/// Resolve a mode prompt answer to a `setting.json` mode string: a menu number,
/// or blank/unrecognized for the default (first entry).
pub(crate) fn resolve_mode_choice(input: &str) -> &'static str {
    if let Ok(n) = input.trim().parse::<usize>()
        && let Some((mode, _)) = MODE_MENU.get(n.wrapping_sub(1))
    {
        return mode;
    }
    MODE_MENU[0].0
}

/// Run the first-run setup when appropriate. Returns the chosen model ref when a
/// config was written (so the caller can use it for this session), else `None`
/// (config present, opted out, non-TTY, or the user skipped).
pub(crate) async fn maybe_first_run(cwd: &Path, no_init: bool) -> anyhow::Result<Option<String>> {
    let project_exists = stepper_config::discover(cwd).project_dir.is_some();
    if !should_offer(project_exists, no_init, std::io::stdin().is_terminal()) {
        return Ok(None);
    }

    eprintln!("\nNo .stepper/ config in this folder. Let's set it up.  (Ctrl-D to skip)\n");

    eprintln!("  (discovering models…)");
    let discovered = discover_models().await;
    let model_menu = print_model_menu(&discovered);
    let Some(answer) = prompt(&format!(
        "  number, a provider/model-id, or blank for [{DEFAULT_MODEL}]: "
    ))?
    else {
        return Ok(None);
    };
    let model = resolve_model_choice(&answer, &model_menu);

    eprintln!("\nPermission mode:");
    for (i, (mode, desc)) in MODE_MENU.iter().enumerate() {
        eprintln!("  {}) {mode}  ({desc})", i + 1);
    }
    let Some(answer) = prompt(&format!("  number or blank for [{}]: ", MODE_MENU[0].0))? else {
        return Ok(None);
    };
    let mode = resolve_mode_choice(&answer);

    // Optional runaway guards (blank = no limit — runaway is assumed rare). These
    // land in setting.json `limits` and apply to every session in this project.
    eprintln!("\nRunaway guards (optional — blank for none):");
    let Some(answer) = prompt("  stop a turn after how many minutes? ")? else {
        return Ok(None);
    };
    let turn_timeout_secs = resolve_minutes_to_secs(&answer);
    let Some(answer) = prompt("  stop once a session spends how many USD? ")? else {
        return Ok(None);
    };
    let max_budget_usd = resolve_budget_usd(&answer);
    let limits = stepper_config::LimitsConfig {
        turn_timeout_secs,
        max_budget_usd,
        max_turns: None,
    };

    write_config(cwd, &model, mode, &limits)?;
    eprintln!("\n  using {model} ({mode}) — launching stepper\n");
    Ok(Some(model))
}

/// Parse a minutes answer into whole seconds (blank / non-numeric / ≤0 → no
/// limit). Fractional minutes are allowed (e.g. `0.5` → 30s).
pub(crate) fn resolve_minutes_to_secs(input: &str) -> Option<u64> {
    let minutes: f64 = input.trim().parse().ok()?;
    (minutes > 0.0).then_some((minutes * 60.0) as u64)
}

/// Parse a USD answer into a budget cap (blank / non-numeric / ≤0 → no limit).
pub(crate) fn resolve_budget_usd(input: &str) -> Option<f64> {
    let usd: f64 = input.trim().parse().ok()?;
    (usd > 0.0).then_some(usd)
}

/// Print `label` to stderr and read one line from stdin. `None` on EOF (Ctrl-D)
/// so the caller treats it as "skip".
fn prompt(label: &str) -> anyhow::Result<Option<String>> {
    eprint!("{label}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        eprintln!();
        return Ok(None);
    }
    Ok(Some(line))
}

/// Create `.stepper/` and write `stepper.md` (if missing) + `setting.json` (if
/// missing) seeded with the chosen model, mode, and (optional) limits.
fn write_config(
    cwd: &Path,
    model: &str,
    mode: &str,
    limits: &stepper_config::LimitsConfig,
) -> anyhow::Result<()> {
    let dir = cwd.join(".stepper");
    std::fs::create_dir_all(&dir)?;

    let stepper_md = dir.join("stepper.md");
    if !stepper_md.exists() {
        std::fs::write(&stepper_md, crate::scaffold_stepper_md(cwd))?;
        eprintln!("  ✓ wrote {}", stepper_md.display());
    }
    let setting = dir.join("setting.json");
    if !setting.exists() {
        let limits = limits.is_set().then_some(limits);
        std::fs::write(&setting, crate::scaffold_setting_json(Some(model), mode, limits))?;
        eprintln!("  ✓ wrote {}", setting.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_only_when_interactive_unconfigured_and_opted_in() {
        assert!(should_offer(false, false, true));
        assert!(!should_offer(true, false, true)); // config already exists
        assert!(!should_offer(false, true, true)); // --no-init
        assert!(!should_offer(false, false, false)); // piped / headless
    }

    #[test]
    fn model_choice_resolves_number_custom_and_falls_back() {
        // A discovered (or fallback) menu of provider/model-id refs.
        let menu: Vec<String> = ["anthropic/a", "openai/b", "ollama-cloud/c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(resolve_model_choice("", &menu), DEFAULT_MODEL);
        assert_eq!(resolve_model_choice("2", &menu), "openai/b");
        assert_eq!(resolve_model_choice(" 3 ", &menu), "ollama-cloud/c");
        assert_eq!(resolve_model_choice("openai/gpt-x", &menu), "openai/gpt-x");
        // legit ids with dots/colons are kept (printable ASCII)
        assert_eq!(resolve_model_choice("openai/gpt-4.1", &menu), "openai/gpt-4.1");
        assert_eq!(resolve_model_choice("ollama-cloud/qwen3:30b", &menu), "ollama-cloud/qwen3:30b");
        assert_eq!(resolve_model_choice("99", &menu), DEFAULT_MODEL); // out of range
        assert_eq!(resolve_model_choice("garbage", &menu), DEFAULT_MODEL);
        assert_eq!(resolve_model_choice("a/b\"x", &menu), DEFAULT_MODEL); // quote rejected
        assert_eq!(resolve_model_choice("a / b", &menu), DEFAULT_MODEL); // whitespace rejected
        assert_eq!(resolve_model_choice("a/b\u{200b}c", &menu), DEFAULT_MODEL); // zero-width rejected
        assert_eq!(resolve_model_choice("anthropic/claüde", &menu), DEFAULT_MODEL); // non-ASCII rejected
    }

    #[test]
    fn print_model_menu_uses_static_fallback_when_no_discovery() {
        // Empty discovery → the 3-item static menu, refs in order.
        let refs = print_model_menu(&[]);
        assert_eq!(refs, vec![MODEL_MENU[0].0, MODEL_MENU[1].0, MODEL_MENU[2].0]);
    }

    #[test]
    fn print_model_menu_flattens_discovered_groups_in_order() {
        use stepper_providers::ModelEntry;
        let entry = |r: &str| ModelEntry {
            model_ref: r.into(),
            id: r.split('/').nth(1).unwrap().into(),
            display_name: r.into(),
            context_window: Some(200_000),
            max_output_tokens: None,
            input_per_mtok: None,
            output_per_mtok: None,
        };
        let discovered = vec![
            ProviderModels {
                provider: "anthropic",
                shown: vec![entry("anthropic/x"), entry("anthropic/y")],
                more: 3,
            },
            ProviderModels {
                provider: "openai",
                shown: vec![entry("openai/z")],
                more: 0,
            },
        ];
        let refs = print_model_menu(&discovered);
        assert_eq!(refs, vec!["anthropic/x", "anthropic/y", "openai/z"]);
    }

    #[test]
    fn mode_choice_resolves_number_and_falls_back() {
        assert_eq!(resolve_mode_choice(""), MODE_MENU[0].0);
        assert_eq!(resolve_mode_choice("2"), MODE_MENU[1].0);
        assert_eq!(resolve_mode_choice("4"), MODE_MENU[3].0);
        assert_eq!(resolve_mode_choice("nonsense"), MODE_MENU[0].0);
    }

    #[test]
    fn chosen_modes_are_all_parseable_by_the_engine() {
        for (mode, _) in MODE_MENU {
            assert!(
                crate::core_setup::parse_mode(mode).is_some(),
                "mode '{mode}' must be accepted by parse_mode"
            );
        }
    }

    #[test]
    fn minutes_and_budget_parse_blank_and_values() {
        assert_eq!(resolve_minutes_to_secs(""), None);
        assert_eq!(resolve_minutes_to_secs("  "), None);
        assert_eq!(resolve_minutes_to_secs("0"), None);
        assert_eq!(resolve_minutes_to_secs("nope"), None);
        assert_eq!(resolve_minutes_to_secs("10"), Some(600));
        assert_eq!(resolve_minutes_to_secs("0.5"), Some(30));
        assert_eq!(resolve_budget_usd(""), None);
        assert_eq!(resolve_budget_usd("0"), None);
        assert_eq!(resolve_budget_usd("garbage"), None);
        assert_eq!(resolve_budget_usd("5"), Some(5.0));
        assert_eq!(resolve_budget_usd("2.50"), Some(2.5));
    }

    #[test]
    fn write_config_yields_config_the_orchestrator_honors() {
        let tmp = tempfile::tempdir().unwrap();
        let limits = stepper_config::LimitsConfig {
            turn_timeout_secs: Some(600),
            max_budget_usd: Some(5.0),
            max_turns: None,
        };
        write_config(tmp.path(), "anthropic/claude-opus-4-8", "plan", &limits).unwrap();

        let cfg = stepper_config::Config::load(tmp.path()).unwrap();
        let got = cfg.settings.limits.clone().expect("limits written");
        assert_eq!(got.turn_timeout_secs, Some(600));
        assert_eq!(got.max_budget_usd, Some(5.0));
        assert!(
            cfg.project_dir.is_some(),
            "the new .stepper/ must be discovered"
        );
        // build_steps() uses orchestrator_model() (-> defaultModel) for the
        // implicit step, so the chosen model actually drives the session.
        assert_eq!(
            cfg.orchestrator_model().as_deref(),
            Some("anthropic/claude-opus-4-8")
        );
        assert_eq!(cfg.settings.mode.as_deref(), Some("plan"));
        assert!(tmp.path().join(".stepper/stepper.md").exists());
    }
}
