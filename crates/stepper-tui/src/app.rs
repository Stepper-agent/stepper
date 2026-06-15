use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::{Paragraph, Widget};
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use stepper_protocol::{ActionTx, EventRx};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::TuiInit;
use crate::input::{Lowered, lower_event};
use crate::render::draw;
use crate::state::{AppState, Effect, Selection};
use crate::terminal::TerminalGuard;
use crate::theme::Theme;

/// The single `tokio::select!` event loop. Multiplexes terminal input, a render
/// tick, the core->TUI `AppEvent` channel, and a cancellation token. Rendering
/// happens on the tick (coalescing bursty token deltas), never per delta.
///
/// Input is read on a dedicated blocking thread (`event::poll`/`event::read`)
/// rather than crossterm's async `EventStream`. EventStream races with the
/// cursor-position query (`ESC[6n`) that ratatui's inline viewport issues on
/// resize, eating the response and producing "cursor position could not be
/// read". The blocking path shares crossterm's internal event reader, so it
/// coordinates with `position()` instead of fighting it.
pub async fn run(
    mut event_rx: EventRx,
    action_tx: ActionTx,
    init: TuiInit,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let mut guard = TerminalGuard::new(init.inline_height);
    let theme = Theme::default();
    let mut state = AppState::new(init);

    let running = Arc::new(AtomicBool::new(true));
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(256);
    let reader = {
        let running = running.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if input_tx.blocking_send(ev).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        })
    };

    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let mut dirty = true;
    // Force a full repaint on the next draw. Needed because overwriting a wide
    // (CJK) glyph with narrower content leaves its right half on screen — a
    // ratatui inline-viewport + wide-char diff limitation. We only force it when
    // the input actually shrinks past a wide char (not on plain ASCII edits, so
    // there's no flicker) and after a scrollback commit.
    let mut force_clear = false;
    let mut last_input_w = 0usize;
    draw(&mut guard.terminal, &state, &theme)?;

    loop {
        if state.should_quit {
            break;
        }
        tokio::select! {
            maybe_ev = input_rx.recv() => {
                match maybe_ev {
                    Some(ev) => {
                        // An action can now commit to scrollback (e.g. echoing the
                        // submitted prompt), so route its effects through the same
                        // `run_effects` the core-event arm uses.
                        if handle_terminal_event(&mut guard.terminal, &mut state, &action_tx, ev)? {
                            force_clear = true;
                        }
                        dirty = true;
                    }
                    None => break, // reader thread ended
                }
            }
            _ = tick.tick() => {
                let iw = input_width(&state);
                if last_input_w >= iw + 2 {
                    force_clear = true; // a wide glyph was removed
                }
                last_input_w = iw;
                if state.turn_active {
                    state.spinner = state.spinner.wrapping_add(1);
                    dirty = true;
                }
                if dirty {
                    if force_clear {
                        let _ = guard.terminal.clear();
                        force_clear = false;
                    }
                    draw(&mut guard.terminal, &state, &theme)?;
                    dirty = false;
                }
            }
            maybe = event_rx.recv() => {
                if let Some(app_ev) = maybe {
                    let effects = state.apply_event(app_ev);
                    if run_effects(&mut guard.terminal, &action_tx, effects)? {
                        force_clear = true; // resync the viewport after insert_before
                    }
                    dirty = true;
                }
            }
            _ = cancel.cancelled() => break,
        }
    }

    running.store(false, Ordering::Relaxed);
    drop(input_rx);
    let _ = reader.join();
    Ok(())
}

/// Handle one terminal event. Returns `Ok(true)` if anything was committed to
/// scrollback (so the caller forces a viewport resync), like `run_effects`.
fn handle_terminal_event(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut AppState,
    action_tx: &ActionTx,
    ev: Event,
) -> anyhow::Result<bool> {
    if let Event::Key(k) = &ev
        && k.kind != KeyEventKind::Press
    {
        return Ok(false);
    }

    // The @-file picker, while open, captures all keys.
    if state.picker.is_some() {
        handle_picker_key(state, &ev);
        return Ok(false);
    }

    // The builtin overlays (context/permissions/rewind/resume picker) capture
    // keys; the approval overlay keeps its y/a/n path through lower_event.
    if state.overlay_captures_keys() {
        handle_overlay_key(state, action_tx, &ev);
        return Ok(false);
    }

    // Typing '@' at a word boundary opens the file picker (the filesystem scan
    // happens here, IO, so state.rs stays pure). Mid-word '@' (e.g. an email)
    // falls through and is inserted literally.
    if let Event::Key(k) = &ev
        && let KeyCode::Char('@') = k.code
        && at_word_boundary(state)
    {
        open_or_refresh_picker(state, String::new());
        return Ok(false);
    }

    // The slash-command palette (input is a `/<partial>` token) captures
    // navigation + Tab-completion; everything else falls through so typing keeps
    // narrowing the matches and Enter still submits the command.
    if state.palette_active()
        && let Event::Key(k) = &ev
    {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        match k.code {
            KeyCode::Up => {
                state.palette_move(-1);
                return Ok(false);
            }
            KeyCode::Down => {
                state.palette_move(1);
                return Ok(false);
            }
            KeyCode::Tab if !shift => {
                state.palette_complete();
                return Ok(false);
            }
            // Enter runs the highlighted command immediately (Shift/Alt+Enter stay
            // newline inserts, handled by lower_event below). Tab still completes
            // so the user can add arguments before running.
            KeyCode::Enter if !shift && !alt => {
                return run_effects(terminal, action_tx, state.palette_run_selected());
            }
            _ => {}
        }
    }

    // Down on an empty prompt opens the background-process shell view (Claude
    // Code style) when any process is tracked. With text in the box, Down keeps
    // moving the textarea cursor.
    if let Event::Key(k) = &ev
        && k.code == KeyCode::Down
        && state.input_text().is_empty()
        && !state.processes.is_empty()
    {
        state.open_shell_view();
        return Ok(false);
    }

    // Ctrl+V pastes an image from the OS clipboard (macOS Cmd+V is intercepted by
    // the terminal, so Ctrl+V is the paste key inside the app). The image is
    // staged for the next prompt; a non-image clipboard falls through.
    if let Event::Key(k) = &ev
        && k.code == KeyCode::Char('v')
        && k.modifiers.contains(KeyModifiers::CONTROL)
    {
        paste_clipboard_image(state, action_tx);
        return Ok(false);
    }

    match lower_event(&ev, state) {
        // An action's effects can include a scrollback commit (the prompt echo),
        // so run them through `run_effects` rather than only forwarding sends.
        Lowered::Action(action) => {
            let effects = state.apply_action(action);
            return run_effects(terminal, action_tx, effects);
        }
        Lowered::ForwardToTextarea => {
            state.esc_armed = false; // typing breaks the Esc-Esc chord
            if let Event::Key(k) = ev {
                state.textarea.input(k);
            }
        }
        Lowered::Ignore => {}
    }
    Ok(false)
}

/// Keys for the builtin overlays: the list picker navigates/selects/cancels,
/// the info panels (context/permissions) dismiss on Esc/Enter/q.
fn handle_overlay_key(state: &mut AppState, action_tx: &ActionTx, ev: &Event) {
    use crate::input::{lower_picker_nav, PickerNav};
    use crate::state::Overlay;
    // The API-key overlay is a masked text field: type chars, Enter saves, Esc
    // cancels.
    if matches!(state.overlay, Some(Overlay::ApiKey(_))) {
        if let Event::Key(k) = ev {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            match k.code {
                KeyCode::Enter => {
                    for eff in state.api_key_submit() {
                        if let Effect::Send(action) = eff {
                            let _ = action_tx.try_send(action);
                        }
                    }
                }
                // Esc and Ctrl+C both cancel (Ctrl+C must not be typed into the
                // key as a literal 'c').
                KeyCode::Esc => state.overlay_close(),
                KeyCode::Char('c') if ctrl => state.overlay_close(),
                KeyCode::Backspace => state.api_key_backspace(),
                // Only insert printable chars typed without ctrl/alt.
                KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                    state.api_key_push(c)
                }
                _ => {}
            }
        }
        return;
    }
    if matches!(state.overlay, Some(Overlay::Picker(_))) {
        match lower_picker_nav(ev) {
            Some(PickerNav::Up) => state.overlay_picker_move(-1),
            Some(PickerNav::Down) => state.overlay_picker_move(1),
            Some(PickerNav::Select) => {
                for eff in state.overlay_picker_select() {
                    if let Effect::Send(action) = eff {
                        let _ = action_tx.try_send(action);
                    }
                }
            }
            Some(PickerNav::Cancel) => state.overlay_close(),
            None => {}
        }
        return;
    }
    // The background-process shell view: ↑↓ select, `k` kills, Esc/q closes.
    if matches!(state.overlay, Some(Overlay::Shell(_))) {
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Up => state.shell_move(-1),
                KeyCode::Down => state.shell_move(1),
                KeyCode::Char('k') => {
                    for eff in state.shell_kill_selected() {
                        if let Effect::Send(action) = eff {
                            let _ = action_tx.try_send(action);
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => state.overlay_close(),
                _ => {}
            }
        }
        return;
    }
    if let Event::Key(k) = ev
        && matches!(
            k.code,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')
        )
    {
        state.overlay_close();
    }
}

fn at_word_boundary(state: &AppState) -> bool {
    let text = state.input_text();
    text.is_empty() || text.ends_with(char::is_whitespace)
}

/// Read a PNG image off the OS clipboard, stage it for the next prompt (sending
/// it to core via `AttachImage`), and bump the input indicator. A non-image
/// clipboard is a quiet no-op; an error surfaces as a notice.
fn paste_clipboard_image(state: &mut AppState, action_tx: &ActionTx) {
    match read_clipboard_image() {
        Ok(Some((media_type, data))) => {
            state.pending_image_count += 1;
            let n = state.pending_image_count;
            state.notice = Some(format!(
                "image attached ({n} pending) — it rides with your next message"
            ));
            let _ = action_tx.try_send(stepper_protocol::Action::AttachImage { media_type, data });
        }
        Ok(None) => {}
        Err(e) => state.notice = Some(format!("clipboard image paste failed: {e}")),
    }
}

/// `(media_type, base64)` of the clipboard image, or `None` if the clipboard
/// holds no image. PNG-encoded from the raw RGBA arboard hands back.
fn read_clipboard_image() -> Result<Option<(String, String)>, String> {
    use base64::Engine;
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = match clipboard.get_image() {
        Ok(img) => img,
        Err(_) => return Ok(None),
    };
    let (w, h) = (img.width as u32, img.height as u32);
    let rgba = image::RgbaImage::from_raw(w, h, img.bytes.into_owned())
        .ok_or("clipboard image had an unexpected byte length")?;
    let mut png: Vec<u8> = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);
    Ok(Some(("image/png".to_string(), data)))
}

fn handle_picker_key(state: &mut AppState, ev: &Event) {
    let Event::Key(k) = ev else {
        return;
    };
    match k.code {
        KeyCode::Up => state.picker_move(-1),
        KeyCode::Down => state.picker_move(1),
        // Tab = drill / autocomplete: a directory re-lists deeper, a file inserts.
        KeyCode::Tab => match state.picker_selection() {
            Some(Selection::Navigate(query)) => open_or_refresh_picker(state, query),
            Some(Selection::Insert(path)) => state.insert_picker_path(&path),
            None => {}
        },
        // Enter = commit the highlighted path (directory OR file) and exit @-mode,
        // inserting `@path ` so the user is never trapped drilling into folders.
        KeyCode::Enter => {
            if let Some(path) = state.picker_commit() {
                state.insert_picker_path(&path);
            }
        }
        KeyCode::Esc => state.picker_cancel(),
        KeyCode::Backspace => {
            let mut query = state.picker_query().unwrap_or_default().to_string();
            if query.pop().is_none() {
                state.picker_cancel();
            } else {
                open_or_refresh_picker(state, query);
            }
        }
        KeyCode::Char(c) => {
            let mut query = state.picker_query().unwrap_or_default().to_string();
            query.push(c);
            open_or_refresh_picker(state, query);
        }
        _ => {}
    }
}

/// List the directory the query resolves to (IO) and rebuild the picker. The
/// query may be relative (against cwd), absolute (`/…`), or `~/…`.
fn open_or_refresh_picker(state: &mut AppState, query: String) {
    let (dir, filter) = split_query(&query, &state.cwd);
    let candidates = crate::files::list_dir(&dir, 500);
    state.set_picker(query, candidates, &filter);
}

fn split_query(query: &str, cwd: &Path) -> (PathBuf, String) {
    match query.rfind('/') {
        Some(idx) => {
            let dir_part = &query[..=idx];
            let filter = query[idx + 1..].to_string();
            (resolve_dir(dir_part, cwd), filter)
        }
        None => (cwd.to_path_buf(), query.to_string()),
    }
}

fn resolve_dir(dir_part: &str, cwd: &Path) -> PathBuf {
    if let Some(rest) = dir_part.strip_prefix("~/") {
        home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(dir_part))
    } else if dir_part == "~/" {
        home_dir().unwrap_or_else(|| PathBuf::from(dir_part))
    } else if dir_part.starts_with('/') {
        PathBuf::from(dir_part)
    } else {
        cwd.join(dir_part)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Returns `true` if anything was committed to scrollback (so the caller can
/// force a viewport repaint).
fn run_effects(
    terminal: &mut ratatui::DefaultTerminal,
    action_tx: &ActionTx,
    effects: crate::state::Effects,
) -> anyhow::Result<bool> {
    let mut committed = false;
    for eff in effects {
        match eff {
            Effect::Send(action) => {
                let _ = action_tx.try_send(action);
            }
            Effect::CommitToScrollback(md) => {
                let text = crate::markdown::render_markdown(&md);
                let height = (text.lines.len() as u16).max(1);
                terminal.insert_before(height, |buf| {
                    Paragraph::new(text).render(buf.area, buf);
                })?;
                committed = true;
            }
            Effect::ClearScreen => {
                // Purge scrollback + clear the screen so the prior conversation
                // disappears; `committed` forces a fresh viewport repaint after.
                use crossterm::terminal::{Clear, ClearType};
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    Clear(ClearType::Purge),
                    Clear(ClearType::All),
                    crossterm::cursor::MoveTo(0, 0),
                );
                committed = true;
            }
        }
    }
    Ok(committed)
}

fn input_width(state: &AppState) -> usize {
    state
        .input_text()
        .lines()
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ApiKeyOverlay, Overlay};
    use crossterm::event::KeyEvent;
    use stepper_protocol::{Action, Mode, ModelView};

    fn state_with_api_key_overlay() -> AppState {
        let mut s = AppState::new(crate::TuiInit {
            inline_height: 10,
            model: ModelView { provider: "p".into(), model: "m".into() },
            mode: Mode::Auto,
            cwd: PathBuf::from("/tmp"),
            commands: vec![],
        });
        s.overlay = Some(Overlay::ApiKey(ApiKeyOverlay {
            provider: "anthropic".into(),
            input: String::new(),
        }));
        s
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn api_key_overlay_types_plain_chars_but_ctrl_c_cancels() {
        let mut s = state_with_api_key_overlay();
        let (tx, _rx) = mpsc::channel::<Action>(8);
        // A plain char is typed into the (masked) key.
        handle_overlay_key(&mut s, &tx, &key(KeyCode::Char('k'), KeyModifiers::NONE));
        match &s.overlay {
            Some(Overlay::ApiKey(o)) => assert_eq!(o.input, "k"),
            _ => panic!("char should type into the api-key overlay"),
        }
        // Ctrl+C cancels the overlay instead of appending a literal 'c'.
        handle_overlay_key(&mut s, &tx, &key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(s.overlay.is_none(), "Ctrl+C must cancel, not type 'c'");
    }
}
