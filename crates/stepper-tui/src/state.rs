use ratatui::style::Style;
use ratatui_textarea::TextArea;
use smallvec::SmallVec;
use std::collections::VecDeque;
use std::path::PathBuf;
use stepper_protocol::{
    Action, AppEvent, ApprovalRequest, CheckpointView, ContextBreakdownView, LayerStatus,
    LayerView, Mode, ModelView, PermissionsSnapshotView, SessionView, TodoItemView, UsageView,
    WorkerView,
};

use crate::TuiInit;

/// Side effects the event loop must execute after a state transition (state.rs
/// itself stays IO-free and synchronous, so it's trivially unit-testable).
pub enum Effect {
    /// Forward an action to core over the action channel.
    Send(Action),
    /// Commit a finalized assistant turn (markdown source) into native
    /// scrollback via `Terminal::insert_before`.
    CommitToScrollback(String),
}

pub type Effects = SmallVec<[Effect; 2]>;

/// A message the user submitted while a turn was already running. Dispatched
/// one-at-a-time when the current turn completes (Claude-Code-style queue).
#[derive(Clone)]
pub enum Queued {
    Chat(String),
    Shell(String),
}

impl Queued {
    pub fn label(&self) -> (&'static str, &str) {
        match self {
            Queued::Chat(t) => ("⏎", t),
            Queued::Shell(c) => ("!", c),
        }
    }
}

#[derive(Default)]
pub struct StreamBuf {
    pub assistant: String,
    pub reasoning: String,
}

impl StreamBuf {
    fn clear(&mut self) {
        self.assistant.clear();
        self.reasoning.clear();
    }

    fn is_empty(&self) -> bool {
        self.assistant.is_empty() && self.reasoning.is_empty()
    }
}

pub enum Overlay {
    Approval(ApprovalRequest),
    /// `/context` — the estimated window decomposition (any-key dismiss).
    Context(ContextBreakdownView),
    /// `/permissions` — the read-only rules/approvals snapshot (any-key dismiss).
    Permissions(PermissionsSnapshotView),
    /// The generic list picker (rewind checkpoints / resume sessions).
    Picker(ListPicker),
}

/// What a list-picker selection turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Rewind,
    Resume,
}

/// The generic list-picker overlay shared by `/rewind` (checkpoints) and
/// `/resume` (sessions): up/down moves with wrap, Enter sends the selection to
/// core as an `Action`, Esc cancels.
pub struct ListPicker {
    pub kind: PickerKind,
    pub items: Vec<ListPickerItem>,
    pub selected: usize,
}

pub struct ListPickerItem {
    /// The id sent back to core (`turn-N` checkpoint id / session id).
    pub id: String,
    /// The rendered row.
    pub label: String,
}

impl ListPicker {
    pub fn title(&self) -> &'static str {
        match self.kind {
            PickerKind::Rewind => " rewind ",
            PickerKind::Resume => " resume ",
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as i32;
        self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
    }

    fn selection(&self) -> Option<Action> {
        let item = self.items.get(self.selected)?;
        Some(match self.kind {
            PickerKind::Rewind => Action::Rewind {
                checkpoint_id: item.id.clone(),
            },
            PickerKind::Resume => Action::Resume {
                session_id: item.id.clone(),
            },
        })
    }
}

/// `@`-triggered file/folder picker. Path-aware: the query is a path fragment
/// (relative, absolute `/…`, or `~/…`). The event loop lists the resolved
/// directory (IO) and hands the candidates here; filtering the trailing name
/// fragment and resolving a selection stay pure.
pub struct FilePicker {
    pub query: String,
    candidates: Vec<String>,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl FilePicker {
    fn move_sel(&mut self, delta: i32) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as i32;
        self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
    }

    fn current(&self) -> Option<&str> {
        self.matches.get(self.selected).map(|&i| self.candidates[i].as_str())
    }

    pub fn entry(&self, row: usize) -> Option<&str> {
        self.matches.get(row).map(|&i| self.candidates[i].as_str())
    }
}

/// Outcome of selecting a picker entry.
pub enum Selection {
    /// A directory — drill in by re-listing with this new query.
    Navigate(String),
    /// A file — insert this path into the prompt as `@path`.
    Insert(String),
}

/// Pure render state. No IO, no awaits — `apply_event` / `apply_action` mutate it
/// and return `Effects` for the loop to run.
pub struct AppState {
    pub live: StreamBuf,
    pub tool_lines: Vec<String>,
    pub textarea: TextArea<'static>,
    pub mode: Mode,
    pub model: ModelView,
    pub usage: UsageView,
    pub active_layer: Option<LayerView>,
    /// Live fan-out workers (parallel layer / `dispatch`), shown as a panel above
    /// the input while a fan-out is running. Sorted by worker index.
    pub workers: Vec<WorkerView>,
    pub todos: Vec<TodoItemView>,
    pub overlay: Option<Overlay>,
    /// Approval requests that arrived while another was on screen — concurrent
    /// parallel workers each gate independently, so they queue here and are shown
    /// one at a time (without this, a later request would clobber the prior one's
    /// overlay → its worker auto-denied).
    pub pending_approvals: VecDeque<ApprovalRequest>,
    pub picker: Option<FilePicker>,
    pub queue: VecDeque<Queued>,
    pub notice: Option<String>,
    pub spinner: usize,
    pub turn_active: bool,
    pub cwd: PathBuf,
    /// Known slash-command names (for the `/` palette), supplied at startup.
    pub commands: Vec<String>,
    pub palette_selected: usize,
    /// A first Esc on empty input arms this; a second Esc (before any other
    /// action/typing) opens the rewind picker (Esc-Esc, Claude-Code-style).
    pub esc_armed: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(init: TuiInit) -> Self {
        let mut textarea = TextArea::default();
        // Disable the emulated (reversed-cell) cursor; the event loop places the
        // real terminal cursor at the display column instead, so CJK / wide
        // characters align correctly (the reversed-wide-cell cursor mis-renders).
        textarea.set_cursor_style(Style::default());
        Self {
            live: StreamBuf::default(),
            tool_lines: Vec::new(),
            textarea,
            mode: init.mode,
            model: init.model,
            usage: UsageView::default(),
            active_layer: None,
            workers: Vec::new(),
            todos: Vec::new(),
            overlay: None,
            pending_approvals: VecDeque::new(),
            picker: None,
            queue: VecDeque::new(),
            notice: None,
            spinner: 0,
            turn_active: false,
            cwd: init.cwd,
            commands: init.commands,
            palette_selected: 0,
            esc_armed: false,
            should_quit: false,
        }
    }

    pub fn input_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    // ── `/` slash-command palette (pure; the command list is supplied at init) ──

    /// Command names whose prefix matches the `/<partial>` currently typed.
    /// Empty unless the input is a single `/`-prefixed token with no whitespace
    /// yet (and no other overlay is open).
    pub fn command_matches(&self) -> Vec<String> {
        if self.picker.is_some() || self.overlay.is_some() {
            return Vec::new();
        }
        let text = self.input_text();
        let Some(rest) = text.strip_prefix('/') else {
            return Vec::new();
        };
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        self.commands
            .iter()
            .filter(|c| c.starts_with(rest))
            .cloned()
            .collect()
    }

    pub fn palette_active(&self) -> bool {
        !self.command_matches().is_empty()
    }

    /// Move the palette selection, wrapping around the current match set.
    pub fn palette_move(&mut self, delta: i32) {
        let n = self.command_matches().len() as i32;
        if n == 0 {
            return;
        }
        self.palette_selected = (((self.palette_selected as i32 + delta) % n + n) % n) as usize;
    }

    /// Replace the input with the selected command, ready for arguments.
    pub fn palette_complete(&mut self) {
        let matches = self.command_matches();
        if matches.is_empty() {
            return;
        }
        let idx = self.palette_selected.min(matches.len() - 1);
        self.textarea = TextArea::default();
        self.textarea.set_cursor_style(Style::default());
        self.textarea.insert_str(format!("/{} ", matches[idx]));
        self.palette_selected = 0;
    }

    // ── @ file picker (IO-free; the file list is supplied by the event loop) ──

    /// (Re)build the picker for `query` after the event loop has listed the
    /// resolved directory into `candidates`. `filter` is the trailing name
    /// fragment of the query (after the last `/`).
    pub fn set_picker(&mut self, query: String, candidates: Vec<String>, filter: &str) {
        let needle = filter.to_lowercase();
        let matches = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| needle.is_empty() || c.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.picker = Some(FilePicker { query, candidates, matches, selected: 0 });
    }

    pub fn picker_query(&self) -> Option<&str> {
        self.picker.as_ref().map(|p| p.query.as_str())
    }

    pub fn picker_move(&mut self, delta: i32) {
        if let Some(p) = &mut self.picker {
            p.move_sel(delta);
        }
    }

    pub fn picker_cancel(&mut self) {
        self.picker = None;
    }

    /// Resolve the highlighted entry: a directory drills in (Navigate, IO done
    /// by the loop), a file is inserted into the prompt (Insert).
    pub fn picker_selection(&self) -> Option<Selection> {
        let p = self.picker.as_ref()?;
        let name = p.current()?;
        let prefix = match p.query.rfind('/') {
            Some(i) => &p.query[..=i],
            None => "",
        };
        let combined = format!("{prefix}{name}");
        Some(if name.ends_with('/') {
            Selection::Navigate(combined)
        } else {
            Selection::Insert(combined)
        })
    }

    pub fn insert_picker_path(&mut self, path: &str) {
        self.textarea.insert_str(format!("@{path} "));
        self.picker = None;
    }

    // ── builtin overlays (context / permissions / list picker) ──

    /// Whether the current overlay is one whose keys the event loop must route
    /// to the overlay methods below (the approval overlay keeps its own y/a/n
    /// path through `lower_event`).
    pub fn overlay_captures_keys(&self) -> bool {
        matches!(
            self.overlay,
            Some(Overlay::Context(_) | Overlay::Permissions(_) | Overlay::Picker(_))
        )
    }

    /// Close the current non-approval overlay; a queued approval (one that
    /// arrived while it was open) surfaces immediately so it is never stranded.
    pub fn overlay_close(&mut self) {
        self.overlay = self.pending_approvals.pop_front().map(Overlay::Approval);
    }

    pub fn overlay_picker_move(&mut self, delta: i32) {
        if let Some(Overlay::Picker(p)) = &mut self.overlay {
            p.move_sel(delta);
        }
    }

    /// Resolve the highlighted picker entry into its `Action` (Rewind/Resume),
    /// closing the picker. Empty pickers just close.
    pub fn overlay_picker_select(&mut self) -> Effects {
        let mut effects = Effects::new();
        if let Some(Overlay::Picker(p)) = &self.overlay {
            if let Some(action) = p.selection() {
                effects.push(Effect::Send(action));
            }
            self.overlay_close();
        }
        effects
    }

    /// Open a builtin overlay unless an approval is on screen (its oneshot must
    /// not be clobbered — the breakdown/picker is droppable, the approval isn't).
    fn open_overlay(&mut self, overlay: Overlay) {
        if matches!(self.overlay, Some(Overlay::Approval(_))) {
            return;
        }
        self.overlay = Some(overlay);
    }

    /// core -> TUI. Returns effects (mainly a scrollback commit on turn end).
    pub fn apply_event(&mut self, event: AppEvent) -> Effects {
        let mut effects = Effects::new();
        match event {
            AppEvent::TurnStarted { .. } => {
                self.turn_active = true;
                self.notice = None;
                self.live.clear();
                self.tool_lines.clear();
                self.workers.clear();
            }
            AppEvent::AssistantTokenDelta(s) => self.live.assistant.push_str(&s),
            AppEvent::ReasoningTokenDelta(s) => self.live.reasoning.push_str(&s),
            AppEvent::ToolCallStarted(v) => {
                self.tool_lines.push(format!("▸ {}: {}", v.name, v.summary));
            }
            AppEvent::ToolCallOutputDelta { .. } => {}
            AppEvent::ToolCallFinished { id, ok } => {
                let mark = if ok { "✓" } else { "✗" };
                self.tool_lines.push(format!("{mark} tool {id} finished"));
            }
            AppEvent::DiffProposed { .. } => {}
            AppEvent::ApprovalRequested(req) => {
                if self.overlay.is_none() {
                    self.overlay = Some(Overlay::Approval(req));
                } else {
                    self.pending_approvals.push_back(req);
                }
            }
            AppEvent::TodoUpdated(items) => self.todos = items,
            AppEvent::LayerStarted { index, total, name } => {
                // A new layer ends any prior fan-out's panel.
                self.workers.clear();
                self.active_layer = Some(LayerView {
                    name,
                    index,
                    total,
                    status: LayerStatus::Running,
                });
            }
            AppEvent::LayerFinished { status, .. } => {
                if let Some(layer) = self.active_layer.as_mut() {
                    layer.status = status;
                }
                self.flush_block(&mut effects);
            }
            AppEvent::WorkerStarted { index, total, label, model } => {
                // run_parallel announces a batch in index order, so index 0 marks a
                // new batch — clear the prior batch's rows (e.g. a second dispatch
                // in the same layer) so no ghost worker lingers.
                if index == 0 {
                    self.workers.clear();
                }
                let view = WorkerView {
                    index,
                    total,
                    label,
                    provider: model.provider,
                    model: model.model,
                    tokens: 0,
                    last_tool: None,
                    status: LayerStatus::Running,
                };
                match self.workers.iter_mut().find(|w| w.index == index) {
                    Some(w) => *w = view,
                    None => {
                        self.workers.push(view);
                        self.workers.sort_by_key(|w| w.index);
                    }
                }
            }
            AppEvent::WorkerActivity { index, tokens, tool } => {
                if let Some(w) = self.workers.iter_mut().find(|w| w.index == index) {
                    if let Some(t) = tokens {
                        w.tokens = t;
                    }
                    if tool.is_some() {
                        w.last_tool = tool;
                    }
                }
            }
            AppEvent::WorkerFinished { index, status } => {
                if let Some(w) = self.workers.iter_mut().find(|w| w.index == index) {
                    w.status = status;
                }
            }
            AppEvent::UsageUpdated(u) => self.usage = u,
            AppEvent::ModelChanged(m) => self.model = m,
            AppEvent::CompactionStarted => self.notice = Some("compacting context…".into()),
            AppEvent::CompactionDone { freed_tokens } => {
                self.notice = Some(format!("compacted (-{freed_tokens} tok)"));
            }
            AppEvent::Notice { text, .. } => self.notice = Some(text),
            AppEvent::ContextBreakdown(breakdown) => {
                self.open_overlay(Overlay::Context(breakdown));
            }
            AppEvent::PermissionsSnapshot(snapshot) => {
                self.open_overlay(Overlay::Permissions(snapshot));
            }
            AppEvent::CheckpointList(checkpoints) => {
                let items = checkpoints
                    .into_iter()
                    .map(|c: CheckpointView| ListPickerItem {
                        label: format!("turn {}", c.turn),
                        id: c.id,
                    })
                    .collect();
                self.open_overlay(Overlay::Picker(ListPicker {
                    kind: PickerKind::Rewind,
                    items,
                    selected: 0,
                }));
            }
            AppEvent::SessionList(sessions) => {
                let items = sessions
                    .into_iter()
                    .map(|s: SessionView| ListPickerItem {
                        label: format!(
                            "{} · {} turn(s) · {} — {}",
                            s.name.as_deref().unwrap_or(&s.id),
                            s.turns,
                            s.age,
                            s.digest
                        ),
                        id: s.id,
                    })
                    .collect();
                self.open_overlay(Overlay::Picker(ListPicker {
                    kind: PickerKind::Resume,
                    items,
                    selected: 0,
                }));
            }
            AppEvent::SessionResumed { id, name, turns } => {
                self.live.clear();
                self.tool_lines.clear();
                self.todos.clear();
                self.workers.clear();
                self.usage = UsageView::default();
                self.notice = Some(format!(
                    "resumed session {} ({turns} turn(s))",
                    name.unwrap_or(id)
                ));
            }
            AppEvent::TurnComplete { .. } => {
                self.turn_active = false;
                self.workers.clear();
                self.flush_block(&mut effects);
                self.dispatch_queued(&mut effects);
            }
            AppEvent::Error(text) => self.notice = Some(format!("error: {text}")),
        }
        effects
    }

    /// TUI -> core (already mode-resolved). Some variants are handled locally;
    /// others produce an `Effect::Send` to forward to core.
    pub fn apply_action(&mut self, action: Action) -> Effects {
        let mut effects = Effects::new();
        // Any action other than another Esc breaks the Esc-Esc chord.
        let was_armed = std::mem::take(&mut self.esc_armed);
        match action {
            Action::SubmitInput(text) => self.submit(Queued::Chat(text), &mut effects),
            Action::RunShell(cmd) => self.submit(Queued::Shell(cmd), &mut effects),
            Action::RemoveLastQueued => {
                self.queue.pop_back();
            }
            Action::InsertNewline => {
                self.textarea.insert_str("\n");
            }
            Action::CycleMode => {
                self.mode = self.mode.next();
                effects.push(Effect::Send(Action::SetMode(self.mode)));
            }
            Action::SetMode(m) => {
                self.mode = m;
                effects.push(Effect::Send(Action::SetMode(m)));
            }
            // Esc: interrupt, and on EMPTY input a second consecutive Esc opens
            // the rewind picker instead (Esc-Esc) — data flows core-ward as the
            // /rewind built-in so the checkpoint list arrives as an AppEvent.
            Action::Interrupt => {
                if self.input_text().is_empty() && was_armed {
                    effects.push(Effect::Send(Action::SlashCommand {
                        name: "rewind".into(),
                        args: String::new(),
                    }));
                } else {
                    self.esc_armed = self.input_text().is_empty();
                    effects.push(Effect::Send(Action::Interrupt));
                }
            }
            Action::Approve { request_id, decision } => {
                if let Some(Overlay::Approval(req)) = self.overlay.take() {
                    if req.id == request_id {
                        let _ = req.reply.send(decision);
                        // surface the next queued worker's approval, if any.
                        if let Some(next) = self.pending_approvals.pop_front() {
                            self.overlay = Some(Overlay::Approval(next));
                        }
                    } else {
                        self.overlay = Some(Overlay::Approval(req));
                    }
                }
            }
            Action::Quit => self.should_quit = true,
            other @ (Action::SlashCommand { .. } | Action::Rewind { .. } | Action::Resume { .. }) => {
                effects.push(Effect::Send(other));
            }
            Action::ScrollUp(_) | Action::ScrollDown(_) | Action::Redraw => {}
        }
        effects
    }

    /// Send now if idle, otherwise queue for after the current turn.
    fn submit(&mut self, item: Queued, effects: &mut Effects) {
        let text_empty = match &item {
            Queued::Chat(t) | Queued::Shell(t) => t.trim().is_empty(),
        };
        if text_empty {
            return;
        }
        self.textarea = TextArea::default();
        if self.turn_active {
            self.queue.push_back(item);
        } else {
            self.turn_active = true;
            effects.push(Effect::Send(item.into_action()));
        }
    }

    fn dispatch_queued(&mut self, effects: &mut Effects) {
        if let Some(next) = self.queue.pop_front() {
            self.turn_active = true;
            effects.push(Effect::Send(next.into_action()));
        }
    }

    /// Commit the current tool lines + assistant markdown as one scrollback block
    /// and clear the live buffer. Called when a layer finishes and at turn end.
    fn flush_block(&mut self, effects: &mut Effects) {
        if self.live.is_empty() && self.tool_lines.is_empty() {
            return;
        }
        let mut md = String::new();
        for line in &self.tool_lines {
            md.push_str(line);
            md.push('\n');
        }
        if !self.tool_lines.is_empty() {
            md.push('\n');
        }
        md.push_str(&self.live.assistant);
        effects.push(Effect::CommitToScrollback(md));
        self.live.clear();
        self.tool_lines.clear();
    }
}

impl Queued {
    fn into_action(self) -> Action {
        match self {
            Queued::Chat(t) => Action::SubmitInput(t),
            Queued::Shell(c) => Action::RunShell(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stepper_protocol::{LayerStatus, ModelView};

    fn test_state() -> AppState {
        AppState::new(crate::TuiInit {
            inline_height: 10,
            model: ModelView { provider: "p".into(), model: "m".into() },
            mode: Mode::Auto,
            cwd: PathBuf::from("/tmp"),
            commands: vec!["review".into(), "rewind".into(), "resume".into()],
        })
    }

    #[test]
    fn command_palette_matches_by_prefix_only_for_a_bare_slash_token() {
        let mut s = test_state();
        assert!(!s.palette_active(), "no `/` typed yet");

        s.textarea.insert_str("/re");
        assert_eq!(
            s.command_matches(),
            vec!["review".to_string(), "rewind".into(), "resume".into()]
        );
        assert!(s.palette_active());

        s.textarea = TextArea::default();
        s.textarea.insert_str("/rev");
        assert_eq!(s.command_matches(), vec!["review".to_string()]);

        s.textarea = TextArea::default();
        s.textarea.insert_str("/nope");
        assert!(s.command_matches().is_empty());

        // Once a space is typed the name is complete — the palette closes.
        s.textarea = TextArea::default();
        s.textarea.insert_str("/review ");
        assert!(!s.palette_active(), "whitespace ends the palette");
    }

    #[test]
    fn command_palette_move_wraps_and_complete_fills_the_input() {
        let mut s = test_state();
        s.textarea.insert_str("/re");
        assert_eq!(s.palette_selected, 0);
        s.palette_move(1);
        assert_eq!(s.palette_selected, 1);
        s.palette_move(-1);
        assert_eq!(s.palette_selected, 0);
        s.palette_move(-1);
        assert_eq!(s.palette_selected, 2, "moving back from the first wraps to the last");

        s.palette_complete();
        assert_eq!(s.input_text(), "/resume ");
        assert!(!s.palette_active(), "after completion the name is done");
    }

    #[test]
    fn streaming_then_turn_complete_commits_scrollback() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("hello ".into()));
        s.apply_event(AppEvent::AssistantTokenDelta("world".into()));
        assert_eq!(s.live.assistant, "hello world");
        assert!(s.turn_active);

        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::CommitToScrollback(md) => assert!(md.contains("hello world")),
            _ => panic!("expected a scrollback commit"),
        }
        assert!(s.live.assistant.is_empty());
        assert!(!s.turn_active);
    }

    #[test]
    fn cycle_mode_advances_and_forwards_setmode() {
        let mut s = test_state();
        let effects = s.apply_action(Action::CycleMode);
        assert_eq!(s.mode, Mode::Plan);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Send(Action::SetMode(Mode::Plan))]
        ));
    }

    #[test]
    fn cycle_mode_includes_default_but_never_dont_ask_or_bypass() {
        let mut s = test_state();
        let mut seen = Vec::new();
        for _ in 0..4 {
            s.apply_action(Action::CycleMode);
            seen.push(s.mode);
        }
        assert_eq!(
            seen,
            vec![Mode::Plan, Mode::AcceptEdits, Mode::Default, Mode::Auto]
        );
    }

    #[test]
    fn layer_finished_commits_block_and_marks_status() {
        let mut s = test_state();
        s.apply_event(AppEvent::LayerStarted { index: 0, total: 2, name: "plan".into() });
        s.apply_event(AppEvent::AssistantTokenDelta("plan body".into()));
        let effects = s.apply_event(AppEvent::LayerFinished { index: 0, status: LayerStatus::Done });
        assert_eq!(effects.len(), 1);
        assert!(s.live.assistant.is_empty());
        assert_eq!(s.active_layer.as_ref().unwrap().status, LayerStatus::Done);
    }

    #[test]
    fn submit_when_idle_sends_immediately() {
        let mut s = test_state();
        let effects = s.apply_action(Action::SubmitInput("hi".into()));
        match effects.as_slice() {
            [Effect::Send(Action::SubmitInput(t))] => assert_eq!(t.as_str(), "hi"),
            _ => panic!("expected immediate send"),
        }
        assert!(s.turn_active);
    }

    #[test]
    fn empty_submit_is_ignored() {
        let mut s = test_state();
        let effects = s.apply_action(Action::SubmitInput("   ".into()));
        assert!(effects.is_empty());
    }

    #[test]
    fn submit_while_busy_queues_then_dispatches_on_complete() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        // queued, not sent
        let effects = s.apply_action(Action::SubmitInput("follow up".into()));
        assert!(effects.is_empty());
        assert_eq!(s.queue.len(), 1);
        // a second one also queues (shell)
        s.apply_action(Action::RunShell("ls".into()));
        assert_eq!(s.queue.len(), 2);
        // turn ends -> first queued dispatched in order
        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::Send(Action::SubmitInput(t)) if t == "follow up"
        )));
        assert_eq!(s.queue.len(), 1);
        assert!(s.turn_active);
    }

    #[test]
    fn picker_navigate_into_dir_and_insert_file() {
        let mut s = test_state();
        s.set_picker(
            "/home/user/".to_string(),
            vec!["src/".into(), "notes.md".into()],
            "",
        );
        // dir entry -> drill in (absolute path preserved)
        match s.picker_selection() {
            Some(Selection::Navigate(q)) => assert_eq!(q, "/home/user/src/"),
            _ => panic!("expected navigate"),
        }
        // file entry -> insert full path
        s.picker_move(1);
        match s.picker_selection() {
            Some(Selection::Insert(p)) => assert_eq!(p, "/home/user/notes.md"),
            _ => panic!("expected insert"),
        }
    }

    #[test]
    fn remove_last_queued_pops_back() {
        let mut s = test_state();
        s.turn_active = true;
        s.apply_action(Action::SubmitInput("a".into()));
        s.apply_action(Action::SubmitInput("b".into()));
        assert_eq!(s.queue.len(), 2);
        s.apply_action(Action::RemoveLastQueued);
        assert_eq!(s.queue.len(), 1);
        match &s.queue[0] {
            Queued::Chat(t) => assert_eq!(t, "a"),
            _ => panic!(),
        }
    }

    #[test]
    fn set_mode_updates_and_forwards() {
        let mut s = test_state();
        let effects = s.apply_action(Action::SetMode(Mode::AcceptEdits));
        assert_eq!(s.mode, Mode::AcceptEdits);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Send(Action::SetMode(Mode::AcceptEdits))]
        ));
    }

    #[test]
    fn interrupt_forwards_to_core() {
        let mut s = test_state();
        let effects = s.apply_action(Action::Interrupt);
        assert!(matches!(effects.as_slice(), [Effect::Send(Action::Interrupt)]));
    }

    #[test]
    fn slash_command_is_forwarded_unchanged() {
        let mut s = test_state();
        let effects = s.apply_action(Action::SlashCommand { name: "model".into(), args: "x".into() });
        match effects.as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "model");
                assert_eq!(args, "x");
            }
            _ => panic!("expected the slash command to be forwarded"),
        }
    }

    #[test]
    fn quit_sets_should_quit_without_effects() {
        let mut s = test_state();
        let effects = s.apply_action(Action::Quit);
        assert!(s.should_quit);
        assert!(effects.is_empty());
    }

    #[test]
    fn insert_newline_grows_textarea_and_emits_nothing() {
        let mut s = test_state();
        s.textarea.insert_str("a");
        let effects = s.apply_action(Action::InsertNewline);
        assert!(effects.is_empty());
        assert_eq!(s.input_text(), "a\n");
    }

    #[test]
    fn scroll_and_redraw_are_local_noops() {
        let mut s = test_state();
        assert!(s.apply_action(Action::ScrollUp(3)).is_empty());
        assert!(s.apply_action(Action::ScrollDown(3)).is_empty());
        assert!(s.apply_action(Action::Redraw).is_empty());
    }

    #[test]
    fn approve_matching_id_sends_decision_and_clears_overlay() {
        use stepper_protocol::{ApprovalDecision, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        let id = Uuid::new_v4();
        let (reply, rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id,
            kind: ApprovalKind::Mcp { server: "fs".into(), tool: "write".into() },
            reply,
        }));
        let effects = s.apply_action(Action::Approve { request_id: id, decision: ApprovalDecision::AllowOnce });
        assert!(effects.is_empty());
        assert!(s.overlay.is_none());
        assert!(matches!(rx.blocking_recv(), Ok(ApprovalDecision::AllowOnce)));
    }

    #[test]
    fn approve_mismatched_id_keeps_overlay_and_does_not_reply() {
        use stepper_protocol::{ApprovalDecision, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        let id = Uuid::new_v4();
        let (reply, rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id,
            kind: ApprovalKind::Mcp { server: "fs".into(), tool: "write".into() },
            reply,
        }));
        s.apply_action(Action::Approve { request_id: Uuid::new_v4(), decision: ApprovalDecision::Deny });
        assert!(s.overlay.is_some());
        drop(rx);
    }

    #[test]
    fn concurrent_approvals_queue_and_surface_one_at_a_time() {
        use stepper_protocol::{ApprovalDecision, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        let (id1, id2) = (Uuid::new_v4(), Uuid::new_v4());
        let (reply1, rx1) = oneshot::channel();
        let (reply2, rx2) = oneshot::channel();
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: id1,
            kind: ApprovalKind::Mcp { server: "a".into(), tool: "x".into() },
            reply: reply1,
        }));
        // a second concurrent worker's approval arrives while the first is on screen
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: id2,
            kind: ApprovalKind::Mcp { server: "b".into(), tool: "y".into() },
            reply: reply2,
        }));
        assert_eq!(s.pending_approvals.len(), 1, "second approval is queued, not clobbered");
        assert!(matches!(&s.overlay, Some(Overlay::Approval(r)) if r.id == id1));

        // resolving the first delivers its decision AND surfaces the queued second
        s.apply_action(Action::Approve { request_id: id1, decision: ApprovalDecision::AllowOnce });
        assert!(matches!(rx1.blocking_recv(), Ok(ApprovalDecision::AllowOnce)));
        assert!(s.pending_approvals.is_empty());
        assert!(matches!(&s.overlay, Some(Overlay::Approval(r)) if r.id == id2), "second now shown");

        // the second worker still gets a real decision (not a silent auto-deny)
        s.apply_action(Action::Approve { request_id: id2, decision: ApprovalDecision::Deny });
        assert!(matches!(rx2.blocking_recv(), Ok(ApprovalDecision::Deny)));
        assert!(s.overlay.is_none());
    }

    #[test]
    fn flush_block_orders_tool_lines_before_assistant_markdown() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 7 });
        s.apply_event(AppEvent::ToolCallStarted(stepper_protocol::ToolCallView {
            id: "t1".into(),
            name: "read".into(),
            summary: "main.rs".into(),
        }));
        s.apply_event(AppEvent::AssistantTokenDelta("body text".into()));
        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 7 });
        let md = match effects.into_iter().next() {
            Some(Effect::CommitToScrollback(md)) => md,
            _ => panic!("expected a scrollback commit"),
        };
        let tool_at = md.find("▸ read: main.rs").expect("tool line present");
        let body_at = md.find("body text").expect("assistant body present");
        assert!(tool_at < body_at, "tool line must precede assistant body");
    }

    #[test]
    fn turn_complete_with_no_content_commits_nothing() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(effects.is_empty());
        assert!(!s.turn_active);
    }

    #[test]
    fn dispatch_queued_preserves_fifo_order_across_two_turns() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_action(Action::SubmitInput("first".into()));
        s.apply_action(Action::RunShell("second".into()));
        assert_eq!(s.queue.len(), 2);

        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(effects.iter().any(|e| matches!(
            e, Effect::Send(Action::SubmitInput(t)) if t == "first"
        )));
        assert_eq!(s.queue.len(), 1);

        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 2 });
        assert!(effects.iter().any(|e| matches!(
            e, Effect::Send(Action::RunShell(c)) if c == "second"
        )));
        assert!(s.queue.is_empty());
    }

    #[test]
    fn approval_event_opens_overlay() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        let (reply, _rx) = oneshot::channel();
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Command { cmd: "ls".into(), outside_project: false },
            reply,
        }));
        assert!(matches!(s.overlay, Some(Overlay::Approval(_))));
    }

    #[test]
    fn notice_and_error_events_set_notice_text() {
        let mut s = test_state();
        s.apply_event(AppEvent::Notice {
            level: stepper_protocol::NoticeLevel::Info,
            text: "saved".into(),
        });
        assert_eq!(s.notice.as_deref(), Some("saved"));
        s.apply_event(AppEvent::Error("boom".into()));
        assert_eq!(s.notice.as_deref(), Some("error: boom"));
    }

    #[test]
    fn tool_call_finished_marks_success_and_failure() {
        let mut s = test_state();
        s.apply_event(AppEvent::ToolCallFinished { id: "1".into(), ok: true });
        s.apply_event(AppEvent::ToolCallFinished { id: "2".into(), ok: false });
        assert!(s.tool_lines[0].starts_with('✓'));
        assert!(s.tool_lines[1].starts_with('✗'));
    }

    #[test]
    fn streambuf_accumulates_then_clears_on_new_turn() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("ab".into()));
        s.apply_event(AppEvent::ReasoningTokenDelta("think".into()));
        assert_eq!(s.live.assistant, "ab");
        assert_eq!(s.live.reasoning, "think");
        s.apply_event(AppEvent::TurnStarted { turn_id: 2 });
        assert!(s.live.assistant.is_empty());
        assert!(s.live.reasoning.is_empty());
    }

    #[test]
    fn picker_filter_narrows_candidates_by_trailing_fragment() {
        let mut s = test_state();
        s.set_picker(
            "no".to_string(),
            vec!["notes.md".into(), "src/".into(), "README".into()],
            "no",
        );
        let p = s.picker.as_ref().unwrap();
        assert_eq!(p.matches.len(), 1);
        match s.picker_selection() {
            Some(Selection::Insert(path)) => assert_eq!(path, "notes.md"),
            _ => panic!("expected the single filtered file to insert"),
        }
    }

    #[test]
    fn picker_move_wraps_around_match_list() {
        let mut s = test_state();
        s.set_picker(
            String::new(),
            vec!["a/".into(), "b.txt".into()],
            "",
        );
        s.picker_move(-1);
        match s.picker_selection() {
            Some(Selection::Insert(p)) => assert_eq!(p, "b.txt"),
            _ => panic!("wrap to last entry (a file) expected"),
        }
        s.picker_move(1);
        match s.picker_selection() {
            Some(Selection::Navigate(q)) => assert_eq!(q, "a/"),
            _ => panic!("wrap back to first entry (a dir) expected"),
        }
    }

    #[test]
    fn relative_picker_navigate_combines_prefix_with_dir_entry() {
        let mut s = test_state();
        s.set_picker(
            "src/m".to_string(),
            vec!["models/".into()],
            "m",
        );
        match s.picker_selection() {
            Some(Selection::Navigate(q)) => assert_eq!(q, "src/models/"),
            _ => panic!("expected navigate combining prefix and dir"),
        }
    }

    #[test]
    fn insert_picker_path_writes_at_token_and_closes_picker() {
        let mut s = test_state();
        s.set_picker(String::new(), vec!["notes.md".into()], "");
        s.insert_picker_path("notes.md");
        assert_eq!(s.input_text(), "@notes.md ");
        assert!(s.picker.is_none());
    }

    #[test]
    fn picker_cancel_clears_picker() {
        let mut s = test_state();
        s.set_picker(String::new(), vec!["x".into()], "");
        assert!(s.picker.is_some());
        s.picker_cancel();
        assert!(s.picker.is_none());
    }

    #[test]
    fn picker_selection_is_none_when_no_matches() {
        let mut s = test_state();
        s.set_picker("zzz".to_string(), vec!["a.txt".into()], "zzz");
        assert!(s.picker_selection().is_none());
    }

    #[test]
    fn tool_call_output_delta_is_a_noop_and_does_not_corrupt_state() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("answer".into()));
        s.apply_event(AppEvent::ToolCallStarted(stepper_protocol::ToolCallView {
            id: "t1".into(),
            name: "read".into(),
            summary: "main.rs".into(),
        }));
        let effects =
            s.apply_event(AppEvent::ToolCallOutputDelta { id: "t1".into(), chunk: "partial".into() });
        assert!(effects.is_empty(), "output delta must produce no effect");
        assert_eq!(s.live.assistant, "answer", "live buffer must be untouched");
        assert_eq!(s.tool_lines.len(), 1, "tool lines must be untouched");
        assert!(s.notice.is_none());
        assert!(s.turn_active);
    }

    #[test]
    fn diff_proposed_is_a_noop_and_does_not_corrupt_state() {
        use stepper_protocol::DiffView;
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("body".into()));
        let effects = s.apply_event(AppEvent::DiffProposed {
            id: "d1".into(),
            diff: DiffView {
                path: PathBuf::from("src/main.rs"),
                old: "a\n".into(),
                new: "b\n".into(),
            },
        });
        assert!(effects.is_empty(), "diff proposed must produce no effect");
        assert_eq!(s.live.assistant, "body", "live buffer must be untouched");
        assert!(s.tool_lines.is_empty(), "tool lines must be untouched");
        assert!(s.overlay.is_none(), "diff proposed must not open an overlay");
        assert!(s.turn_active);
    }

    #[test]
    fn compaction_started_sets_compacting_notice() {
        let mut s = test_state();
        let effects = s.apply_event(AppEvent::CompactionStarted);
        assert!(effects.is_empty());
        assert_eq!(s.notice.as_deref(), Some("compacting context…"));
    }

    #[test]
    fn compaction_done_sets_freed_tokens_notice() {
        let mut s = test_state();
        s.apply_event(AppEvent::CompactionStarted);
        let effects = s.apply_event(AppEvent::CompactionDone { freed_tokens: 4096 });
        assert!(effects.is_empty());
        assert_eq!(s.notice.as_deref(), Some("compacted (-4096 tok)"));
    }

    fn checkpoint_list() -> AppEvent {
        AppEvent::CheckpointList(vec![
            CheckpointView { id: "turn-3".into(), turn: 3 },
            CheckpointView { id: "turn-2".into(), turn: 2 },
            CheckpointView { id: "turn-1".into(), turn: 1 },
        ])
    }

    #[test]
    fn checkpoint_list_opens_rewind_picker_and_select_sends_rewind() {
        let mut s = test_state();
        assert!(s.apply_event(checkpoint_list()).is_empty());
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.kind, PickerKind::Rewind);
                assert_eq!(p.items.len(), 3);
                assert_eq!(p.items[0].label, "turn 3");
                assert_eq!(p.selected, 0);
            }
            _ => panic!("expected the rewind picker overlay"),
        }

        s.overlay_picker_move(1);
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            [Effect::Send(Action::Rewind { checkpoint_id })] => {
                assert_eq!(checkpoint_id, "turn-2")
            }
            _ => panic!("expected a Rewind send"),
        }
        assert!(s.overlay.is_none(), "selection closes the picker");
    }

    #[test]
    fn picker_overlay_move_wraps_and_cancel_closes_without_effects() {
        let mut s = test_state();
        s.apply_event(checkpoint_list());
        s.overlay_picker_move(-1);
        match &s.overlay {
            Some(Overlay::Picker(p)) => assert_eq!(p.selected, 2, "wraps to the last entry"),
            _ => panic!("picker expected"),
        }
        s.overlay_picker_move(1);
        match &s.overlay {
            Some(Overlay::Picker(p)) => assert_eq!(p.selected, 0, "wraps back to the first"),
            _ => panic!("picker expected"),
        }
        s.overlay_close();
        assert!(s.overlay.is_none(), "cancel closes the picker");
    }

    #[test]
    fn session_list_opens_resume_picker_and_select_sends_resume() {
        let mut s = test_state();
        s.apply_event(AppEvent::SessionList(vec![stepper_protocol::SessionView {
            id: "abc".into(),
            name: Some("earlier".into()),
            digest: "fix the bug".into(),
            turns: 2,
            age: "3m ago".into(),
        }]));
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.kind, PickerKind::Resume);
                assert!(p.items[0].label.contains("earlier"), "label: {}", p.items[0].label);
                assert!(p.items[0].label.contains("fix the bug"));
                assert!(p.items[0].label.contains("3m ago"));
            }
            _ => panic!("expected the resume picker overlay"),
        }
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            [Effect::Send(Action::Resume { session_id })] => assert_eq!(session_id, "abc"),
            _ => panic!("expected a Resume send"),
        }
        assert!(s.overlay.is_none());
    }

    #[test]
    fn builtin_overlays_never_clobber_a_live_approval() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        let (reply, _rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Command { cmd: "ls".into(), outside_project: false },
            reply,
        }));
        s.apply_event(checkpoint_list());
        assert!(
            matches!(s.overlay, Some(Overlay::Approval(_))),
            "the approval (and its oneshot) must stay on screen"
        );
    }

    #[test]
    fn closing_a_builtin_overlay_surfaces_a_queued_approval() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        s.apply_event(checkpoint_list());
        let (reply, _rx) = oneshot::channel();
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Command { cmd: "ls".into(), outside_project: false },
            reply,
        }));
        assert_eq!(s.pending_approvals.len(), 1, "approval queued behind the picker");
        s.overlay_close();
        assert!(
            matches!(s.overlay, Some(Overlay::Approval(_))),
            "the queued approval surfaces when the picker closes"
        );
    }

    #[test]
    fn context_and_permissions_events_open_their_overlays() {
        let mut s = test_state();
        s.apply_event(AppEvent::ContextBreakdown(
            stepper_protocol::ContextBreakdownView {
                context_limit: 1000,
                free: 900,
                ..Default::default()
            },
        ));
        assert!(matches!(s.overlay, Some(Overlay::Context(_))));
        s.overlay_close();
        s.apply_event(AppEvent::PermissionsSnapshot(
            stepper_protocol::PermissionsSnapshotView {
                mode: "plan".into(),
                ..Default::default()
            },
        ));
        assert!(matches!(s.overlay, Some(Overlay::Permissions(_))));
    }

    #[test]
    fn session_resumed_clears_live_state_and_sets_notice() {
        let mut s = test_state();
        s.apply_event(AppEvent::AssistantTokenDelta("stale".into()));
        s.apply_event(AppEvent::TodoUpdated(vec![stepper_protocol::TodoItemView {
            id: "0".into(),
            content: "old todo".into(),
            status: stepper_protocol::TodoStatus::Pending,
        }]));
        s.usage.tokens_in = 99;
        s.apply_event(AppEvent::SessionResumed {
            id: "abc".into(),
            name: Some("earlier".into()),
            turns: 3,
        });
        assert!(s.live.assistant.is_empty());
        assert!(s.todos.is_empty());
        assert_eq!(s.usage.tokens_in, 0, "stale usage cleared");
        assert_eq!(s.notice.as_deref(), Some("resumed session earlier (3 turn(s))"));
    }

    #[test]
    fn esc_esc_on_empty_input_opens_the_rewind_picker_via_core() {
        let mut s = test_state();
        // First Esc: a plain interrupt, arming the chord.
        let effects = s.apply_action(Action::Interrupt);
        assert!(matches!(effects.as_slice(), [Effect::Send(Action::Interrupt)]));
        assert!(s.esc_armed);
        // Second Esc: the /rewind built-in (the checkpoint list arrives as an event).
        let effects = s.apply_action(Action::Interrupt);
        match effects.as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "rewind");
                assert!(args.is_empty());
            }
            _ => panic!("expected the /rewind slash command"),
        }
        assert!(!s.esc_armed, "the chord resets after firing");
    }

    #[test]
    fn esc_esc_requires_empty_input() {
        let mut s = test_state();
        s.textarea.insert_str("draft");
        let effects = s.apply_action(Action::Interrupt);
        assert!(matches!(effects.as_slice(), [Effect::Send(Action::Interrupt)]));
        assert!(!s.esc_armed, "a non-empty input never arms the chord");
        let effects = s.apply_action(Action::Interrupt);
        assert!(
            matches!(effects.as_slice(), [Effect::Send(Action::Interrupt)]),
            "still a plain interrupt"
        );
    }

    #[test]
    fn any_other_action_breaks_the_esc_esc_chord() {
        let mut s = test_state();
        s.apply_action(Action::Interrupt);
        assert!(s.esc_armed);
        s.apply_action(Action::CycleMode);
        assert!(!s.esc_armed, "another action disarms the chord");
        let effects = s.apply_action(Action::Interrupt);
        assert!(
            matches!(effects.as_slice(), [Effect::Send(Action::Interrupt)]),
            "the next Esc starts a fresh chord"
        );
    }

    fn worker_started(index: usize, total: usize, label: &str) -> AppEvent {
        AppEvent::WorkerStarted {
            index,
            total,
            label: label.into(),
            model: ModelView { provider: "omlx".into(), model: "qwen3".into() },
        }
    }

    #[test]
    fn worker_events_build_update_and_finish_panel_rows() {
        let mut s = test_state();
        // run_parallel announces in index order; index 0 opens the batch.
        s.apply_event(worker_started(0, 2, "api"));
        s.apply_event(worker_started(1, 2, "db"));
        assert_eq!(s.workers.len(), 2);
        assert_eq!(s.workers[0].index, 0);
        assert_eq!(s.workers[0].label, "api");
        assert_eq!(s.workers[1].index, 1);

        s.apply_event(AppEvent::WorkerActivity {
            index: 1,
            tokens: Some(1234),
            tool: Some("edit_file: db.rs".into()),
        });
        let w = s.workers.iter().find(|w| w.index == 1).unwrap();
        assert_eq!(w.tokens, 1234);
        assert_eq!(w.last_tool.as_deref(), Some("edit_file: db.rs"));
        assert_eq!(w.status, LayerStatus::Running);

        s.apply_event(AppEvent::WorkerFinished { index: 0, status: LayerStatus::Done });
        assert_eq!(s.workers.iter().find(|w| w.index == 0).unwrap().status, LayerStatus::Done);
    }

    #[test]
    fn worker_activity_with_none_fields_preserves_existing_values() {
        let mut s = test_state();
        s.apply_event(worker_started(0, 1, "x"));
        s.apply_event(AppEvent::WorkerActivity { index: 0, tokens: Some(10), tool: Some("read: a".into()) });
        // a token-only update must not wipe the last tool, and vice-versa.
        s.apply_event(AppEvent::WorkerActivity { index: 0, tokens: Some(99), tool: None });
        let w = &s.workers[0];
        assert_eq!(w.tokens, 99);
        assert_eq!(w.last_tool.as_deref(), Some("read: a"), "None tool must not clear last_tool");
    }

    #[test]
    fn a_new_batch_resets_the_panel_on_worker_index_zero() {
        let mut s = test_state();
        // batch 1: three workers, all done (e.g. a first `dispatch` call)
        s.apply_event(worker_started(0, 3, "a"));
        s.apply_event(worker_started(1, 3, "b"));
        s.apply_event(worker_started(2, 3, "c"));
        for i in 0..3 {
            s.apply_event(AppEvent::WorkerFinished { index: i, status: LayerStatus::Done });
        }
        assert_eq!(s.workers.len(), 3);
        // batch 2 in the SAME layer restarts at index 0 — it must replace, not append.
        s.apply_event(worker_started(0, 2, "x"));
        s.apply_event(worker_started(1, 2, "y"));
        assert_eq!(s.workers.len(), 2, "index 0 cleared batch 1's ghost rows");
        assert_eq!(s.workers[1].label, "y");
    }

    #[test]
    fn a_new_layer_and_turn_end_clear_the_worker_panel() {
        let mut s = test_state();
        s.apply_event(worker_started(0, 2, "api"));
        s.apply_event(worker_started(1, 2, "db"));
        assert_eq!(s.workers.len(), 2);
        s.apply_event(AppEvent::LayerStarted { index: 2, total: 3, name: "test".into() });
        assert!(s.workers.is_empty(), "a new layer ends the prior fan-out's panel");

        s.apply_event(worker_started(0, 1, "again"));
        assert_eq!(s.workers.len(), 1);
        s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(s.workers.is_empty(), "turn end clears the panel");
    }
}
