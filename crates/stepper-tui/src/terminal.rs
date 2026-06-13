use ratatui::{DefaultTerminal, TerminalOptions, Viewport};

/// RAII wrapper around the inline-viewport terminal. `init_with_options` with a
/// `Viewport::Inline(N)` keeps the native terminal scrollback (no alternate
/// screen); finalized turns are pushed there via `Terminal::insert_before`.
///
/// A chained panic hook AND the `Drop` impl both call `ratatui::restore()` so a
/// return or a crash never leaves the user in raw mode.
pub struct TerminalGuard {
    pub terminal: DefaultTerminal,
}

impl TerminalGuard {
    pub fn new(inline_height: u16) -> Self {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            ratatui::restore();
            prev(info);
        }));
        let terminal = ratatui::init_with_options(TerminalOptions {
            viewport: Viewport::Inline(inline_height),
        });
        Self { terminal }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}
