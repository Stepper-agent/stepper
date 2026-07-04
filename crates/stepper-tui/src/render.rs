use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use stepper_protocol::{
    ApprovalKind, ApprovalRequest, ContextBreakdownView, LayerStatus, NoticeLevel,
    PermissionsSnapshotView,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::state::{
    AgentPicker, ApiKeyOverlay, AppState, FilePicker, HistorySearch, ListPicker, Overlay,
    ProcStatus, QuestionView, SettingsView, ShellView, ThemeState,
};
use crate::theme::Theme;

pub fn draw(terminal: &mut DefaultTerminal, state: &AppState, theme: &Theme) -> anyhow::Result<()> {
    terminal.draw(|frame| ui(frame, state, theme))?;
    Ok(())
}

fn ui(frame: &mut Frame, state: &AppState, theme: &Theme) {
    let queue_h = state.queue.len().min(3) as u16;
    // The fan-out worker panel: one row per worker (capped) + bordered block.
    let worker_h = if state.workers.is_empty() {
        0
    } else {
        state.workers.len().min(6) as u16 + 2
    };
    // Grow the input box to fit soft-wrapped content instead of overflowing it.
    // The status line (1) is always kept, and the input is allowed to shrink
    // toward 1 row when workers+queue+input would otherwise overbook the small
    // fixed-height inline viewport and collapse the live area to 0 (render_live
    // also guards height==0). On a normal screen a one-line prompt still gets the
    // usual 3-row box.
    // A status notice (error/warn/info) gets its own always-on row above the
    // footer so an error is visible regardless of turn/tool/assistant state.
    let notice_h = if state.notice.is_some() { 1 } else { 0 };
    let total = frame.area();
    // Paint the inline viewport as a coloured surface panel (the live app region),
    // so the active turn reads as a card. Committed turns keep the terminal's own
    // background (they live in native scrollback). `None` leaves the terminal bg.
    if let Some(bg) = Theme::preset_bg(&state.theme_preset) {
        frame.render_widget(Block::default().style(Style::default().bg(bg)), total);
    }
    let content_rows = input_display_rows(&state.input_text(), total.width.saturating_sub(2));
    let max_input_h = total
        .height
        .saturating_sub(worker_h + queue_h + notice_h + 1 + 1)
        .max(1);
    let input_h = (content_rows + 2).clamp(1, max_input_h);
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(worker_h),
        Constraint::Length(queue_h),
        Constraint::Length(input_h),
        Constraint::Length(notice_h),
        Constraint::Length(1),
    ])
    .split(total);

    if let Some(picker) = &state.picker {
        render_picker(frame, rows[0], picker, theme);
    } else if let Some(agent_picker) = &state.agent_picker {
        render_agent_picker(frame, rows[0], agent_picker, theme);
    } else if let Some(overlay) = &state.overlay {
        match overlay {
            Overlay::Approval(req) => render_approval(frame, rows[0], req, theme),
            Overlay::Context(breakdown) => render_context(frame, rows[0], breakdown, theme),
            Overlay::Permissions(snapshot) => {
                render_permissions(frame, rows[0], snapshot, theme)
            }
            Overlay::Picker(picker) => render_list_picker(frame, rows[0], picker, theme),
            Overlay::ApiKey(o) => render_api_key(frame, rows[0], o, theme),
            Overlay::Shell(s) => render_shell(frame, rows[0], state, s, theme),
            Overlay::Theme(ts) => render_theme(frame, rows[0], ts, theme),
            Overlay::Settings(v) => render_settings(frame, rows[0], v, theme),
            Overlay::HistorySearch(s) => render_history_search(frame, rows[0], state, s, theme),
            Overlay::Question(q) => render_question(frame, rows[0], q, theme),
        }
    } else if state.palette_active() {
        render_palette(frame, rows[0], state, theme);
    } else {
        render_live(frame, rows[0], state, theme);
    }
    if worker_h > 0 {
        render_workers(frame, rows[1], state, theme);
    }
    render_queue(frame, rows[2], state, theme);
    render_input(frame, rows[3], state, theme);
    if notice_h > 0 {
        render_notice(frame, rows[4], state, theme);
    }
    render_status(frame, rows[5], state, theme);
}

/// The always-on status-notice row above the footer: one severity-colored,
/// truncated line (red error / yellow warn / muted info) so an error is never
/// swallowed by an active turn or hidden behind tool/assistant output.
fn render_notice(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(notice) = &state.notice else {
        return;
    };
    let color = match notice.level {
        NoticeLevel::Error => theme.error,
        NoticeLevel::Warn => theme.warning,
        NoticeLevel::Info => theme.muted,
    };
    let text = truncate(&notice.text, area.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color)))),
        area,
    );
}

/// The fan-out worker panel (Claude-Code-style sub-agent view): one row per live
/// worker — glyph + `wN label` + its latest tool/activity + running token count.
fn render_workers(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let done = state
        .workers
        .iter()
        .filter(|w| matches!(w.status, LayerStatus::Done | LayerStatus::Failed))
        .count();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(format!(" workers ({done}/{}) ", state.workers.len()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = inner.height as usize;
    let lines: Vec<Line> = state
        .workers
        .iter()
        .take(rows)
        .map(|w| {
            let (glyph, color) = match w.status {
                LayerStatus::Running => {
                    (spinner_frame(state.spinner).to_string(), theme.layer_color(w.index))
                }
                LayerStatus::Done => ("✓".to_string(), theme.success),
                LayerStatus::Failed => ("✗".to_string(), theme.error),
                LayerStatus::Pending => ("○".to_string(), theme.muted),
            };
            let activity = match &w.last_tool {
                Some(t) => format!("▸ {t}"),
                None => match w.status {
                    LayerStatus::Done => "done".to_string(),
                    LayerStatus::Failed => "failed".to_string(),
                    _ => "working…".to_string(),
                },
            };
            let head = format!("{glyph} w{} {}", w.index + 1, w.label);
            // Size the activity to the space left after head + token count so the
            // running token figure stays visible on narrow/inline terminals.
            let token_str = format!("{} tok", fmt_count(w.tokens));
            let budget = (inner.width as usize)
                .saturating_sub(head.width() + token_str.width() + 4)
                .max(1);
            let tail = format!("  {}  {token_str}", truncate(&activity, budget));
            Line::from(vec![
                Span::styled(
                    head,
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(tail, Style::default().fg(theme.muted)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn render_queue(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 || state.queue.is_empty() {
        return;
    }
    let shown = (area.height as usize).min(state.queue.len());
    let lines: Vec<Line> = state
        .queue
        .iter()
        .take(shown)
        .map(|item| {
            let (sigil, text) = item.label();
            let trunc = truncate(text, area.width.saturating_sub(12) as usize);
            Line::from(vec![
                Span::styled(format!("{sigil} "), Style::default().fg(theme.accent)),
                Span::styled(
                    format!("queued: {trunc}"),
                    Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_picker(frame: &mut Frame, area: Rect, picker: &FilePicker, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" files (@) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(vec![
        Span::styled("@", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(picker.query.clone(), Style::default().fg(theme.accent)),
        Span::styled(
            "   ↑↓ select · Tab open · Enter insert · Esc cancel",
            Style::default().fg(theme.muted),
        ),
    ])];
    let rows = (inner.height as usize).saturating_sub(1);
    for row in 0..picker.matches.len().min(rows) {
        let Some(path) = picker.entry(row) else { break };
        let style = if row == picker.selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(Span::styled(format!("  {path}"), style)));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The `#`-agent autocomplete picker (mirrors the `@` file picker): the live
/// query on top, then the matching agents (name + description), the highlighted
/// one reversed.
fn render_agent_picker(frame: &mut Frame, area: Rect, picker: &AgentPicker, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" agents (#) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(vec![
        Span::styled("#", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(picker.query.clone(), Style::default().fg(theme.accent)),
        Span::styled(
            "   ↑↓ select · Tab/Enter insert · Esc cancel",
            Style::default().fg(theme.muted),
        ),
    ])];
    let rows = (inner.height as usize).saturating_sub(1);
    for row in 0..picker.matches.len().min(rows) {
        let Some(agent) = picker.entry(row) else { break };
        let style = if row == picker.selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        let label = if agent.description.is_empty() {
            format!("  {}", agent.name)
        } else {
            format!("  {} — {}", agent.name, agent.description)
        };
        lines.push(Line::from(Span::styled(label, style)));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The background-process shell view (Down key): a process list on top and the
/// selected process's recent console output below.
fn render_shell(frame: &mut Frame, area: Rect, state: &AppState, shell: &ShellView, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" shell · ↑↓ select · k kill · Esc close ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if state.processes.is_empty() || inner.height == 0 {
        frame.render_widget(
            Paragraph::new("no background processes — run `!cmd &`"),
            inner,
        );
        return;
    }

    let sel = shell.selected.min(state.processes.len() - 1);
    let list_h = ((state.processes.len() as u16) + 1)
        .min(inner.height.saturating_sub(1).max(1))
        .max(1);
    let chunks =
        Layout::vertical([Constraint::Length(list_h), Constraint::Min(0)]).split(inner);

    let rows: Vec<Line> = state
        .processes
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (glyph, color, status) = match &p.status {
                ProcStatus::Running => {
                    (spinner_frame(state.spinner).to_string(), theme.accent, "running".to_string())
                }
                ProcStatus::Exited(Some(0)) => ("✓".to_string(), theme.success, "exit 0".to_string()),
                ProcStatus::Exited(Some(c)) => ("✗".to_string(), theme.error, format!("exit {c}")),
                ProcStatus::Exited(None) => ("✗".to_string(), theme.error, "exited".to_string()),
            };
            let style = if i == sel {
                Style::default().fg(color).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(color)
            };
            let cmd = truncate(&p.command, (inner.width as usize).saturating_sub(24));
            Line::from(Span::styled(format!("{glyph} [{}] {status}  {cmd}", p.id), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(Text::from(rows)), chunks[0]);

    if chunks[1].height > 0
        && let Some(p) = state.processes.get(sel)
    {
        let avail = chunks[1].height as usize;
        let lines: Vec<Line> = p
            .output
            .iter()
            .rev()
            .take(avail)
            .rev()
            .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.muted))))
            .collect();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            chunks[1],
        );
    }
}

fn render_palette(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let matches = state.command_matches();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" commands (/) ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(Span::styled(
        "   ↑↓ select · Tab complete · Enter run",
        Style::default().fg(theme.muted),
    ))];
    let rows = (inner.height as usize).saturating_sub(1);
    let selected = state.palette_selected.min(matches.len().saturating_sub(1));
    let offset = scroll_offset(selected, matches.len(), rows);
    for (i, cmd) in matches.iter().enumerate().skip(offset).take(rows) {
        let style = if i == selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        // `argument-hint` (e.g. `<pr-number>`) is shown right after the name so
        // the user sees what the command expects before invoking it.
        let hint = cmd
            .argument_hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        let label = if cmd.description.is_empty() {
            format!("/{}{hint}", cmd.name)
        } else {
            format!("/{}{hint} — {}", cmd.name, cmd.description)
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&label, inner.width.saturating_sub(2) as usize)),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The generic list-picker overlay (rewind checkpoints / resume sessions):
/// hint line + one row per item, the selected row reversed.
fn render_list_picker(frame: &mut Frame, area: Rect, picker: &ListPicker, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(picker.title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Searchable pickers (models) show the live filter query; type to narrow.
    let hint = if picker.searchable() {
        format!("   filter: {}▏  ↑↓ select · Enter choose · Esc cancel", picker.query)
    } else {
        "   ↑↓ select · Enter choose · Esc cancel".to_string()
    };
    let mut lines = vec![Line::from(Span::styled(hint, Style::default().fg(theme.muted)))];
    if picker.matches.is_empty() {
        lines.push(Line::from(Span::styled("  (no matches)", Style::default().fg(theme.muted))));
    }
    let rows = (inner.height as usize).saturating_sub(1);
    let offset = scroll_offset(picker.selected, picker.matches.len(), rows);
    for (row, &idx) in picker.matches.iter().enumerate().skip(offset).take(rows) {
        let style = if !picker.items[idx].connectable {
            // Unactionable row (an unsupported `/connect` provider): dimmed, never
            // highlighted — Enter is a no-op so a reversed cursor would mislead.
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM)
        } else if row == picker.selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&picker.items[idx].label, inner.width.saturating_sub(2) as usize)),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// `ask_user_question`: the question, then its numbered options with the
/// highlighted one reversed. Number keys / ↑↓+Enter pick, Esc cancels.
fn render_question(frame: &mut Frame, area: Rect, q: &QuestionView, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" question ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            truncate(&q.req.question, inner.width as usize),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];
    for (i, opt) in q.req.options.iter().enumerate() {
        let style = if i == q.selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(Span::styled(
            format!("  {}. {}", i + 1, truncate(opt, inner.width.saturating_sub(5) as usize)),
            style,
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  1-9 / ↑↓+Enter to choose · Esc to skip",
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Ctrl+R reverse search: a filter line over the prompt history, the matched
/// entries below (most-recent first), with the selection highlighted.
fn render_history_search(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    search: &HistorySearch,
    theme: &Theme,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" history ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let hint = format!(
        "   search: {}▏  ↑↓/^R select · Enter use · Esc cancel",
        search.query
    );
    let mut lines = vec![Line::from(Span::styled(hint, Style::default().fg(theme.muted)))];
    let entries = state.history_search_rows();
    if entries.is_empty() {
        lines.push(Line::from(Span::styled("  (no matches)", Style::default().fg(theme.muted))));
    }
    // Find the selected row so the visible window scrolls with it.
    let selected = entries.iter().position(|(_, sel)| *sel).unwrap_or(0);
    let rows = (inner.height as usize).saturating_sub(1);
    let offset = scroll_offset(selected, entries.len(), rows);
    for (entry, is_selected) in entries.iter().skip(offset).take(rows) {
        let style = if *is_selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        // History entries can be multi-line; show only the first line, flattened.
        let first = entry.lines().next().unwrap_or("");
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(first, inner.width.saturating_sub(2) as usize)),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The `/theme` color editor: a preset selector row plus one editable row per
/// color role, each showing a live swatch of its current (typed) value.
fn render_theme(frame: &mut Frame, area: Rect, ts: &ThemeState, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" theme editor ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(Span::styled(
        "   ↑↓ select · ←→ preset · type hex/name · Enter save · Esc cancel",
        Style::default().fg(theme.muted),
    ))];

    // Row 0: preset selector.
    let preset_style = if ts.selected == 0 {
        Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
    } else {
        Style::default().fg(theme.accent)
    };
    lines.push(Line::from(Span::styled(
        format!("  preset:  ‹ {} ›", ts.preset_name()),
        preset_style,
    )));

    // Color rows (scroll to keep the selected one visible).
    let rows = (inner.height as usize).saturating_sub(2);
    let offset = scroll_offset(ts.selected.saturating_sub(1), ts.colors.len(), rows);
    for (i, (name, value)) in ts.colors.iter().enumerate().skip(offset).take(rows) {
        let selected = ts.selected == i + 1;
        let label_style = if selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        // A live swatch of the typed value; invalid values show a red marker.
        let swatch = match Theme::parse_color(value) {
            Some(c) => Span::styled("██", Style::default().fg(c)),
            None => Span::styled("✗ ", Style::default().fg(theme.error)),
        };
        let caret = if selected { "›" } else { " " };
        lines.push(Line::from(vec![
            Span::styled(format!(" {caret} {name:<14} "), label_style),
            swatch,
            Span::styled(format!("  {value}"), label_style),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// The masked API-key entry overlay: a hint line and the typed key shown as
/// bullets (never the real characters), titled with the provider.
fn render_api_key(frame: &mut Frame, area: Rect, o: &ApiKeyOverlay, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            format!(" api key · {} ", o.provider),
            Style::default().fg(theme.muted),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let masked = "•".repeat(o.input.chars().count());
    let lines = vec![
        Line::from(Span::styled(
            "   Enter save · Esc cancel · stored in your OS keyring",
            Style::default().fg(theme.muted),
        )),
        Line::from(Span::styled(format!("  {masked}"), Style::default().fg(theme.accent))),
    ];
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// `/context` — the estimated per-category window decomposition.
fn render_context(frame: &mut Frame, area: Rect, b: &ContextBreakdownView, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" context ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let pct = |v: u64| {
        if b.context_limit == 0 {
            0
        } else {
            (v as u128 * 100 / b.context_limit as u128) as u64
        }
    };
    let categories = [
        ("system prompt", b.system_prompt),
        ("tools", b.tools),
        ("mcp tools", b.mcp_tools),
        ("skills", b.skills),
        ("memory", b.memory),
        ("messages", b.messages),
        ("free", b.free),
    ];
    // Pin the `Esc dismiss` footer so a short inline viewport can never clip it
    // (the old plain Paragraph clipped the bottom — breakdown tail AND the dismiss
    // hint — with no way to see it). The window headline + breakdown fill the body
    // above, top-aligned, so the headline and the first categories stay visible.
    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let (body_area, foot_area) = (split[0], split[1]);
    let mut lines = vec![Line::from(Span::styled(
        format!("window: {} tokens (estimated)", fmt_count(b.context_limit)),
        Style::default().fg(theme.muted),
    ))];
    for (label, value) in categories {
        lines.push(Line::from(vec![
            Span::styled(format!("  {label:<14}"), Style::default().fg(theme.accent)),
            Span::styled(
                format!("{:>8} tok  {:>3}%", fmt_count(value), pct(value)),
                Style::default().fg(theme.muted),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), body_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("Esc dismiss", Style::default().fg(theme.muted)))),
        foot_area,
    );
}

/// `/permissions` — the read-only mode/rules/approvals snapshot.
fn render_permissions(
    frame: &mut Frame,
    area: Rect,
    snapshot: &PermissionsSnapshotView,
    theme: &Theme,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" permissions ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Pin the `mode:` headline (top) and `Esc dismiss` (bottom) as fixed rows so
    // a short inline viewport can never push them off-screen; the rules/approvals
    // list fills the scrollable middle (top-aligned, so mode + the first rules —
    // the security-critical part — stay visible). The old whole-block scroll
    // pinned to the bottom and hid the mode + rules entirely.
    let split = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let (head_area, body_area, foot_area) = (split[0], split[1], split[2]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("mode: ", Style::default().fg(theme.muted)),
            Span::styled(
                snapshot.mode.clone(),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
        ])),
        head_area,
    );
    let mut lines = Vec::new();
    if snapshot.rules.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no rules configured)",
            Style::default().fg(theme.muted),
        )));
    }
    for rule in &snapshot.rules {
        let color = match rule.verdict.as_str() {
            "deny" => theme.error,
            "ask" => theme.warning,
            _ => theme.success,
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<5} ", rule.verdict), Style::default().fg(color)),
            Span::styled(rule.rule.clone(), Style::default().fg(theme.accent)),
            Span::styled(format!("  ({})", rule.source), Style::default().fg(theme.muted)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("approvals ({}):", snapshot.approvals.len()),
        Style::default().fg(theme.muted),
    )));
    for approval in &snapshot.approvals {
        let granted = approval
            .granted_at
            .as_deref()
            .map(|g| format!("  granted {g}"))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", approval.rule), Style::default().fg(theme.accent)),
            Span::styled(granted, Style::default().fg(theme.muted)),
        ]));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), body_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("Esc dismiss", Style::default().fg(theme.muted)))),
        foot_area,
    );
}

fn render_settings(frame: &mut Frame, area: Rect, view: &SettingsView, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(" settings ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Pinned tab bar (top) + scrollable focused-tab body + fixed footer hint.
    let split = Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let (tabs_area, body_area, foot_area) = (split[0], split[1], split[2]);

    let mut tab_spans = Vec::new();
    for (i, tab) in view.snapshot.tabs.iter().enumerate() {
        if i > 0 {
            tab_spans.push(Span::raw("  "));
        }
        let style = if i == view.tab {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(theme.muted)
        };
        tab_spans.push(Span::styled(tab.title.clone(), style));
    }
    frame.render_widget(Paragraph::new(Line::from(tab_spans)), tabs_area);

    let mut lines = Vec::new();
    if let Some(tab) = view.snapshot.tabs.get(view.tab) {
        if tab.rows.is_empty() {
            lines.push(Line::from(Span::styled("  (nothing to show)", Style::default().fg(theme.muted))));
        }
        for r in &tab.rows {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<18}", r.label), Style::default().fg(theme.muted)),
                Span::styled(r.value.clone(), Style::default().fg(theme.accent)),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), body_area);

    let jump_hint = view
        .snapshot
        .tabs
        .get(view.tab)
        .and_then(|t| t.jump.as_ref())
        .map(|j| format!("Enter open /{j}  ·  "))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{jump_hint}←/→ tabs  ·  Esc close"),
            Style::default().fg(theme.muted),
        ))),
        foot_area,
    );
}

fn truncate(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.width() <= max {
        return first.to_string();
    }
    let budget = max.saturating_sub(1); // leave a cell for the ellipsis
    let mut out = String::new();
    let mut width = 0;
    for ch in first.chars() {
        let cw = ch.width().unwrap_or(0);
        if width + cw > budget {
            break;
        }
        out.push(ch);
        width += cw;
    }
    out.push('…');
    out
}

fn render_live(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // A busy screen (many workers + queue + a tall input) can squeeze the live
    // area to zero rows; rendering into a 0-row rect is a no-op, mirror the
    // render_workers/render_queue guards.
    if area.height == 0 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    // The plan/todo list (if any) sits at the top so multi-step progress is
    // legible — the model's `todo_write` updates were stored but never drawn.
    if !state.todos.is_empty() {
        use stepper_protocol::TodoStatus;
        lines.push(Line::from(Span::styled(
            "plan",
            Style::default().fg(theme.muted).add_modifier(Modifier::BOLD),
        )));
        for todo in &state.todos {
            let (mark, style) = match todo.status {
                TodoStatus::Completed => ("☑", Style::default().fg(theme.muted).add_modifier(Modifier::CROSSED_OUT)),
                TodoStatus::InProgress => ("▶", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                TodoStatus::Pending => ("☐", Style::default().fg(theme.muted)),
            };
            lines.push(Line::from(Span::styled(format!("  {mark} {}", todo.content), style)));
        }
        lines.push(Line::from(""));
    }
    // Reasoning models stream their thinking before the answer — show it dimmed
    // so the agent isn't a frozen spinner during a long reason. (The data was
    // already accumulated into live.reasoning; only this draw was missing.)
    if !state.live.reasoning.is_empty() {
        let dim = Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC);
        lines.push(Line::from(Span::styled("thinking…", dim)));
        for rline in state.live.reasoning.lines() {
            lines.push(Line::from(Span::styled(rline.to_string(), dim)));
        }
        if !state.tool_lines.is_empty() || !state.live.assistant.is_empty() {
            lines.push(Line::from(""));
        }
    }
    for tl in &state.tool_lines {
        lines.push(Line::from(Span::styled(
            tl.clone(),
            Style::default().fg(theme.accent),
        )));
    }
    if !state.live.assistant.is_empty() {
        lines.extend(crate::markdown::render_markdown(&state.live.assistant).lines);
    } else if state.tool_lines.is_empty() && !state.turn_active {
        // The notice now has its own always-on row, so the idle hint is a literal.
        lines.push(Line::from(Span::styled(
            "ready — type a message · Enter send · Shift+Tab mode",
            Style::default().fg(theme.muted),
        )));
    }

    // Fence the in-progress / result stream in a titled border so it is visually
    // distinct from the committed scrollback above and the input below. On a tiny
    // (height < 3) live area the border would eat all the content rows, so fall
    // back to borderless there.
    if area.height >= 3 {
        let title = if state.turn_active {
            format!(" {} working… · esc to interrupt ", spinner_frame(state.spinner))
        } else {
            " result ".to_string()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.muted))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // Scroll in WRAPPED rows (what ratatui scrolls by); a long streamed line
        // soft-wraps to many rows, and counting logical lines would leave the
        // newest output below the fold (the agent looks frozen mid-turn).
        let scroll = live_scroll(wrapped_row_count(&lines, inner.width), inner.height, state.scroll_offset);
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((scroll, 0)),
            inner,
        );
    } else {
        let scroll = live_scroll(wrapped_row_count(&lines, area.width), area.height, state.scroll_offset);
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((scroll, 0)),
            area,
        );
    }
}

/// How many visual rows the input text needs at `width` columns once
/// soft-wrapped (WordOrGlyph), so the input box can grow to fit instead of
/// overflowing. Each logical line takes at least one row. Mirrors
/// ratatui-textarea's wrapping closely enough for sizing — any small mismatch is
/// absorbed by the textarea's own vertical scroll.
fn input_display_rows(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let rows: usize = text.split('\n').map(|line| wrapped_rows_for_line(line, width)).sum();
    rows.clamp(1, u16::MAX as usize) as u16
}

fn wrapped_rows_for_line(line: &str, width: usize) -> usize {
    let mut rows = 1usize;
    let mut col = 0usize;
    // Word chunks keep their trailing whitespace; a chunk wider than the row (a
    // long word, or unspaced CJK) is glyph-wrapped across however many rows it
    // needs. Splitting on any whitespace (not just space) keeps tabs from being
    // mismeasured.
    for chunk in line.split_inclusive(char::is_whitespace) {
        let w = UnicodeWidthStr::width(chunk);
        if w > width {
            if col > 0 {
                rows += 1;
            }
            let extra = (w - 1) / width;
            rows += extra;
            col = w - extra * width;
        } else if col + w > width {
            rows += 1;
            col = w;
        } else {
            col += w;
        }
    }
    rows
}

/// Total wrapped visual rows `lines` occupy at `width` columns under
/// `Wrap { trim: false }` — the unit ratatui's `Paragraph::scroll` (and
/// `insert_before` height) actually use. Scrolling/sizing by the LOGICAL line
/// count under-counts whenever a line soft-wraps, pushing the bottom (the newest
/// streamed output, an action/dismiss hint) below the fold.
pub(crate) fn wrapped_row_count(lines: &[Line], width: u16) -> usize {
    let width = width.max(1) as usize;
    lines
        .iter()
        .map(|line| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            wrapped_rows_for_line(&text, width)
        })
        .sum()
}

/// The Paragraph scroll for the live region: pinned to the bottom, then moved up
/// by the user's `offset` (clamped so over-scrolling past the top just shows the
/// top). `offset == 0` keeps the latest output visible.
fn live_scroll(line_count: usize, window: u16, offset: u16) -> u16 {
    let total = line_count as u16;
    let max_scroll = total.saturating_sub(window.max(1));
    max_scroll.saturating_sub(offset.min(max_scroll))
}

/// First visible index for a scrolling list so `selected` stays inside a
/// `window`-row viewport (lists longer than the window scroll to follow it).
fn scroll_offset(selected: usize, len: usize, window: usize) -> usize {
    if window == 0 || len <= window {
        return 0;
    }
    selected.saturating_sub(window - 1).min(len - window)
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // `!`-prefixed input is shell mode (Claude-Code-style).
    let bash_mode = state.input_text().starts_with('!');
    let (base_title, border) = if bash_mode {
        (" bash ! ", theme.warning)
    } else {
        (" message ", theme.border_active)
    };
    // Surface staged clipboard images (Ctrl+V) in the input title.
    let title = if state.pending_image_count > 0 {
        format!("{base_title}· {} image(s) ", state.pending_image_count)
    } else {
        base_title.to_string()
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(Span::styled(title, Style::default().fg(theme.muted)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&state.textarea, inner);

    // Place the REAL terminal cursor at the textarea's display column (the
    // emulated reversed-cell cursor is disabled in AppState::new). The terminal
    // renders its own cursor correctly over wide / CJK glyphs.
    if state.picker.is_none() && state.agent_picker.is_none() && state.overlay.is_none() {
        let sc = state.textarea.screen_cursor();
        // `screen_cursor().row` is the ABSOLUTE row; when a long prompt overflows
        // the box the widget scrolls internally and draws the cursor at
        // `row - top`. Its viewport isn't public, so mirror its scroll rule
        // (`next_scroll_top`) with a retained offset — otherwise the terminal
        // cursor drifts from the highlighted text once the input scrolls.
        let top = next_scroll_top(state.input_scroll_top.get(), sc.row as u16, inner.height);
        state.input_scroll_top.set(top);
        let x = inner.x + (sc.col as u16).min(inner.width.saturating_sub(1));
        let y = inner.y + (sc.row as u16).saturating_sub(top).min(inner.height.saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
}

/// ratatui-textarea's own vertical scroll rule (`widget::next_scroll_top`): keep
/// the previous top unless the cursor left the viewport, then scroll minimally to
/// bring it back to the nearest edge. Replicated here because the widget's
/// viewport offset is `pub(crate)`. Self-correcting: any render pins `top` into
/// `[cursor - height + 1, cursor]`, so a stale value heals in one frame.
fn next_scroll_top(prev_top: u16, cursor_row: u16, height: u16) -> u16 {
    if cursor_row < prev_top {
        cursor_row
    } else if height > 0 && prev_top + height <= cursor_row {
        cursor_row + 1 - height
    } else {
        prev_top
    }
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // A custom `statusLine` command's output replaces the built-in footer.
    if let Some(line) = &state.status_line {
        let truncated = truncate(line, area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(truncated, Style::default().fg(theme.muted)))),
            area,
        );
        return;
    }
    // LEFT: active-layer badge + mode (§4.6.1).
    let mut left: Vec<Span> = Vec::new();
    match &state.active_layer {
        Some(layer) => {
            let (glyph, color) = match layer.status {
                LayerStatus::Running => {
                    (spinner_frame(state.spinner).to_string(), theme.layer_color(layer.index))
                }
                LayerStatus::Done => ("✓".to_string(), theme.success),
                LayerStatus::Failed => ("✗".to_string(), theme.error),
                LayerStatus::Pending => ("○".to_string(), theme.muted),
            };
            left.push(Span::styled(format!("{glyph} "), Style::default().fg(color)));
            left.push(Span::styled(
                format!("layer {}/{}: {}", layer.index + 1, layer.total, layer.name),
                Style::default()
                    .fg(theme.layer_color(layer.index))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        None => left.push(Span::styled("idle", Style::default().fg(theme.muted))),
    }
    left.push(Span::raw("  "));
    // Colour the mode badge by how much it relaxes the guardrails: bypass/dont-ask
    // run without any prompt (error red), accept-edits auto-applies edits (warning).
    let mode_color = match state.mode.label() {
        "bypass" | "dont-ask" => theme.error,
        "accept-edits" => theme.warning,
        _ => theme.accent,
    };
    left.push(Span::styled(format!("[{}]", state.mode.label()), Style::default().fg(mode_color)));
    left.push(sep(theme));
    let cwd_name = state.cwd.file_name().and_then(|s| s.to_str()).unwrap_or(".");
    left.push(Span::styled(cwd_name.to_string(), Style::default().fg(theme.muted)));

    // RIGHT: model · tokens · ctx% gauge · $cost (Claude-Code-style footer).
    let pct = state.usage.context_pct_left();
    let mut right: Vec<Span> = Vec::new();
    right.push(Span::styled(
        format!("{}/{}", state.model.provider, state.model.model),
        Style::default().fg(theme.muted),
    ));
    // Reasoning effort, shown only when set (low/medium/high) — off = absent.
    if let Some(effort) = &state.effort {
        right.push(sep(theme));
        right.push(Span::styled(format!("effort {effort}"), Style::default().fg(theme.accent)));
    }
    right.push(sep(theme));
    right.push(Span::styled(
        format!("{} tok", fmt_count(state.usage.tokens_total())),
        Style::default().fg(theme.muted),
    ));
    right.push(sep(theme));
    right.push(Span::styled(format!("ctx {pct}% "), Style::default().fg(theme.muted)));
    right.push(Span::styled(
        gauge_str(pct),
        Style::default().fg(theme.gauge_color(pct)),
    ));
    right.push(sep(theme));
    right.push(Span::styled(
        fmt_cost(state.usage.cost_usd),
        Style::default().fg(theme.muted),
    ));

    frame.render_widget(Paragraph::new(compose(area.width, left, right)), area);
}

fn render_approval(frame: &mut Frame, area: Rect, req: &ApprovalRequest, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();
    // Compute the diff up front so the title can carry +/- counts (a file edit is
    // the trust surface — the user should see the size of the change at a glance).
    let file_diff = match &req.kind {
        ApprovalKind::FileEdit(diff) => Some((diff, similar::TextDiff::from_lines(diff.old.as_str(), diff.new.as_str()))),
        _ => None,
    };
    let title = match &req.kind {
        ApprovalKind::Command { cmd, outside_project } => format!(
            "run command{}: {cmd}",
            if *outside_project { " (outside project)" } else { "" }
        ),
        ApprovalKind::FileEdit(diff) => {
            let (mut adds, mut dels) = (0usize, 0usize);
            if let Some((_, td)) = &file_diff {
                for c in td.iter_all_changes() {
                    match c.tag() {
                        similar::ChangeTag::Insert => adds += 1,
                        similar::ChangeTag::Delete => dels += 1,
                        similar::ChangeTag::Equal => {}
                    }
                }
            }
            format!("edit {}  (+{adds} -{dels})", diff.path.display())
        }
        ApprovalKind::OutsideProject { path, action } => {
            format!("{action} outside project: {}", path.display())
        }
        ApprovalKind::Mcp { server, tool } => format!("mcp {server}/{tool}"),
    };
    lines.push(Line::from(Span::styled(
        title,
        Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
    )));

    // Where the first changed line lands, so the view scrolls to the change
    // instead of the (usually unchanged) bottom of a big file.
    let mut first_change: Option<usize> = None;
    if let Some((_, td)) = &file_diff {
        let changes: Vec<_> = td.iter_all_changes().collect();
        // Keep an unchanged line only within CONTEXT lines of a real change;
        // collapse longer runs into a "⋯ N unchanged" marker.
        const CONTEXT: usize = 3;
        let mut keep = vec![false; changes.len()];
        for (i, c) in changes.iter().enumerate() {
            if c.tag() != similar::ChangeTag::Equal {
                let lo = i.saturating_sub(CONTEXT);
                let hi = (i + CONTEXT).min(changes.len() - 1);
                keep[lo..=hi].iter_mut().for_each(|k| *k = true);
            }
        }
        let mut i = 0;
        while i < changes.len() {
            if keep[i] {
                let c = &changes[i];
                let (sign, color) = match c.tag() {
                    similar::ChangeTag::Delete => ('-', theme.diff_removed),
                    similar::ChangeTag::Insert => ('+', theme.diff_added),
                    similar::ChangeTag::Equal => (' ', theme.muted),
                };
                if c.tag() != similar::ChangeTag::Equal && first_change.is_none() {
                    first_change = Some(lines.len());
                }
                let body = c.value().trim_end_matches('\n').to_string();
                lines.push(Line::from(Span::styled(format!("{sign}{body}"), Style::default().fg(color))));
                i += 1;
            } else {
                let start = i;
                while i < changes.len() && !keep[i] {
                    i += 1;
                }
                let n = i - start;
                lines.push(Line::from(Span::styled(
                    format!("  ⋯ {n} unchanged line{}", if n == 1 { "" } else { "s" }),
                    Style::default().fg(theme.muted),
                )));
            }
        }
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warning))
        .title(" approval ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Reserve the bottom row for the action hint and render it OUTSIDE the
    // scrolled diff, so it is never scrolled off-screen. The diff can be far
    // taller than the (often short) inline viewport; with the hint inside the
    // scroll region it dropped below the fold and the prompt looked frozen —
    // the user could not see that y/a/n was expected.
    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let (diff_area, hint_area) = (split[0], split[1]);
    // Scroll so the first change is near the top (one line of headroom); else
    // pin to the bottom. Compute in WRAPPED rows (the unit ratatui scrolls by):
    // a diff line wider than the overlay soft-wraps, so a logical-line count would
    // under-scroll and leave the change (or the bottom) out of view.
    let total = wrapped_row_count(&lines, diff_area.width) as u16;
    let max_scroll = total.saturating_sub(diff_area.height.max(1));
    let scroll = match first_change {
        Some(line) => {
            let before = wrapped_row_count(&lines[..line], diff_area.width) as u16;
            before.saturating_sub(1).min(max_scroll)
        }
        None => max_scroll,
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        diff_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "[y] allow once   [a] always allow   [n] deny",
            Style::default().fg(theme.accent),
        ))),
        hint_area,
    );
}

fn sep(theme: &Theme) -> Span<'static> {
    Span::styled(" · ", Style::default().fg(theme.muted))
}

fn compose(width: u16, left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let lw: usize = left.iter().map(Span::width).sum();
    let rw: usize = right.iter().map(Span::width).sum();
    let pad = (width as usize).saturating_sub(lw + rw);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);
    Line::from(spans)
}

fn spinner_frame(n: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[n % FRAMES.len()]
}

fn gauge_str(pct_left: u8) -> String {
    let cells = 5usize;
    let filled = (pct_left as usize * cells).div_ceil(100).min(cells);
    let mut s = String::new();
    for i in 0..cells {
        s.push(if i < filled { '▰' } else { '▱' });
    }
    s
}

fn fmt_count(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn fmt_cost(c: f64) -> String {
    if c <= 0.0 {
        "$0.00".to_string()
    } else {
        format!("${c:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use stepper_protocol::{LayerView, Mode, ModelView, UsageView};

    use crate::state::Queued;

    #[test]
    fn next_scroll_top_keeps_the_cursor_in_the_viewport() {
        // Cursor visible → top unchanged.
        assert_eq!(next_scroll_top(3, 4, 8), 3);
        // Cursor above the top → scroll up so it becomes the first row.
        assert_eq!(next_scroll_top(5, 2, 8), 2);
        // Cursor below the bottom → scroll down so it becomes the last row.
        assert_eq!(next_scroll_top(0, 10, 8), 3, "row 10 with height 8 → top 3 (rows 3..10)");
        // The visible cursor row (sc.row - top) is always within [0, height-1].
        for (top, row, h) in [(0u16, 0u16, 8u16), (5, 2, 8), (0, 10, 8), (3, 4, 8)] {
            let new_top = next_scroll_top(top, row, h);
            assert!(row >= new_top && row - new_top < h, "row {row} visible under top {new_top}, h {h}");
        }
    }

    fn render_to_string(state: &AppState, width: u16, height: u16) -> String {
        let theme = Theme::default();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui(frame, state, &theme)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn base_state() -> AppState {
        AppState::new(crate::TuiInit {
            inline_height: 12,
            model: ModelView { provider: "ollama".into(), model: "qwen3".into() },
            mode: Mode::Plan,
            cwd: PathBuf::from("/tmp/work"),
            commands: vec![
                crate::CommandInfo {
                    name: "review".into(),
                    description: "review the diff".into(),
                    argument_hint: Some("<pr-number>".into()),
                },
                crate::CommandInfo::named("rewind"),
                crate::CommandInfo::named("resume"),
            ],
            agents: Vec::new(),
            theme_preset: None,
            theme_colors: Vec::new(),
            effort: None,
            notify_on_complete: false,
            notify_on_approval: false,
            notify_on_error: false,
            history_path: None,
            status_line_cmd: None,
            keybindings: Vec::new(),
            initial_prompt: None,
        })
    }

    #[test]
    fn command_palette_renders_matching_commands() {
        let mut s = base_state();
        s.textarea.insert_str("/re");
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("commands (/)"), "palette title: {out}");
        assert!(
            out.contains("/review") && out.contains("/rewind") && out.contains("/resume"),
            "palette lists matches: {out}"
        );
        assert!(out.contains("review the diff"), "palette shows the description: {out}");
    }

    #[test]
    fn command_palette_shows_argument_hints() {
        let mut s = base_state();
        s.textarea.insert_str("/rev");
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("/review"), "command listed: {out}");
        assert!(out.contains("<pr-number>"), "argument hint shown next to it: {out}");
    }

    #[test]
    fn status_line_shows_layer_progress_model_and_tokens() {
        let mut s = base_state();
        s.active_layer = Some(LayerView {
            name: "implement".into(),
            index: 1,
            total: 3,
            status: LayerStatus::Running,
        });
        s.model = ModelView { provider: "ollama".into(), model: "qwen3".into() };
        s.usage = UsageView {
            tokens_in: 1200,
            tokens_out: 800,
            context_used: 50,
            context_limit: 200,
            cost_usd: 0.42,
            ..Default::default()
        };
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("layer 2/3: implement"), "status: {out}");
        assert!(out.contains("[plan]"), "mode label: {out}");
        assert!(out.contains("ollama/qwen3"), "model: {out}");
        assert!(out.contains("2.0k tok"), "token count: {out}");
        assert!(out.contains("ctx 75%"), "context gauge percent: {out}");
        assert!(out.contains("$0.42"), "cost: {out}");
    }

    #[test]
    fn idle_status_renders_idle_segment_and_ready_hint() {
        let s = base_state();
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("idle"), "idle segment: {out}");
        assert!(out.contains("ready"), "ready hint: {out}");
        assert!(out.contains("work"), "cwd file name: {out}");
    }

    #[test]
    fn queued_messages_render_in_the_queue_strip() {
        let mut s = base_state();
        s.queue.push_back(Queued::Chat("follow up question".into()));
        s.queue.push_back(Queued::Shell("cargo test".into()));
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("queued: follow up question"), "chat queued: {out}");
        assert!(out.contains("queued: cargo test"), "shell queued: {out}");
    }

    #[test]
    fn approval_overlay_renders_diff_and_prompt() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest, DiffView};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = base_state();
        let (reply, _rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::FileEdit(DiffView {
                path: PathBuf::from("src/main.rs"),
                old: "old line\n".into(),
                new: "new line\n".into(),
            }),
            reply,
        }));
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("edit src/main.rs"), "approval title: {out}");
        assert!(out.contains("allow once"), "approval prompt: {out}");
    }

    #[test]
    fn approval_hint_stays_visible_with_a_tall_diff_in_a_short_viewport() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest, DiffView};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = base_state();
        let (reply, _rx) = oneshot::channel();
        // A change at the TOP of a long file: the diff is far taller than a short
        // inline viewport. The old renderer scrolled to the top change and pushed
        // the `[y/a/n]` hint (the last line of the scrolled body) off-screen, so
        // the prompt looked frozen. The hint must now stay pinned regardless.
        let old: String = std::iter::once("CHANGE ME\n".to_string())
            .chain((0..40).map(|i| format!("line {i}\n")))
            .collect();
        let new: String = std::iter::once("CHANGED LINE\n".to_string())
            .chain((0..40).map(|i| format!("line {i}\n")))
            .collect();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::FileEdit(DiffView { path: PathBuf::from("a.txt"), old, new }),
            reply,
        }));
        let out = render_to_string(&s, 80, 10);
        assert!(out.contains("CHANGED LINE"), "the top change is shown: {out}");
        assert!(
            out.contains("allow once"),
            "the y/a/n hint must stay visible even when the diff overflows the viewport: {out}"
        );
    }

    #[test]
    fn picker_overlay_lists_candidate_entries() {
        let mut s = base_state();
        s.set_picker(
            String::new(),
            vec!["src/".into(), "notes.md".into()],
            "",
        );
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("files (@)"), "picker title: {out}");
        assert!(out.contains("src/"), "dir entry: {out}");
        assert!(out.contains("notes.md"), "file entry: {out}");
    }

    #[test]
    fn agent_picker_lists_named_agents_with_descriptions() {
        let mut s = base_state();
        s.agents = vec![crate::AgentInfo {
            name: "reviewer".into(),
            description: "code review".into(),
        }];
        s.open_agent_picker();
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("agents (#)"), "agent picker title: {out}");
        assert!(out.contains("reviewer"), "agent name: {out}");
        assert!(out.contains("code review"), "agent description: {out}");
    }

    #[test]
    fn streaming_assistant_text_renders_in_live_region() {
        let mut s = base_state();
        s.turn_active = true;
        s.live.assistant = "streamed answer here".into();
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("streamed answer here"), "live body: {out}");
    }

    #[test]
    fn bash_input_switches_input_block_title() {
        let mut s = base_state();
        s.textarea.insert_str("!ls");
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("bash !"), "bash title: {out}");
    }

    #[test]
    fn status_line_renders_ctx_gauge_glyphs_and_done_layer_glyph() {
        let mut s = base_state();
        s.active_layer = Some(LayerView {
            name: "implement".into(),
            index: 1,
            total: 3,
            status: LayerStatus::Done,
        });
        s.usage = UsageView {
            context_used: 50,
            context_limit: 200,
            ..Default::default()
        };
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains('▰'), "filled gauge cell glyph: {out}");
        assert!(out.contains('▱'), "empty gauge cell glyph: {out}");
        assert!(out.contains('✓'), "Done layer-status glyph: {out}");
    }

    #[test]
    fn status_line_renders_running_spinner_glyph() {
        let mut s = base_state();
        s.spinner = 0;
        s.active_layer = Some(LayerView {
            name: "plan".into(),
            index: 0,
            total: 2,
            status: LayerStatus::Running,
        });
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains('⠋'), "Running spinner-frame-0 glyph: {out}");
    }

    #[test]
    fn truncate_cuts_on_display_columns_for_wide_chars() {
        let wide = "一二三四五";
        let out = truncate(wide, 5);
        assert_eq!(out, "一二…", "wide string truncated by columns, not char count: {out}");
        assert_eq!(out.width(), 5, "result must fit the display-column budget");
        assert!(out.chars().count() < wide.chars().count(), "char count alone would not have cut");
    }

    #[test]
    fn truncate_keeps_short_wide_string_whole() {
        let wide = "한글";
        let out = truncate(wide, 5);
        assert_eq!(out, "한글", "width 4 fits in 5 columns and stays whole");
    }

    #[test]
    fn input_rows_count_logical_and_wrapped_lines() {
        // Short text fits one row; explicit newlines add rows.
        assert_eq!(input_display_rows("hi", 20), 1);
        assert_eq!(input_display_rows("a\nb\nc", 20), 3);
        // A line wider than the box wraps onto extra rows (word-aware).
        assert_eq!(input_display_rows("aaaa bbbb cccc", 10), 2);
        // A single word wider than the whole row glyph-wraps.
        assert_eq!(input_display_rows("aaaaaaaaaaaa", 5), 3);
        // Unspaced wide (CJK) text wraps by display columns, not char count.
        assert_eq!(input_display_rows("가나다라마", 6), 2);
        // Empty input still occupies one row.
        assert_eq!(input_display_rows("", 10), 1);
    }

    #[test]
    fn scroll_offset_keeps_selection_in_view() {
        // Fits entirely → no scroll.
        assert_eq!(scroll_offset(0, 5, 7), 0);
        assert_eq!(scroll_offset(4, 5, 7), 0);
        // 9 items, 7-row window: selecting below the fold scrolls just enough.
        assert_eq!(scroll_offset(6, 9, 7), 0, "still visible without scrolling");
        assert_eq!(scroll_offset(7, 9, 7), 1, "scrolls one row to reveal index 7");
        assert_eq!(scroll_offset(8, 9, 7), 2, "clamped to the last full window");
        // Degenerate window.
        assert_eq!(scroll_offset(3, 9, 0), 0);
    }

    #[test]
    fn list_picker_overlay_renders_title_hint_and_rows() {
        use crate::state::{ListPicker, ListPickerItem, PickerKind};
        let mut s = base_state();
        let mut picker = ListPicker::new(
            PickerKind::Rewind(stepper_protocol::RewindScope::Both),
            vec![
                ListPickerItem { id: "turn-2".into(), label: "turn 2".into(), connectable: true },
                ListPickerItem { id: "turn-1".into(), label: "turn 1".into(), connectable: true },
            ],
        );
        picker.selected = 1;
        s.overlay = Some(Overlay::Picker(picker));
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("rewind"), "picker title: {out}");
        assert!(out.contains("Enter choose"), "hint line: {out}");
        assert!(out.contains("turn 2") && out.contains("turn 1"), "rows: {out}");
    }

    #[test]
    fn resume_picker_overlay_uses_its_own_title() {
        use crate::state::{ListPicker, ListPickerItem, PickerKind};
        let mut s = base_state();
        s.overlay = Some(Overlay::Picker(ListPicker::new(
            PickerKind::Resume,
            vec![ListPickerItem {
                id: "abc".into(),
                label: "earlier · 2 turn(s) · 3m ago — fix the bug".into(),
                connectable: true,
            }],
        )));
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("resume"), "picker title: {out}");
        assert!(out.contains("fix the bug"), "session row: {out}");
    }

    #[test]
    fn theme_editor_overlay_renders_preset_and_color_rows() {
        let mut s = base_state();
        s.open_theme_editor();
        let out = render_to_string(&s, 100, 16);
        assert!(out.contains("theme editor"), "editor title: {out}");
        assert!(out.contains("preset"), "preset selector row: {out}");
        assert!(out.contains("accent"), "a color role row is shown: {out}");
    }

    #[test]
    fn context_overlay_renders_category_breakdown() {
        use stepper_protocol::ContextBreakdownView;
        let mut s = base_state();
        s.overlay = Some(Overlay::Context(ContextBreakdownView {
            system_prompt: 1000,
            tools: 500,
            mcp_tools: 0,
            skills: 250,
            memory: 2000,
            messages: 4000,
            free: 192_250,
            context_limit: 200_000,
        }));
        let out = render_to_string(&s, 100, 16);
        assert!(out.contains("context"), "panel title: {out}");
        assert!(out.contains("system prompt"), "category: {out}");
        assert!(out.contains("mcp tools"), "category: {out}");
        assert!(out.contains("messages"), "category: {out}");
        assert!(out.contains("4.0k tok"), "messages token count: {out}");
        assert!(out.contains("free"), "free row: {out}");
        assert!(out.contains("200.0k tokens"), "window line: {out}");
        // The dismiss hint is pinned, so it stays visible (it used to clip).
        assert!(out.contains("Esc dismiss"), "dismiss hint stays visible: {out}");
    }

    #[test]
    fn permissions_overlay_renders_mode_rules_with_sources_and_approvals() {
        use stepper_protocol::{ApprovalRuleView, PermissionRuleView, PermissionsSnapshotView};
        let mut s = base_state();
        s.overlay = Some(Overlay::Permissions(PermissionsSnapshotView {
            mode: "accept-edits".into(),
            rules: vec![
                PermissionRuleView {
                    verdict: "allow".into(),
                    rule: "Bash(cargo *)".into(),
                    source: "scaffold".into(),
                },
                PermissionRuleView {
                    verdict: "deny".into(),
                    rule: "Read(//etc/**)".into(),
                    source: "project".into(),
                },
            ],
            approvals: vec![ApprovalRuleView {
                rule: "Bash(git status)".into(),
                scope: Some("git status".into()),
                granted_at: Some("2026-06-01".into()),
            }],
        }));
        let out = render_to_string(&s, 100, 14);
        assert!(out.contains("permissions"), "panel title: {out}");
        assert!(out.contains("mode: accept-edits"), "mode line: {out}");
        assert!(out.contains("Bash(cargo *)") && out.contains("(scaffold)"), "rule + source: {out}");
        assert!(out.contains("Read(//etc/**)") && out.contains("(project)"), "deny rule: {out}");
        assert!(out.contains("approvals (1)"), "approvals header: {out}");
        assert!(out.contains("Bash(git status)"), "approval row: {out}");
    }

    #[test]
    fn permissions_keeps_mode_visible_in_a_short_viewport() {
        use stepper_protocol::{PermissionRuleView, PermissionsSnapshotView};
        let mut s = base_state();
        // More rules than fit: the old whole-block scroll pinned to the bottom and
        // pushed the `mode:` headline (and rules) off the top of a short viewport.
        let rules = (0..8)
            .map(|i| PermissionRuleView {
                verdict: "ask".into(),
                rule: format!("Bash(cmd{i}:*)"),
                source: "project".into(),
            })
            .collect();
        s.overlay = Some(Overlay::Permissions(PermissionsSnapshotView {
            mode: "plan".into(),
            rules,
            approvals: vec![],
        }));
        let out = render_to_string(&s, 70, 8);
        assert!(out.contains("mode:") && out.contains("plan"), "mode headline stays pinned: {out}");
        assert!(out.contains("Esc dismiss"), "dismiss hint stays pinned: {out}");
    }

    #[test]
    fn live_region_keeps_newest_output_when_a_long_line_wraps() {
        let mut s = base_state();
        s.turn_active = true;
        // A long line soft-wraps to many rows; the newest line is appended after.
        // The old logical-line scroll under-counted and left the tail off-screen.
        s.live.assistant = format!("{}\nNEWEST_TOKENS", "x".repeat(300));
        let out = render_to_string(&s, 40, 8);
        assert!(
            out.contains("NEWEST_TOKENS"),
            "newest streamed output stays in view when an earlier line wraps: {out}"
        );
    }

    #[test]
    fn approval_surfaces_over_an_open_file_picker() {
        use stepper_protocol::{AppEvent, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = base_state();
        s.set_picker(String::new(), vec!["src/".into(), "notes.md".into()], "");
        assert!(s.picker.is_some(), "the @-file picker is open");
        let (reply, _rx) = oneshot::channel();
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Command { cmd: "rm -rf /".into(), outside_project: false },
            reply,
        }));
        // The transient picker is dropped so the approval is both drawn and
        // key-routable (it used to stay hidden behind the picker → turn hangs).
        assert!(s.picker.is_none(), "the @-picker is dropped for the approval");
        assert!(matches!(s.overlay, Some(Overlay::Approval(_))), "approval is the live overlay");
        let out = render_to_string(&s, 80, 10);
        assert!(out.contains("allow once"), "approval prompt is visible, not hidden: {out}");
    }

    #[test]
    fn worker_panel_renders_rows_with_labels_tokens_and_glyphs() {
        use stepper_protocol::WorkerView;
        let mut s = base_state();
        s.turn_active = true;
        s.workers = vec![
            WorkerView {
                index: 0,
                total: 2,
                label: "api".into(),
                provider: "omlx".into(),
                model: "qwen3".into(),
                tokens: 1200,
                last_tool: Some("edit_file: a.rs".into()),
                status: LayerStatus::Running,
            },
            WorkerView {
                index: 1,
                total: 2,
                label: "db".into(),
                provider: "omlx".into(),
                model: "qwen3".into(),
                tokens: 0,
                last_tool: None,
                status: LayerStatus::Done,
            },
        ];
        let out = render_to_string(&s, 100, 16);
        assert!(out.contains("workers (1/2)"), "panel title shows done/total: {out}");
        assert!(out.contains("w1 api"), "worker 1 row: {out}");
        assert!(out.contains("w2 db"), "worker 2 row: {out}");
        assert!(out.contains("edit_file: a.rs"), "running worker's last tool: {out}");
        assert!(out.contains("1.2k tok"), "worker token count: {out}");
        assert!(out.contains('✓'), "done worker glyph: {out}");
    }

    #[test]
    fn live_area_survives_max_workers_queue_and_tall_input() {
        // The overbooked-layout path: 6 workers (8 rows) + 3 queued + a wrapping
        // input on a 14-row viewport would push the live area to 0. The input is
        // allowed to shrink and render_live guards height==0, so this must render
        // (no panic) and keep the status line (model name) visible.
        use stepper_protocol::WorkerView;
        let mut s = base_state();
        s.turn_active = true;
        for i in 0..6 {
            s.workers.push(WorkerView {
                index: i,
                total: 6,
                label: format!("w{i}"),
                provider: "omlx".into(),
                model: "qwen3".into(),
                tokens: 0,
                last_tool: None,
                status: LayerStatus::Running,
            });
        }
        for _ in 0..3 {
            s.queue.push_back(Queued::Chat("queued message".into()));
        }
        s.textarea.insert_str("a very long line of text ".repeat(8));
        s.live.assistant.push_str("streamed assistant output");
        let out = render_to_string(&s, 40, 14);
        assert!(out.contains("qwen3"), "status line stays visible under overbooking: {out}");
    }

    #[test]
    fn scroll_offset_reveals_earlier_live_lines() {
        let mut s = base_state();
        // Many verbatim tool lines (not markdown-merged) overflow the live region.
        for i in 0..30 {
            s.tool_lines.push(format!("line{i:02}"));
        }
        // Pinned to the bottom: the last line shows, the first does not.
        let bottom = render_to_string(&s, 40, 10);
        assert!(bottom.contains("line29"), "pinned view shows the latest line: {bottom}");
        assert!(!bottom.contains("line00"), "the first line is scrolled off: {bottom}");
        // Scrolled all the way up (offset clamps to the top): the first line shows.
        s.scroll_offset = 100;
        let scrolled = render_to_string(&s, 40, 10);
        assert!(scrolled.contains("line00"), "scrolling up reveals the first line: {scrolled}");
    }

    #[test]
    fn error_notice_is_shown_even_during_an_active_turn() {
        // The regression: an error during a turn (with tool output present) used to
        // be swallowed because the only notice draw was the idle hint fallback.
        let mut s = base_state();
        s.turn_active = true;
        s.tool_lines.push("▸ read_file: a.rs".into());
        s.notice = Some(crate::state::Notice {
            level: stepper_protocol::NoticeLevel::Error,
            text: "boom".into(),
        });
        let out = render_to_string(&s, 100, 14);
        assert!(out.contains("boom"), "error notice must render even mid-turn: {out}");
    }

    #[test]
    fn palette_scrolls_selection_into_view() {
        // A long command list on a short viewport must scroll so the selected
        // entry (past the fold) is visible and early entries scroll off.
        let mut s = base_state();
        s.commands = (0..20).map(|i| crate::CommandInfo::named(&format!("cmd{i:02}"))).collect();
        s.textarea.insert_str("/cmd");
        s.palette_selected = 18;
        let out = render_to_string(&s, 40, 8);
        assert!(out.contains("cmd18"), "selected item scrolled into view: {out}");
        assert!(!out.contains("cmd00"), "early items scrolled off the top: {out}");
    }

}
