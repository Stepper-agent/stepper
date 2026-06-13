use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use stepper_protocol::{
    ApprovalKind, ApprovalRequest, ContextBreakdownView, LayerStatus, PermissionsSnapshotView,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::state::{AppState, FilePicker, ListPicker, Overlay};
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
    let rows = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(worker_h),
        Constraint::Length(queue_h),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    if let Some(picker) = &state.picker {
        render_picker(frame, rows[0], picker, theme);
    } else if let Some(overlay) = &state.overlay {
        match overlay {
            Overlay::Approval(req) => render_approval(frame, rows[0], req, theme),
            Overlay::Context(breakdown) => render_context(frame, rows[0], breakdown, theme),
            Overlay::Permissions(snapshot) => {
                render_permissions(frame, rows[0], snapshot, theme)
            }
            Overlay::Picker(picker) => render_list_picker(frame, rows[0], picker, theme),
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
    render_status(frame, rows[4], state, theme);
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
            "   ↑↓ select · Enter insert · Esc cancel",
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
    for (row, name) in matches.iter().take(rows).enumerate() {
        let style = if row == selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(Span::styled(format!("  /{name}"), style)));
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

    let mut lines = vec![Line::from(Span::styled(
        "   ↑↓ select · Enter choose · Esc cancel",
        Style::default().fg(theme.muted),
    ))];
    let rows = (inner.height as usize).saturating_sub(1);
    for (row, item) in picker.items.iter().take(rows).enumerate() {
        let style = if row == picker.selected {
            Style::default().fg(theme.accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme.muted)
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", truncate(&item.label, inner.width.saturating_sub(2) as usize)),
            style,
        )));
    }
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
    lines.push(Line::from(Span::styled(
        "  Esc dismiss",
        Style::default().fg(theme.muted),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
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

    let mut lines = vec![Line::from(vec![
        Span::styled("mode: ", Style::default().fg(theme.muted)),
        Span::styled(
            snapshot.mode.clone(),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
    ])];
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
    lines.push(Line::from(Span::styled(
        "  Esc dismiss",
        Style::default().fg(theme.muted),
    )));
    let scroll = (lines.len() as u16).saturating_sub(inner.height.max(1));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
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
    let mut lines: Vec<Line> = Vec::new();
    for tl in &state.tool_lines {
        lines.push(Line::from(Span::styled(
            tl.clone(),
            Style::default().fg(theme.accent),
        )));
    }
    if !state.live.assistant.is_empty() {
        lines.extend(tui_markdown::from_str(&state.live.assistant).lines);
    } else if state.tool_lines.is_empty() && !state.turn_active {
        let hint = state
            .notice
            .clone()
            .unwrap_or_else(|| "ready — type a message · Enter send · Shift+Tab mode".into());
        lines.push(Line::from(Span::styled(hint, Style::default().fg(theme.muted))));
    }

    let scroll = (lines.len() as u16).saturating_sub(area.height.max(1));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    // `!`-prefixed input is shell mode (Claude-Code-style).
    let bash_mode = state.input_text().starts_with('!');
    let (title, border) = if bash_mode {
        (" bash ! ", theme.warning)
    } else {
        (" message ", theme.border_active)
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
    if state.picker.is_none() && state.overlay.is_none() {
        let sc = state.textarea.screen_cursor();
        let x = inner.x + (sc.col as u16).min(inner.width.saturating_sub(1));
        let y = inner.y + (sc.row as u16).min(inner.height.saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
}

fn render_status(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
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
    left.push(Span::styled(
        format!("[{}]", state.mode.label()),
        Style::default().fg(theme.accent),
    ));
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
    let title = match &req.kind {
        ApprovalKind::Command { cmd, outside_project } => format!(
            "run command{}: {cmd}",
            if *outside_project { " (outside project)" } else { "" }
        ),
        ApprovalKind::FileEdit(diff) => format!("edit {}", diff.path.display()),
        ApprovalKind::OutsideProject { path, action } => {
            format!("{action} outside project: {}", path.display())
        }
        ApprovalKind::Mcp { server, tool } => format!("mcp {server}/{tool}"),
    };
    lines.push(Line::from(Span::styled(
        title,
        Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
    )));

    if let ApprovalKind::FileEdit(diff) = &req.kind {
        let td = similar::TextDiff::from_lines(diff.old.as_str(), diff.new.as_str());
        for change in td.iter_all_changes() {
            let (sign, color) = match change.tag() {
                similar::ChangeTag::Delete => ('-', theme.diff_removed),
                similar::ChangeTag::Insert => ('+', theme.diff_added),
                similar::ChangeTag::Equal => (' ', theme.muted),
            };
            let body = change.value().trim_end_matches('\n').to_string();
            lines.push(Line::from(Span::styled(
                format!("{sign}{body}"),
                Style::default().fg(color),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[y] allow once   [a] always allow   [n] deny",
        Style::default().fg(theme.accent),
    )));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warning))
        .title(" approval ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let scroll = (lines.len() as u16).saturating_sub(inner.height.max(1));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        inner,
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
            commands: vec!["review".into(), "rewind".into(), "resume".into()],
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
    fn list_picker_overlay_renders_title_hint_and_rows() {
        use crate::state::{ListPicker, ListPickerItem, PickerKind};
        let mut s = base_state();
        s.overlay = Some(Overlay::Picker(ListPicker {
            kind: PickerKind::Rewind,
            items: vec![
                ListPickerItem { id: "turn-2".into(), label: "turn 2".into() },
                ListPickerItem { id: "turn-1".into(), label: "turn 1".into() },
            ],
            selected: 1,
        }));
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("rewind"), "picker title: {out}");
        assert!(out.contains("Enter choose"), "hint line: {out}");
        assert!(out.contains("turn 2") && out.contains("turn 1"), "rows: {out}");
    }

    #[test]
    fn resume_picker_overlay_uses_its_own_title() {
        use crate::state::{ListPicker, ListPickerItem, PickerKind};
        let mut s = base_state();
        s.overlay = Some(Overlay::Picker(ListPicker {
            kind: PickerKind::Resume,
            items: vec![ListPickerItem {
                id: "abc".into(),
                label: "earlier · 2 turn(s) · 3m ago — fix the bug".into(),
            }],
            selected: 0,
        }));
        let out = render_to_string(&s, 100, 12);
        assert!(out.contains("resume"), "picker title: {out}");
        assert!(out.contains("fix the bug"), "session row: {out}");
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
        let out = render_to_string(&s, 100, 14);
        assert!(out.contains("context"), "panel title: {out}");
        assert!(out.contains("system prompt"), "category: {out}");
        assert!(out.contains("mcp tools"), "category: {out}");
        assert!(out.contains("messages"), "category: {out}");
        assert!(out.contains("4.0k tok"), "messages token count: {out}");
        assert!(out.contains("free"), "free row: {out}");
        assert!(out.contains("200.0k tokens"), "window line: {out}");
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
}
