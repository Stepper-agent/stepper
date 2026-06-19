use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use stepper_protocol::Mode;

#[derive(Parser)]
#[command(name = "stepper", version, about = "Layered CLI AI coding agent")]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Args, Clone)]
pub struct GlobalArgs {
    /// Override the active model, as `provider/model-id`.
    #[arg(long, global = true)]
    pub model: Option<String>,
    /// Fallback model (`provider/model-id`) tried once when a step's primary
    /// model fails non-retryably or exhausts its retries.
    #[arg(long, global = true)]
    pub fallback_model: Option<String>,
    /// Start in this mode (auto | plan | accept-edits | default | dont-ask).
    #[arg(long, value_enum, global = true)]
    pub mode: Option<ModeArg>,
    /// Reasoning effort applied to every layer (off | low | medium | high).
    /// Maps to OpenAI `reasoning_effort` + Anthropic extended-thinking budget;
    /// a layer's own `reasoning-effort` frontmatter overrides it.
    #[arg(long, global = true)]
    pub effort: Option<String>,
    /// One-shot non-interactive prompt (no inline viewport).
    #[arg(short = 'p', long, global = true)]
    pub print: Option<String>,
    /// Resume a saved session by id.
    #[arg(long, global = true)]
    pub resume: Option<String>,
    /// Resume the most recent session for this project.
    #[arg(short = 'c', long = "continue", global = true, conflicts_with = "resume")]
    pub continue_session: bool,
    /// Name this session (stored on the record, shown by session pickers).
    #[arg(long, global = true)]
    pub name: Option<String>,
    /// Project working directory (defaults to the current dir).
    #[arg(long, global = true)]
    pub cwd: Option<PathBuf>,
    /// Abort the turn after this many total ReAct steps (across layers).
    #[arg(long, global = true)]
    pub max_turns: Option<u32>,
    /// Abort once the accumulated session cost (USD) reaches this cap.
    #[arg(long, global = true)]
    pub max_budget_usd: Option<f64>,
    /// Stop a turn once it has run this many wall-clock seconds (runaway guard).
    /// Wall-clock — it also counts time spent waiting at an approval prompt. 0 =
    /// no limit.
    #[arg(long, global = true)]
    pub turn_timeout: Option<u64>,
    /// DANGEROUS: bypass-permissions mode — everything that would prompt is
    /// allowed; explicit deny rules still deny.
    #[arg(long, global = true, conflicts_with = "mode")]
    pub dangerously_skip_permissions: bool,
    /// DANGEROUS (headless `-p` only): blindly approve every approval prompt
    /// instead of denying it.
    #[arg(long, global = true)]
    pub dangerously_auto_approve: bool,
    /// Skip the first-run guided setup that offers to scaffold `.stepper/` when
    /// no project config is found (also via the `STEPPER_NO_INIT` env var).
    #[arg(long, global = true, env = "STEPPER_NO_INIT")]
    pub no_init: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Launch the interactive TUI (this is also the default with no subcommand).
    Run,
    /// Manage provider credentials, e.g. `stepper auth login --codex`.
    Auth(AuthArgs),
    /// Inspect or validate `.stepper/setting.json`.
    Config(ConfigArgs),
    /// Scan the repo and generate `.stepper/stepper.md`.
    Init,
    /// Scaffold a new layer: `stepper layer <name>`.
    Layer {
        /// Layer name (letters, digits, '-' and '_').
        name: String,
    },
    /// Scaffold a new slash command: `stepper command <name>`.
    #[command(name = "command")]
    Cmd {
        /// Command name (letters, digits, '-' and '_').
        name: String,
    },
    /// Write a default plan → implement → review layer pipeline.
    ScaffoldLayer,
    /// Migrate another agent's global config (Claude Code / Codex / Cursor /
    /// Gemini) into `~/.stepper/`. Shows the plan then asks before writing;
    /// `--dry-run` previews only, `--yes` skips the prompt.
    Import(ImportArgs),
}

#[derive(Args)]
pub struct ImportArgs {
    /// Which agent to import from (default `all`). Positional
    /// (`stepper import claude`) — matches the `/import` slash form.
    #[arg(value_enum)]
    pub source: Option<ImportSourceArg>,
    /// Same as the positional source, as a flag.
    #[arg(long, value_enum)]
    pub from: Option<ImportSourceArg>,
    /// Show the migration plan without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Apply without the interactive confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Import source for the CLI — clap enforces the value and lists it in `--help`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ImportSourceArg {
    Claude,
    Codex,
    Cursor,
    Gemini,
    All,
}

impl ImportArgs {
    /// The resolved source: the positional wins, else `--from`, else `all`.
    pub fn resolved_source(&self) -> ImportSourceArg {
        self.source.or(self.from).unwrap_or(ImportSourceArg::All)
    }
}

#[derive(Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub cmd: AuthCmd,
}

#[derive(Subcommand)]
pub enum AuthCmd {
    /// Log in to a provider.
    Login(AuthLoginArgs),
    /// Store a provider's API key in the OS keyring (read from stdin).
    SetKey {
        /// Provider name, e.g. `anthropic` or `ollama-cloud`.
        provider: String,
    },
    /// Remove a provider's API key from the OS keyring.
    DeleteKey {
        provider: String,
    },
}

#[derive(Args)]
pub struct AuthLoginArgs {
    /// Use the ChatGPT "Sign in with ChatGPT" OAuth flow (Codex backend).
    #[arg(long)]
    pub codex: bool,
}

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub action: Option<ConfigAction>,
    /// Print the JSON Schema for `.stepper/setting.json`.
    #[arg(long)]
    pub schema: bool,
    /// Validate the project's `.stepper/setting.json`.
    #[arg(long)]
    pub validate: bool,
}

/// `stepper config set/get` — edit a scalar `setting.json` key (validated) or
/// print its resolved value.
#[derive(Subcommand)]
pub enum ConfigAction {
    /// Set a scalar key (e.g. `defaultModel`, `mode`, `limits.turnTimeoutSecs`,
    /// `dispatch.enabled`) in the project (or `~/.stepper`) setting.json.
    Set {
        /// Dotted key.
        key: String,
        /// New value (parsed to the key's type and validated before writing).
        value: String,
    },
    /// Print a scalar key's resolved value (`(unset)` when absent).
    Get {
        /// Dotted key.
        key: String,
    },
}

/// `--mode` choices. `Bypass` is deliberately absent — it is only reachable via
/// the explicit `--dangerously-skip-permissions` flag.
#[derive(Clone, Copy, ValueEnum)]
pub enum ModeArg {
    Auto,
    Plan,
    AcceptEdits,
    Default,
    DontAsk,
}

impl From<ModeArg> for Mode {
    fn from(arg: ModeArg) -> Self {
        match arg {
            ModeArg::Auto => Mode::Auto,
            ModeArg::Plan => Mode::Plan,
            ModeArg::AcceptEdits => Mode::AcceptEdits,
            ModeArg::Default => Mode::Default,
            ModeArg::DontAsk => Mode::DontAsk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_flag_parses_short_and_long() {
        let cli = Cli::try_parse_from(["stepper", "-c"]).unwrap();
        assert!(cli.global.continue_session);
        let cli = Cli::try_parse_from(["stepper", "--continue"]).unwrap();
        assert!(cli.global.continue_session);
        let cli = Cli::try_parse_from(["stepper"]).unwrap();
        assert!(!cli.global.continue_session);
    }

    #[test]
    fn continue_conflicts_with_resume() {
        assert!(Cli::try_parse_from(["stepper", "-c", "--resume", "abc"]).is_err());
    }

    #[test]
    fn headless_print_accepts_resume_and_continue() {
        let cli = Cli::try_parse_from(["stepper", "-p", "do it", "--resume", "abc"]).unwrap();
        assert_eq!(cli.global.print.as_deref(), Some("do it"));
        assert_eq!(cli.global.resume.as_deref(), Some("abc"));
        let cli = Cli::try_parse_from(["stepper", "-p", "do it", "-c"]).unwrap();
        assert!(cli.global.continue_session);
    }

    #[test]
    fn name_flag_parses() {
        let cli = Cli::try_parse_from(["stepper", "--name", "spike"]).unwrap();
        assert_eq!(cli.global.name.as_deref(), Some("spike"));
    }

    #[test]
    fn no_init_flag_parses() {
        let cli = Cli::try_parse_from(["stepper", "--no-init"]).unwrap();
        assert!(cli.global.no_init);
        let cli = Cli::try_parse_from(["stepper"]).unwrap();
        assert!(!cli.global.no_init);
    }

    #[test]
    fn import_args_parse_positional_flag_and_defaults() {
        let cli = Cli::try_parse_from(["stepper", "import"]).unwrap();
        let Some(Command::Import(args)) = cli.command else {
            panic!("expected import command");
        };
        assert_eq!(args.resolved_source(), ImportSourceArg::All);
        assert!(!args.dry_run && !args.yes);

        // Positional form (matches the `/import claude` slash spelling).
        let cli = Cli::try_parse_from(["stepper", "import", "codex"]).unwrap();
        let Some(Command::Import(args)) = cli.command else {
            panic!("expected import command");
        };
        assert_eq!(args.resolved_source(), ImportSourceArg::Codex);

        // Cursor / Gemini sources parse too.
        for (arg, want) in [("cursor", ImportSourceArg::Cursor), ("gemini", ImportSourceArg::Gemini)] {
            let cli = Cli::try_parse_from(["stepper", "import", arg]).unwrap();
            let Some(Command::Import(args)) = cli.command else {
                panic!("expected import command");
            };
            assert_eq!(args.resolved_source(), want);
        }
    }

    #[test]
    fn config_set_get_parse() {
        let cli = Cli::try_parse_from(["stepper", "config", "set", "defaultModel", "openai/gpt-5"]).unwrap();
        let Some(Command::Config(args)) = cli.command else {
            panic!("expected config command");
        };
        match args.action {
            Some(ConfigAction::Set { key, value }) => {
                assert_eq!(key, "defaultModel");
                assert_eq!(value, "openai/gpt-5");
            }
            _ => panic!("expected a set action"),
        }

        let cli = Cli::try_parse_from(["stepper", "config", "get", "mode"]).unwrap();
        let Some(Command::Config(args)) = cli.command else {
            panic!("expected config command");
        };
        assert!(matches!(args.action, Some(ConfigAction::Get { key }) if key == "mode"));

        // The existing flags still parse with no action.
        let cli = Cli::try_parse_from(["stepper", "config", "--validate"]).unwrap();
        let Some(Command::Config(args)) = cli.command else {
            panic!("expected config command");
        };
        assert!(args.action.is_none() && args.validate);

        let cli = Cli::try_parse_from(["stepper", "config"]).unwrap();
        let Some(Command::Config(args)) = cli.command else {
            panic!("expected config command");
        };
        assert!(args.action.is_none() && !args.schema && !args.validate);

        // Flag form, plus dry-run.
        let cli = Cli::try_parse_from(["stepper", "import", "--from", "claude", "--dry-run"]).unwrap();
        let Some(Command::Import(args)) = cli.command else {
            panic!("expected import command");
        };
        assert_eq!(args.resolved_source(), ImportSourceArg::Claude);
        assert!(args.dry_run);

        // Positional wins over the flag.
        let cli = Cli::try_parse_from(["stepper", "import", "claude", "--from", "codex", "-y"]).unwrap();
        let Some(Command::Import(args)) = cli.command else {
            panic!("expected import command");
        };
        assert_eq!(args.resolved_source(), ImportSourceArg::Claude);
        assert!(args.yes);

        // clap rejects an unknown value (no longer a downstream string error).
        assert!(Cli::try_parse_from(["stepper", "import", "bogus"]).is_err());
    }
}
