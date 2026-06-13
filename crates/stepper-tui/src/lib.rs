//! `stepper-tui` — ratatui inline-viewport TUI.
//!
//! INVARIANT: depends only on `stepper-protocol` (+ the ratatui stack). It talks
//! to the agent core exclusively by receiving `AppEvent`s and emitting `Action`s
//! over channels, so it knows nothing about how a turn actually runs.

mod app;
mod files;
mod input;
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
    /// Known slash-command names for the `/` palette.
    pub commands: Vec<String>,
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
