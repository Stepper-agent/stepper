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
}

/// A slash command shown in the `/` palette: its name and a one-line description.
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
}

#[cfg(test)]
impl CommandInfo {
    /// A descriptionless command, for test fixtures.
    pub fn named(name: &str) -> Self {
        CommandInfo {
            name: name.to_string(),
            description: String::new(),
        }
    }
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
