//! First-run guided setup. When `stepper` is launched interactively in a
//! directory with no `.stepper/` config, offer to scaffold one by picking a
//! default model and a permission mode. An existing config, `--no-init` /
//! `STEPPER_NO_INIT`, a non-interactive stdin (piped or headless `-p`), or the
//! user pressing Ctrl-D all skip it silently and leave the working dir untouched.

use std::io::{IsTerminal, Write};
use std::path::Path;

/// Selectable models `(provider/model-id, one-line description)`. The first
/// entry is the default (blank answer / unrecognized input).
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

/// Resolve a model prompt answer to a concrete `provider/model-id`: a menu
/// number, or an explicit `provider/model-id`. A custom id must be a single
/// `provider/model` made only of printable ASCII without quotes — this rejects
/// whitespace, control, and invisible/zero-width unicode (so the written id is
/// sane and the JSON stays valid). Blank or unrecognized input → the default.
pub(crate) fn resolve_model_choice(input: &str) -> String {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>()
        && let Some((model, _)) = MODEL_MENU.get(n.wrapping_sub(1))
    {
        return model.to_string();
    }
    if trimmed.contains('/')
        && !trimmed.contains('"')
        && trimmed.chars().all(|c| c.is_ascii_graphic())
    {
        return trimmed.to_string();
    }
    MODEL_MENU[0].0.to_string()
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
pub(crate) fn maybe_first_run(cwd: &Path, no_init: bool) -> anyhow::Result<Option<String>> {
    let project_exists = stepper_config::discover(cwd).project_dir.is_some();
    if !should_offer(project_exists, no_init, std::io::stdin().is_terminal()) {
        return Ok(None);
    }

    eprintln!("\nNo .stepper/ config in this folder. Let's set it up.  (Ctrl-D to skip)\n");

    eprintln!("Default model:");
    for (i, (model, desc)) in MODEL_MENU.iter().enumerate() {
        eprintln!("  {}) {model}  ({desc})", i + 1);
    }
    let Some(answer) = prompt(&format!(
        "  number, a provider/model-id, or blank for [{}]: ",
        MODEL_MENU[0].0
    ))?
    else {
        return Ok(None);
    };
    let model = resolve_model_choice(&answer);

    eprintln!("\nPermission mode:");
    for (i, (mode, desc)) in MODE_MENU.iter().enumerate() {
        eprintln!("  {}) {mode}  ({desc})", i + 1);
    }
    let Some(answer) = prompt(&format!("  number or blank for [{}]: ", MODE_MENU[0].0))? else {
        return Ok(None);
    };
    let mode = resolve_mode_choice(&answer);

    write_config(cwd, &model, mode)?;
    eprintln!("\n  using {model} ({mode}) — launching stepper\n");
    Ok(Some(model))
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
/// missing) seeded with the chosen model and mode.
fn write_config(cwd: &Path, model: &str, mode: &str) -> anyhow::Result<()> {
    let dir = cwd.join(".stepper");
    std::fs::create_dir_all(&dir)?;

    let stepper_md = dir.join("stepper.md");
    if !stepper_md.exists() {
        std::fs::write(&stepper_md, crate::scaffold_stepper_md(cwd))?;
        eprintln!("  ✓ wrote {}", stepper_md.display());
    }
    let setting = dir.join("setting.json");
    if !setting.exists() {
        std::fs::write(&setting, crate::scaffold_setting_json(Some(model), mode))?;
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
        assert_eq!(resolve_model_choice(""), MODEL_MENU[0].0);
        assert_eq!(resolve_model_choice("2"), MODEL_MENU[1].0);
        assert_eq!(resolve_model_choice(" 3 "), MODEL_MENU[2].0);
        assert_eq!(resolve_model_choice("openai/gpt-x"), "openai/gpt-x");
        // legit ids with dots/colons are kept (printable ASCII)
        assert_eq!(resolve_model_choice("openai/gpt-4.1"), "openai/gpt-4.1");
        assert_eq!(resolve_model_choice("ollama-cloud/qwen3:30b"), "ollama-cloud/qwen3:30b");
        assert_eq!(resolve_model_choice("99"), MODEL_MENU[0].0); // out of range
        assert_eq!(resolve_model_choice("garbage"), MODEL_MENU[0].0);
        assert_eq!(resolve_model_choice("a/b\"x"), MODEL_MENU[0].0); // quote rejected
        assert_eq!(resolve_model_choice("a / b"), MODEL_MENU[0].0); // whitespace rejected
        assert_eq!(resolve_model_choice("a/b\u{200b}c"), MODEL_MENU[0].0); // zero-width rejected
        assert_eq!(resolve_model_choice("anthropic/claüde"), MODEL_MENU[0].0); // non-ASCII rejected
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
    fn write_config_yields_config_the_orchestrator_honors() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "anthropic/claude-opus-4-8", "plan").unwrap();

        let cfg = stepper_config::Config::load(tmp.path()).unwrap();
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
