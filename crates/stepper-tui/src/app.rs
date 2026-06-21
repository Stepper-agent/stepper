use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::widgets::{Paragraph, Widget, Wrap};
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
    // Load the persisted prompt history (IO here keeps `state.rs` pure) before the
    // state takes ownership of `init`.
    let history = load_history_file(init.history_path.as_deref());
    // The live theme lives in AppState (the `/theme` editor mutates it).
    let mut state = AppState::new(init);
    state.set_history(history);

    let mut running = Arc::new(AtomicBool::new(true));
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(256);
    let mut reader = spawn_reader(running.clone(), input_tx);

    // Custom status-line command output arrives here from a throttled background
    // run (so a slow/hung script never blocks the UI loop).
    let (status_tx, mut status_rx) = mpsc::channel::<String>(4);
    let mut status_tick: u32 = 0;

    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let mut dirty = true;
    // Force a full repaint on the next draw. Needed because overwriting a wide
    // (CJK) glyph with narrower content leaves its right half on screen — a
    // ratatui inline-viewport + wide-char diff limitation. We only force it when
    // the input actually shrinks past a wide char (not on plain ASCII edits, so
    // there's no flicker) and after a scrollback commit.
    let mut force_clear = false;
    let mut last_input_w = 0usize;
    draw(&mut guard.terminal, &state, &state.theme)?;

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
                // Refresh the custom status line ~every 2s (and immediately on the
                // first tick), running the command off the loop with the live JSON
                // context so its output lands via `status_rx`.
                if let Some(cmd) = state.status_line_cmd.clone() {
                    if status_tick.is_multiple_of(60) {
                        tokio::spawn(run_status_line(cmd, state.status_line_context(), status_tx.clone()));
                    }
                    status_tick = status_tick.wrapping_add(1);
                }
                if dirty {
                    if force_clear {
                        let _ = guard.terminal.clear();
                        force_clear = false;
                    }
                    draw(&mut guard.terminal, &state, &state.theme)?;
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
            Some(line) = status_rx.recv() => {
                state.status_line = Some(line);
                dirty = true;
            }
            _ = cancel.cancelled() => break,
        }

        // `/editor` or Ctrl+E: hand the terminal to $EDITOR. The reader thread must
        // stop first (the child inherits stdin, and a live reader would race it for
        // keystrokes); restart it on a fresh channel afterwards.
        if let Some(seed) = state.take_editor_request() {
            running.store(false, Ordering::Relaxed);
            let _ = reader.join();
            open_external_editor(&mut guard, &mut state, &seed);
            running = Arc::new(AtomicBool::new(true));
            let (tx, rx) = mpsc::channel::<Event>(256);
            input_rx = rx;
            reader = spawn_reader(running.clone(), tx);
            force_clear = true;
            dirty = true;
        }
    }

    running.store(false, Ordering::Relaxed);
    drop(input_rx);
    let _ = reader.join();
    Ok(())
}

/// Run the custom status-line command off the UI loop: pipe the JSON `ctx` to its
/// stdin, capture the first line of stdout, and send it back. Bounded at 5s and
/// killed on drop, so a slow or hung script can never wedge the UI or leak.
async fn run_status_line(cmd: Vec<String>, ctx: serde_json::Value, tx: mpsc::Sender<String>) {
    let Some((prog, args)) = cmd.split_first() else {
        return;
    };
    let child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(ctx.to_string().as_bytes()).await;
        // Drop closes stdin so the command sees EOF and can finish.
    }
    let output = match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        _ => return, // timeout (child killed on drop) or spawn/io error
    };
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if !line.is_empty() {
        let _ = tx.send(line).await;
    }
}

/// Load the persisted prompt history (a JSON array of strings). A missing or
/// unreadable/corrupt file yields an empty history rather than failing the launch.
fn load_history_file(path: Option<&std::path::Path>) -> Vec<String> {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|body| serde_json::from_str::<Vec<String>>(&body).ok())
        .unwrap_or_default()
}

/// The dedicated blocking input reader: forward terminal events to the loop until
/// `running` clears or the channel closes. Split out so the event loop can stop it
/// (to hand stdin to an external editor) and restart it on a fresh channel.
fn spawn_reader(running: Arc<AtomicBool>, tx: mpsc::Sender<Event>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while running.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if tx.blocking_send(ev).is_err() {
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
}

/// Hand the terminal to `$VISUAL`/`$EDITOR` to compose the prompt. `seed` starts
/// the temp file; on a clean exit the saved buffer replaces the input box. The
/// caller must stop the input reader first (the child inherits stdin).
fn open_external_editor(guard: &mut TerminalGuard, state: &mut AppState, seed: &str) {
    let Some(editor) = std::env::var_os("VISUAL").or_else(|| std::env::var_os("EDITOR")) else {
        state.set_notice("set $EDITOR or $VISUAL to compose in an external editor");
        return;
    };
    let editor = editor.to_string_lossy().into_owned();
    let mut parts = editor.split_whitespace();
    let Some(prog) = parts.next() else {
        state.set_notice("$EDITOR is empty");
        return;
    };
    let args: Vec<&str> = parts.collect();

    // O_EXCL + random name (not a predictable /tmp/stepper-prompt-<pid>.md): on a
    // shared host a fixed path is a symlink-follow / TOCTOU hazard — an attacker
    // could pre-plant a symlink so the seed write clobbers a victim file, or swap
    // the file mid-edit to inject text into the prompt. The handle auto-removes the
    // file on drop.
    let tmp = match tempfile::Builder::new().prefix("stepper-prompt-").suffix(".md").tempfile() {
        Ok(t) => t,
        Err(e) => {
            state.set_notice(format!("editor: could not create temp file: {e}"));
            return;
        }
    };
    if let Err(e) = std::fs::write(tmp.path(), seed) {
        state.set_notice(format!("editor: could not write temp file: {e}"));
        return;
    }

    guard.suspend();
    let status = std::process::Command::new(prog)
        .args(&args)
        .arg(tmp.path())
        .current_dir(&state.cwd)
        .status();
    guard.resume();

    match status {
        // Drop a single trailing newline (editors append one) but keep blank lines.
        Ok(s) if s.success() => match std::fs::read_to_string(tmp.path()) {
            Ok(content) => state.set_input(content.strip_suffix('\n').unwrap_or(&content)),
            Err(e) => state.set_notice(format!("editor: could not read result: {e}")),
        },
        Ok(_) => state.set_notice("editor exited without saving — input unchanged"),
        Err(e) => state.set_notice(format!("could not launch editor '{prog}': {e}")),
    }
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

    // Ctrl+C is the universal "get me out" key. A picker or a captured overlay
    // would otherwise swallow it (no quit, or it types into a filter), trapping
    // the user; route it to Quit uniformly. The ApiKey overlay keeps its own
    // Ctrl+C = cancel (don't end the whole session over a mistyped key prompt).
    if let Event::Key(k) = &ev
        && k.code == KeyCode::Char('c')
        && k.modifiers.contains(KeyModifiers::CONTROL)
        && !matches!(state.overlay, Some(crate::state::Overlay::ApiKey(_)))
    {
        let effects = state.apply_action(stepper_protocol::Action::Quit);
        return run_effects(terminal, action_tx, effects);
    }

    // The @-file picker, while open, captures all keys.
    if state.picker.is_some() {
        handle_picker_key(state, &ev);
        return Ok(false);
    }

    // The #-agent autocomplete picker, while open, captures all keys.
    if state.agent_picker.is_some() {
        handle_agent_picker_key(state, &ev);
        return Ok(false);
    }

    // The builtin overlays (context/permissions/rewind/resume picker) capture
    // keys; the approval overlay keeps its y/a/n path through lower_event.
    if state.overlay_captures_keys() {
        handle_overlay_key(state, action_tx, &ev);
        return Ok(false);
    }

    // Mouse wheel scrolls the live-region scrollback (like PgUp/PgDn), but not
    // while the `/` palette is up (where the wheel would scroll behind it).
    if let Event::Mouse(m) = &ev {
        if !state.palette_active() {
            let action = match m.kind {
                MouseEventKind::ScrollUp => Some(stepper_protocol::Action::ScrollUp(3)),
                MouseEventKind::ScrollDown => Some(stepper_protocol::Action::ScrollDown(3)),
                _ => None,
            };
            if let Some(a) = action {
                let _ = state.apply_action(a);
            }
        }
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

    // Typing '#' at the start of an empty prompt opens the named-agent
    // autocomplete picker. Unlike '@' (a path is meaningful anywhere), the
    // `#agent` route is prefix-only, so the picker only arms at the very start
    // and never over an open overlay — see `agent_trigger_armed`.
    if let Event::Key(k) = &ev
        && let KeyCode::Char('#') = k.code
        && state.agent_trigger_armed()
    {
        state.open_agent_picker();
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

    // Ctrl+R opens reverse search over the prompt history (shell convention).
    if let Event::Key(k) = &ev
        && k.code == KeyCode::Char('r')
        && k.modifiers.contains(KeyModifiers::CONTROL)
    {
        state.open_history_search();
        return Ok(false);
    }

    // ↑/↓ recall the prompt history when the cursor is on the boundary row (↑ on
    // the first line, ↓ on the last) — otherwise they move within a multi-line
    // draft. `history_*` return false (e.g. empty history / not browsing) so the
    // key falls through to the textarea as a plain cursor move. Gated on no overlay
    // so it never overwrites a draft hidden behind an active Approval (whose keys
    // are NOT captured by `overlay_captures_keys`, so they reach here).
    if let Event::Key(k) = &ev
        && state.overlay.is_none()
    {
        let row = state.textarea.cursor().0;
        let last_row = state.textarea.lines().len().saturating_sub(1);
        if k.code == KeyCode::Up && row == 0 && state.history_prev() {
            return Ok(false);
        }
        if k.code == KeyCode::Down && row == last_row && state.history_next() {
            return Ok(false);
        }
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
        // Searchable pickers (models): a printable char types into the filter and
        // Backspace edits it; arrows/Enter/Esc still navigate via lower_picker_nav.
        if state.overlay_picker_searchable()
            && let Event::Key(k) = ev
        {
            match k.code {
                KeyCode::Char(c)
                    if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.overlay_picker_push(c);
                    return;
                }
                KeyCode::Backspace => {
                    state.overlay_picker_backspace();
                    return;
                }
                _ => {}
            }
        }
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
    // The `/theme` editor: ↑↓ move rows, ←/→ cycle preset, type a color on a
    // color row, Enter saves (+ persists via SetTheme), Esc reverts.
    if matches!(state.overlay, Some(Overlay::Theme(_))) {
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Up => state.theme_editor_move(-1),
                KeyCode::Down => state.theme_editor_move(1),
                KeyCode::Left => state.theme_editor_cycle(-1),
                KeyCode::Right => state.theme_editor_cycle(1),
                KeyCode::Backspace => state.theme_editor_edit(None),
                KeyCode::Char(c)
                    if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    state.theme_editor_edit(Some(c))
                }
                KeyCode::Enter => {
                    if let Some(action) = state.theme_editor_save() {
                        let _ = action_tx.try_send(action);
                    }
                }
                KeyCode::Esc => state.theme_editor_cancel(),
                _ => {}
            }
        }
        return;
    }
    // The `/settings` tabbed overview: ←/→ (or Tab) switch tabs, Enter opens the
    // focused tab's editor (jump) or closes if it has none, Esc/q closes.
    if matches!(state.overlay, Some(Overlay::Settings(_))) {
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Left => state.settings_tab_move(-1),
                KeyCode::Right | KeyCode::Tab => state.settings_tab_move(1),
                KeyCode::Enter => match state.settings_jump() {
                    Some(action) => {
                        state.overlay_close();
                        let _ = action_tx.try_send(action);
                    }
                    None => state.overlay_close(),
                },
                KeyCode::Esc | KeyCode::Char('q') => state.overlay_close(),
                _ => {}
            }
        }
        return;
    }
    // Ctrl+R reverse search: type to filter, ↑/↓ (or another Ctrl+R) move the
    // selection, Enter loads the entry into the input, Esc/Ctrl+C cancel.
    if matches!(state.overlay, Some(Overlay::HistorySearch(_))) {
        if let Event::Key(k) = ev {
            let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
            match k.code {
                KeyCode::Up => state.history_search_move(-1),
                KeyCode::Down => state.history_search_move(1),
                // A repeated Ctrl+R steps to the next (older) match, shell-style.
                KeyCode::Char('r') if ctrl => state.history_search_move(1),
                KeyCode::Backspace => state.history_search_backspace(),
                KeyCode::Enter => state.history_search_accept(),
                KeyCode::Esc => state.overlay_close(),
                KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                    state.history_search_push(c)
                }
                _ => {}
            }
        }
        return;
    }
    // `ask_user_question`: number keys pick an option directly, ↑/↓ + Enter pick
    // the highlighted one, Esc cancels. The chosen index replies through the
    // request's oneshot (handled in `answer_question`).
    if matches!(state.overlay, Some(Overlay::Question(_))) {
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Up => state.question_move(-1),
                KeyCode::Down => state.question_move(1),
                KeyCode::Enter => state.answer_question(state.question_selected()),
                KeyCode::Esc => state.answer_question(None),
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = c as usize - '1' as usize;
                    state.answer_question(Some(idx));
                }
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
            state.notice = Some(crate::state::Notice {
                level: stepper_protocol::NoticeLevel::Info,
                text: format!("image attached ({n} pending) — it rides with your next message"),
            });
            let _ = action_tx.try_send(stepper_protocol::Action::AttachImage { media_type, data });
        }
        Ok(None) => {}
        Err(e) => {
            state.notice = Some(crate::state::Notice {
                level: stepper_protocol::NoticeLevel::Error,
                text: format!("clipboard image paste failed: {e}"),
            });
        }
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

/// Keys for the `#`-agent autocomplete picker: ↑↓ select, type to filter, Tab or
/// Enter inserts `#name `, Esc cancels, Backspace edits the query (and exits past
/// the trigger). No IO — the agent list is static.
fn handle_agent_picker_key(state: &mut AppState, ev: &Event) {
    let Event::Key(k) = ev else {
        return;
    };
    match k.code {
        KeyCode::Up => state.agent_picker_move(-1),
        KeyCode::Down => state.agent_picker_move(1),
        KeyCode::Tab | KeyCode::Enter => state.agent_picker_select(),
        KeyCode::Esc => state.agent_picker_cancel(),
        KeyCode::Backspace => state.agent_picker_backspace(),
        KeyCode::Char(c) => state.agent_picker_push(c),
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
                // Wrap to the terminal width and reserve the WRAPPED row count:
                // insert_before's buffer spans the full width, so without a wrap a
                // line wider than the terminal is truncated (text lost) and the
                // reserved height (logical lines) is too small.
                let width = terminal.size().map(|s| s.width).unwrap_or(80);
                let height = crate::render::wrapped_row_count(&text.lines, width).max(1) as u16;
                terminal.insert_before(height, |buf| {
                    Paragraph::new(text).wrap(Wrap { trim: false }).render(buf.area, buf);
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
            Effect::Bell => {
                // A bare BEL (`\x07`) — the terminal turns it into an audible beep
                // or a visual flash. Deliberately does NOT set `committed`: it
                // moves no cursor and writes no cell, so it must not trigger a
                // viewport repaint.
                use std::io::Write;
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x07");
                let _ = out.flush();
            }
            Effect::PersistHistory { path, lines } => {
                // Best-effort: a failed write must never disrupt the session.
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(body) = serde_json::to_string(&lines) {
                    let _ = std::fs::write(&path, body);
                }
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
            agents: Vec::new(),
            theme_preset: None,
            theme_colors: Vec::new(),
            effort: None,
            notify_on_complete: false,
            notify_on_approval: false,
            notify_on_error: false,
            history_path: None,
            status_line_cmd: None,
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
