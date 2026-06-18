use std::io::stdout;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::{DefaultTerminal, TerminalOptions, Viewport};

/// Set while the Kitty keyboard-enhancement flags are pushed, so the panic hook
/// (which has no handle to the guard) knows whether to pop them. Process-global
/// because there is only ever one terminal session.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// Pop the keyboard-enhancement flags if we pushed them. Idempotent.
fn pop_keyboard_enhancement() {
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
}

/// RAII wrapper around the inline-viewport terminal. `init_with_options` with a
/// `Viewport::Inline(N)` keeps the native terminal scrollback (no alternate
/// screen); finalized turns are pushed there via `Terminal::insert_before`.
///
/// On init we opt into the Kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES`
/// (when the terminal supports it) so modified keys like **Shift+Enter** arrive
/// as distinct `KeyEvent`s instead of being indistinguishable from a bare Enter.
/// Terminals without support (e.g. Apple Terminal) keep the legacy encoding and
/// fall back to the Alt+Enter / Ctrl+J bindings in `input.rs`.
///
/// A chained panic hook AND the `Drop` impl both pop the flags and call
/// `ratatui::restore()` so a return or a crash never leaves the user in raw mode
/// or with the enhancement flags still set.
pub struct TerminalGuard {
    pub terminal: DefaultTerminal,
}

impl TerminalGuard {
    pub fn new(inline_height: u16) -> Self {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(stdout(), DisableMouseCapture);
            pop_keyboard_enhancement();
            ratatui::restore();
            prev(info);
        }));
        let terminal = ratatui::init_with_options(TerminalOptions {
            viewport: Viewport::Inline(inline_height),
        });
        // Deliberately DO NOT enable mouse capture: with the inline viewport the
        // native terminal keeps its own scrollback and text selection, and
        // capturing the mouse would steal the wheel (no native scroll) and drag
        // (no select-to-copy). In-app live-region scrolling stays on PgUp/PgDn.
        if supports_keyboard_enhancement().unwrap_or(false)
            && execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        {
            KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
        }
        Self { terminal }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture);
        pop_keyboard_enhancement();
        ratatui::restore();
    }
}
