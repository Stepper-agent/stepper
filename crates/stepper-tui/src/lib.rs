//! `stepper-tui` — ratatui inline-viewport TUI.
//!
//! INVARIANT: depends only on `stepper-protocol` (+ the ratatui stack). It talks
//! to the agent core exclusively by receiving `AppEvent`s and emitting `Action`s
//! over channels, so it knows nothing about how a turn actually runs.

mod app;
mod files;
mod input;
mod markdown;
mod render;
mod state;
mod terminal;
mod theme;

pub mod mock;

use std::path::PathBuf;
use stepper_protocol::{ActionTx, EventRx, Mode, ModelView};
use tokio_util::sync::CancellationToken;

pub use mock::spawn_fake_core;

/// Initial render configuration handed to the TUI.
pub struct TuiInit {
    pub inline_height: u16,
    pub model: ModelView,
    pub mode: Mode,
    pub cwd: PathBuf,
    /// Known slash commands (name + one-line description) for the `/` palette.
    pub commands: Vec<CommandInfo>,
    /// Named sub-agents (`.stepper/agents/`) for the `#`-agent autocomplete picker.
    /// Empty disables the picker (a literal `#` is typed instead).
    pub agents: Vec<AgentInfo>,
    /// Color-theme preset name (`None` → `dark`) loaded from settings.
    pub theme_preset: Option<String>,
    /// Per-role color overrides (`name`, `color-string`) loaded from settings.
    pub theme_colors: Vec<(String, String)>,
    /// Session reasoning-effort level loaded from settings (`None` = off), shown
    /// in the status footer.
    pub effort: Option<String>,
    /// Ring the terminal bell when a turn completes (`notification` setting).
    pub notify_on_complete: bool,
    /// Ring the terminal bell when an approval prompt is surfaced.
    pub notify_on_approval: bool,
    /// Ring the terminal bell when a turn errors.
    pub notify_on_error: bool,
    /// Where to persist the prompt history (`~/.stepper/history/<project>.json`).
    /// `None` keeps history in memory only (no `$HOME`). Loaded by the event loop.
    pub history_path: Option<PathBuf>,
    /// Custom status-line command (program + args) from `settings.statusLine`.
    /// `None` = the built-in footer. The event loop runs it with a JSON context
    /// on stdin and renders its first stdout line in place of the footer.
    pub status_line_cmd: Option<Vec<String>>,
}

/// A slash command shown in the `/` palette: its name, a one-line description,
/// and an optional `argument-hint` (e.g. `<pr-number>`) rendered next to the name.
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
}

#[cfg(test)]
impl CommandInfo {
    /// A descriptionless command, for test fixtures.
    pub fn named(name: &str) -> Self {
        CommandInfo {
            name: name.to_string(),
            description: String::new(),
            argument_hint: None,
        }
    }
}

/// A named sub-agent shown in the `#`-agent autocomplete picker: its name (what
/// `#name` selects) and a one-line description.
#[derive(Clone, Debug)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
}

/// Run the interactive inline-viewport TUI until the user quits or `cancel` fires.
pub async fn run_tui(
    event_rx: EventRx,
    action_tx: ActionTx,
    init: TuiInit,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    app::run(event_rx, action_tx, init, cancel).await
}
