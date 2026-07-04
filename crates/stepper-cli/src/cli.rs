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
    /// An opening prompt: `stepper "fix the failing test"` starts the interactive
    /// session on that turn. Ignored when a subcommand or `-p` is given. Also
    /// filled from piped stdin (`echo ... | stepper`).
    #[arg(trailing_var_arg = true)]
    pub prompt: Vec<String>,
}

#[derive(Args, Clone)]
pub struct GlobalArgs {
    /// Override the active model, as `provider/model-id`.
    #[arg(long, global = true)]
    pub model: Option<String>,
    /// Fallback model chain (`provider/model-id`), tried in order when a step's
    /// primary model fails non-retryably or exhausts its retries. Comma-separated
    /// or repeated (`--fallback-model a,b` or `--fallback-model a --fallback-model
    /// b`); overrides `setting.json` `fallbackModel`. Capped at 3 links.
    #[arg(long, global = true, value_delimiter = ',')]
    pub fallback_model: Vec<String>,
    /// Start in this mode (auto | plan | accept-edits | default | dont-ask).
    #[arg(long, value_enum, global = true)]
    pub mode: Option<ModeArg>,
    /// Reasoning effort applied to every layer (off | low | medium | high | xhigh
    /// | max). Modern Claude maps it to adaptive thinking + `output_config.effort`;
    /// OpenAI to `reasoning_effort` (xhigh/max clamp to high); a layer's own
    /// `reasoning-effort` frontmatter overrides it.
    #[arg(long, global = true)]
    pub effort: Option<String>,
    /// Replace the project base context (the `CLAUDE.md`/`stepper.md` slot) with
    /// this text for the run. The universal agent directives and each layer's role
    /// are kept. Useful for headless wrappers that inject their own context.
    #[arg(long, global = true, conflicts_with = "system_prompt_file")]
    pub system_prompt: Option<String>,
    /// Like `--system-prompt`, but read the replacement text from a file.
    #[arg(long, global = true)]
    pub system_prompt_file: Option<PathBuf>,
    /// Append this text to every layer's system message (after its role). Useful
    /// for wrapping stepper as a headless linter/reviewer with extra instructions.
    #[arg(long, global = true, conflicts_with = "append_system_prompt_file")]
    pub append_system_prompt: Option<String>,
    /// Like `--append-system-prompt`, but read the appended text from a file.
    #[arg(long, global = true)]
    pub append_system_prompt_file: Option<PathBuf>,
    /// One-shot non-interactive prompt (no inline viewport). The value is
    /// optional: `-p` alone reads the prompt from piped stdin (`cat task.md |
    /// stepper -p`); `-p "text"` uses the text (and appends piped stdin if any).
    #[arg(short = 'p', long, global = true, num_args = 0..=1, default_missing_value = "")]
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
    /// Fork the resumed/continued session: continue under a new id, leaving the
    /// original untouched (no effect without `--resume`/`--continue`).
    #[arg(long, global = true)]
    pub fork: bool,
    /// (headless `-p`) Route the run to a named sub-agent (`.stepper/agents/<name>`),
    /// the same as prefixing the prompt with `#<name>`. An unknown name errors.
    #[arg(long, global = true)]
    pub agent: Option<String>,
    /// (headless `-p`) Attach file(s) — their text contents are inlined into the
    /// prompt (repeatable). Relative paths resolve against the working directory.
    #[arg(long, global = true)]
    pub file: Vec<PathBuf>,
    /// (headless `-p`) Output format: `text` (default, streams the reply) or
    /// `json` (one JSON event per line — tool calls, the final text, done/error).
    #[arg(long, value_enum, global = true)]
    pub format: Option<OutputFormat>,
    /// (headless `-p`) Require the final reply to be JSON matching this JSON
    /// Schema — an inline schema string or a path to a `.json` schema file. On a
    /// mismatch the run re-prompts the model with the validation error (up to
    /// `--output-schema-retries` times) and exits non-zero if it never conforms.
    #[arg(long, global = true)]
    pub output_schema: Option<String>,
    /// (headless `-p`) How many times to re-prompt when the reply fails the
    /// `--output-schema` (default 2).
    #[arg(long, global = true, default_value_t = 2)]
    pub output_schema_retries: u32,
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
    /// Skip the first-run guided setup that offers to scaffold `.stepper/` when no
    /// project config is found (also via the `STEPPER_NO_INIT` env var — resolved
    /// with presence semantics in `launch`, NOT bound to clap's bool `env` which
    /// would reject any value other than "true"/"false" and abort every command).
    #[arg(long, global = true)]
    pub no_init: bool,
    /// Write logs to `~/.stepper/logs/stepper.log` at this level (off | error |
    /// warn | info | debug | trace). Off by default — nothing is written unless
    /// this is set. The interactive TUI always logs to the file only (never the
    /// screen); see `--print-logs` for headless runs.
    #[arg(long, value_enum, global = true)]
    pub log_level: Option<LogLevelArg>,
    /// (headless `-p`) Also stream logs to stderr, in addition to the log file.
    /// Ignored in the interactive TUI, where stderr would corrupt the viewport.
    #[arg(long, global = true)]
    pub print_logs: bool,
}

/// Log verbosity for `--log-level`. `Off` installs no subscriber (the default),
/// so the `tracing` macros throughout the crates stay no-ops with zero overhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LogLevelArg {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevelArg {
    /// The `EnvFilter` directive (also a valid `tracing` level name).
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevelArg::Off => "off",
            LogLevelArg::Error => "error",
            LogLevelArg::Warn => "warn",
            LogLevelArg::Info => "info",
            LogLevelArg::Debug => "debug",
            LogLevelArg::Trace => "trace",
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Launch the interactive TUI (this is also the default with no subcommand).
    Run,
    /// Manage provider credentials, e.g. `stepper auth login --codex`.
    Auth(AuthArgs),
    /// Inspect or validate `.stepper/setting.json`.
    Config(ConfigArgs),
    /// Run a full health check: config, provider keys, MCP servers, the models
    /// catalog, and the latest release (`stepper doctor`).
    Doctor,
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
    /// Manage saved sessions: `stepper session list` / `session delete <id>`.
    Session(SessionArgs),
    /// Manage MCP server OAuth: `stepper mcp auth <name>` / `logout <name>` / `status`.
    Mcp(McpArgs),
    /// Cross-session token & cost statistics: `stepper stats`.
    Stats(StatsArgs),
    /// List models across configured providers: `stepper models [provider]`.
    Models(ModelsArgs),
}

#[derive(Args)]
pub struct ModelsArgs {
    /// Only list models from this provider id (the `provider/` prefix).
    pub provider: Option<String>,
    /// Emit the model entries as JSON instead of the plain `provider/model` list.
    #[arg(long)]
    pub json: bool,
    /// Append each model's context window, max output, and per-Mtok pricing.
    #[arg(long)]
    pub verbose: bool,
    /// Accepted for `opencode models` parity. stepper keeps no on-disk catalog
    /// cache, so models are always fetched fresh — this flag is a no-op.
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Args)]
pub struct StatsArgs {
    /// Only count turns from the last N days (turns without a timestamp — saved
    /// before stats tracking — are excluded once this is set).
    #[arg(long)]
    pub days: Option<u64>,
    /// Emit the full `SessionStats` as JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
    /// Include the per-model breakdown in the human summary.
    #[arg(long)]
    pub models: bool,
    /// Include the top-N tools by call count in the human summary.
    #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "10")]
    pub tools: Option<usize>,
    /// Write the stats to a file instead of stdout. The format is taken from the
    /// extension: `.csv` → CSV (per-model table), anything else → JSON.
    #[arg(long, value_name = "PATH")]
    pub export: Option<std::path::PathBuf>,
}

#[derive(Args)]
pub struct McpArgs {
    #[command(subcommand)]
    pub cmd: McpCmd,
}

#[derive(Subcommand)]
pub enum McpCmd {
    /// List the MCP servers configured in `setting.json`.
    List,
    /// Show one server's config and connect to list its tools/resources/prompts.
    Get {
        /// The server name.
        name: String,
    },
    /// Add an MCP server to `setting.json` (stdio via `--command`, http via `--url`).
    Add {
        /// The server name (the `mcpServers` key).
        name: String,
        /// stdio command to launch (the program; pass arguments with `--arg`).
        #[arg(long, conflicts_with = "url")]
        command: Option<String>,
        /// An argument for the stdio `--command` (repeatable).
        #[arg(long = "arg")]
        args: Vec<String>,
        /// http(s) URL of a remote server (sets `"type": "http"`).
        #[arg(long)]
        url: Option<String>,
    },
    /// Remove an MCP server from `setting.json`.
    Remove {
        /// The server name.
        name: String,
    },
    /// Authorize an OAuth MCP server in the browser and store its tokens.
    Auth {
        /// The server name from `mcpServers` in `.stepper/setting.json`.
        name: String,
    },
    /// Drop a server's stored OAuth tokens.
    Logout {
        /// The server name.
        name: String,
    },
    /// Show OAuth status for every OAuth-capable configured server.
    Status,
}

#[derive(Args)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub cmd: SessionCmd,
}

#[derive(Subcommand)]
pub enum SessionCmd {
    /// List saved sessions for this project, newest first.
    List {
        /// Show only the N most recent (default: all).
        #[arg(short = 'n', long)]
        limit: Option<usize>,
        /// Emit JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Delete a saved session by id (see `session list`).
    Delete {
        id: String,
    },
    /// Rename a saved session (the name shown by session pickers).
    Rename {
        /// The session id (see `session list`).
        id: String,
        /// The new name.
        name: String,
    },
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

/// `--format` choices for a headless `-p` run: stream the assistant text
/// (default) or emit one JSON event per line (stepper's own minimal schema).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
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
    fn stats_args_parse() {
        // Bare `stats` → all defaults.
        let cli = Cli::try_parse_from(["stepper", "stats"]).unwrap();
        match cli.command {
            Some(Command::Stats(a)) => {
                assert_eq!(a.days, None);
                assert!(!a.json && !a.models);
                assert_eq!(a.tools, None);
                assert!(a.export.is_none());
            }
            _ => panic!("expected stats"),
        }
        // Flags: --days, --json, --models, --tools (with and without N), --export/--format.
        let cli = Cli::try_parse_from(["stepper", "stats", "--days", "7", "--json", "--models"]).unwrap();
        match cli.command {
            Some(Command::Stats(a)) => {
                assert_eq!(a.days, Some(7));
                assert!(a.json && a.models);
            }
            _ => panic!("expected stats"),
        }
        // `--tools` alone defaults to 10; `--tools N` takes the value.
        let cli = Cli::try_parse_from(["stepper", "stats", "--tools"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Stats(a)) if a.tools == Some(10)));
        let cli = Cli::try_parse_from(["stepper", "stats", "--tools", "3"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Stats(a)) if a.tools == Some(3)));
        let cli = Cli::try_parse_from(["stepper", "stats", "--export", "/tmp/s.csv"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Stats(a)) if a.export.as_deref() == Some(std::path::Path::new("/tmp/s.csv"))));
    }

    #[test]
    fn models_args_parse() {
        // Bare `models` → no provider filter, plain output.
        let cli = Cli::try_parse_from(["stepper", "models"]).unwrap();
        match cli.command {
            Some(Command::Models(a)) => {
                assert_eq!(a.provider, None);
                assert!(!a.json && !a.verbose && !a.refresh);
            }
            _ => panic!("expected models"),
        }
        // Positional provider filter + flags.
        let cli = Cli::try_parse_from(["stepper", "models", "anthropic", "--json", "--verbose"]).unwrap();
        match cli.command {
            Some(Command::Models(a)) => {
                assert_eq!(a.provider.as_deref(), Some("anthropic"));
                assert!(a.json && a.verbose);
            }
            _ => panic!("expected models"),
        }
        // `--refresh` is accepted (no-op for parity).
        let cli = Cli::try_parse_from(["stepper", "models", "--refresh"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Models(a)) if a.refresh && a.provider.is_none()));
    }

    #[test]
    fn log_flags_parse_and_default_off() {
        // Default: no logging.
        let cli = Cli::try_parse_from(["stepper"]).unwrap();
        assert_eq!(cli.global.log_level, None);
        assert!(!cli.global.print_logs);
        // `--log-level` + `--print-logs` (global, so they attach to any subcommand).
        let cli = Cli::try_parse_from(["stepper", "--log-level", "debug", "--print-logs", "-p", "hi"]).unwrap();
        assert_eq!(cli.global.log_level, Some(LogLevelArg::Debug));
        assert!(cli.global.print_logs);
        // Level → EnvFilter directive string.
        assert_eq!(LogLevelArg::Off.as_str(), "off");
        assert_eq!(LogLevelArg::Trace.as_str(), "trace");
    }

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
    fn session_subcommands_parse() {
        let cli = Cli::try_parse_from(["stepper", "session", "list"]).unwrap();
        let Some(Command::Session(args)) = cli.command else { panic!("expected session command") };
        assert!(matches!(args.cmd, SessionCmd::List { limit: None, json: false }));

        let cli = Cli::try_parse_from(["stepper", "session", "list", "-n", "5", "--json"]).unwrap();
        let Some(Command::Session(args)) = cli.command else { panic!("expected session command") };
        assert!(matches!(args.cmd, SessionCmd::List { limit: Some(5), json: true }));

        let cli = Cli::try_parse_from(["stepper", "session", "delete", "abc"]).unwrap();
        let Some(Command::Session(args)) = cli.command else { panic!("expected session command") };
        assert!(matches!(args.cmd, SessionCmd::Delete { id } if id == "abc"));

        let cli = Cli::try_parse_from(["stepper", "session", "rename", "abc", "My Work"]).unwrap();
        let Some(Command::Session(args)) = cli.command else { panic!("expected session command") };
        assert!(matches!(args.cmd, SessionCmd::Rename { id, name } if id == "abc" && name == "My Work"));
    }

    #[test]
    fn fork_flag_parses_and_defaults_off() {
        let cli = Cli::try_parse_from(["stepper", "--resume", "abc", "--fork"]).unwrap();
        assert!(cli.global.fork);
        assert_eq!(cli.global.resume.as_deref(), Some("abc"));
        assert!(!Cli::try_parse_from(["stepper"]).unwrap().global.fork);
    }

    #[test]
    fn agent_flag_parses() {
        let cli = Cli::try_parse_from(["stepper", "-p", "do it", "--agent", "reviewer"]).unwrap();
        assert_eq!(cli.global.agent.as_deref(), Some("reviewer"));
        assert!(Cli::try_parse_from(["stepper"]).unwrap().global.agent.is_none());
    }

    #[test]
    fn file_flag_parses_repeatable() {
        let cli = Cli::try_parse_from(["stepper", "-p", "x", "--file", "a.txt", "--file", "b.rs"]).unwrap();
        assert_eq!(cli.global.file, vec![PathBuf::from("a.txt"), PathBuf::from("b.rs")]);
        assert!(Cli::try_parse_from(["stepper"]).unwrap().global.file.is_empty());
    }

    #[test]
    fn format_flag_parses() {
        let cli = Cli::try_parse_from(["stepper", "-p", "x", "--format", "json"]).unwrap();
        assert_eq!(cli.global.format, Some(OutputFormat::Json));
        assert!(Cli::try_parse_from(["stepper"]).unwrap().global.format.is_none());
    }

    #[test]
    fn output_schema_flags_parse_with_default_retries() {
        let cli = Cli::try_parse_from(["stepper", "-p", "x", "--output-schema", "{\"type\":\"object\"}"]).unwrap();
        assert_eq!(cli.global.output_schema.as_deref(), Some("{\"type\":\"object\"}"));
        assert_eq!(cli.global.output_schema_retries, 2, "default retries");
        let cli = Cli::try_parse_from(["stepper", "-p", "x", "--output-schema", "s.json", "--output-schema-retries", "5"]).unwrap();
        assert_eq!(cli.global.output_schema_retries, 5);
        assert!(Cli::try_parse_from(["stepper"]).unwrap().global.output_schema.is_none());
    }

    #[test]
    fn doctor_command_parses() {
        let cli = Cli::try_parse_from(["stepper", "doctor"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Doctor)));
    }

    #[test]
    fn fallback_model_parses_comma_and_repeat_into_a_chain() {
        // Comma-separated → ordered chain.
        let cli = Cli::try_parse_from(["stepper", "--fallback-model", "p/a,p/b"]).unwrap();
        assert_eq!(cli.global.fallback_model, vec!["p/a", "p/b"]);
        // Repeated flag accumulates too.
        let cli = Cli::try_parse_from(["stepper", "--fallback-model", "p/a", "--fallback-model", "p/b"]).unwrap();
        assert_eq!(cli.global.fallback_model, vec!["p/a", "p/b"]);
        // Absent → empty.
        assert!(Cli::try_parse_from(["stepper"]).unwrap().global.fallback_model.is_empty());
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
