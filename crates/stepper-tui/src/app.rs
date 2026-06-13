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
                        handle_terminal_event(&mut state, &action_tx, ev);
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

fn handle_terminal_event(state: &mut AppState, action_tx: &ActionTx, ev: Event) {
    if let Event::Key(k) = &ev
        && k.kind != KeyEventKind::Press
    {
        return;
    }

    // The @-file picker, while open, captures all keys.
    if state.picker.is_some() {
        handle_picker_key(state, &ev);
        return;
    }

    // The builtin overlays (context/permissions/rewind/resume picker) capture
    // keys; the approval overlay keeps its y/a/n path through lower_event.
    if state.overlay_captures_keys() {
        handle_overlay_key(state, action_tx, &ev);
        return;
    }

    // Typing '@' at a word boundary opens the file picker (the filesystem scan
    // happens here, IO, so state.rs stays pure). Mid-word '@' (e.g. an email)
    // falls through and is inserted literally.
    if let Event::Key(k) = &ev
        && let KeyCode::Char('@') = k.code
        && at_word_boundary(state)
    {
        open_or_refresh_picker(state, String::new());
        return;
    }

    // The slash-command palette (input is a `/<partial>` token) captures
    // navigation + Tab-completion; everything else falls through so typing keeps
    // narrowing the matches and Enter still submits the command.
    if state.palette_active()
        && let Event::Key(k) = &ev
    {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Up => {
                state.palette_move(-1);
                return;
            }
            KeyCode::Down => {
                state.palette_move(1);
                return;
            }
            KeyCode::Tab if !shift => {
                state.palette_complete();
                return;
            }
            _ => {}
        }
    }

    match lower_event(&ev, state) {
        Lowered::Action(action) => {
            for eff in state.apply_action(action) {
                if let Effect::Send(forward) = eff {
                    let _ = action_tx.try_send(forward);
                }
            }
        }
        Lowered::ForwardToTextarea => {
            state.esc_armed = false; // typing breaks the Esc-Esc chord
            if let Event::Key(k) = ev {
                state.textarea.input(k);
            }
        }
        Lowered::Ignore => {}
    }
}

/// Keys for the builtin overlays: the list picker navigates/selects/cancels,
/// the info panels (context/permissions) dismiss on Esc/Enter/q.
fn handle_overlay_key(state: &mut AppState, action_tx: &ActionTx, ev: &Event) {
    use crate::input::{lower_picker_nav, PickerNav};
    use crate::state::Overlay;
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

fn handle_picker_key(state: &mut AppState, ev: &Event) {
    let Event::Key(k) = ev else {
        return;
    };
    match k.code {
        KeyCode::Up => state.picker_move(-1),
        KeyCode::Down => state.picker_move(1),
        KeyCode::Enter | KeyCode::Tab => match state.picker_selection() {
            Some(Selection::Navigate(query)) => open_or_refresh_picker(state, query),
            Some(Selection::Insert(path)) => state.insert_picker_path(&path),
            None => {}
        },
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
                let text = tui_markdown::from_str(&md);
                let height = (text.lines.len() as u16).max(1);
                terminal.insert_before(height, |buf| {
                    Paragraph::new(text).render(buf.area, buf);
                })?;
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
