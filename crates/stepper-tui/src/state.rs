use ratatui::style::Style;
use ratatui_textarea::{TextArea, WrapMode};
use smallvec::SmallVec;
use unicode_width::UnicodeWidthStr;
use std::collections::VecDeque;
use std::path::PathBuf;
use stepper_protocol::{
    Action, AppEvent, ApprovalRequest, CheckpointView, ContextBreakdownView, LayerStatus,
    LayerView, Mode, ModelChoiceView, ModelView, NoticeLevel, PermissionsSnapshotView,
    ProviderChoiceView, QuestionRequest, RewindScope, SessionView, SettingsSnapshotView,
    TodoItemView, UsageView, WorkerView,
};

use crate::{AgentInfo, CommandInfo, TuiInit};

/// Side effects the event loop must execute after a state transition (state.rs
/// itself stays IO-free and synchronous, so it's trivially unit-testable).
pub enum Effect {
    /// Forward an action to core over the action channel.
    Send(Action),
    /// Commit a finalized assistant turn (markdown source) into native
    /// scrollback via `Terminal::insert_before`.
    CommitToScrollback(String),
    /// Purge the terminal scrollback (`/clear`) so the prior conversation
    /// disappears, then repaint the viewport fresh.
    ClearScreen,
    /// Ring the terminal bell (`\x07`) — a turn finished, an approval is awaited,
    /// or a turn errored, and the matching `notification` trigger is enabled.
    Bell,
    /// Persist the prompt history to `path` (the event loop does the IO so
    /// `state.rs` stays pure). Carries the full capped list, written as a JSON array.
    PersistHistory { path: PathBuf, lines: Vec<String> },
    /// Write this text (the last reply, via `/copy`) to the OS clipboard. The
    /// arboard call lives in the event loop so `state.rs` stays IO-free.
    CopyToClipboard(String),
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

/// A status notice with its severity, so the render can color it and always show
/// it (an error must not look like a routine info message, nor vanish mid-turn).
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
}

/// An informational notice (the default severity for internal status messages).
fn info_notice(text: impl Into<String>) -> Notice {
    Notice {
        level: NoticeLevel::Info,
        text: text.into(),
    }
}

pub enum Overlay {
    Approval(ApprovalRequest),
    /// `/context` — the estimated window decomposition (any-key dismiss).
    Context(ContextBreakdownView),
    /// `/permissions` — the read-only rules/approvals snapshot (any-key dismiss).
    Permissions(PermissionsSnapshotView),
    /// The generic list picker (rewind checkpoints / resume sessions / models).
    Picker(ListPicker),
    /// API-key entry for a provider (`/login`, or a keyless model switch). Keys
    /// are typed in masked; Enter sends `Action::SetApiKey`, Esc cancels.
    ApiKey(ApiKeyOverlay),
    /// The `/connect` "add custom provider" form: name + base URL text fields
    /// and a wire-type selector. Tab/↑↓ move fields, ←/→ cycle the type, Enter
    /// submits (`Action::ConnectCustom`), Esc cancels.
    ConnectCustom(ConnectCustomOverlay),
    /// The background-process "shell view" (Down key): up/down selects a process,
    /// `k` kills it, Esc/q closes.
    Shell(ShellView),
    /// `/theme` — the color-theme editor: row 0 cycles the preset (←/→), the
    /// rest edit each color role's hex/name inline. Enter saves, Esc cancels.
    Theme(ThemeState),
    /// `/settings` — the tabbed read-only settings overview. ←/→ switch tabs,
    /// Enter opens the focused tab's editor (jump), Esc closes.
    Settings(SettingsView),
    /// Ctrl+R — reverse search over the prompt history. Typing filters; ↑/↓ or a
    /// repeated Ctrl+R move the selection; Enter loads the entry into the input,
    /// Esc cancels. Holds query + selection; the matched entries live in
    /// `AppState.history` (indexed by `matches`).
    HistorySearch(HistorySearch),
    /// `ask_user_question` — a model-asked multiple-choice question. Number keys /
    /// ↑↓+Enter pick an option (replying through the embedded oneshot); Esc cancels.
    Question(QuestionView),
}

/// `ask_user_question` overlay state: the live request plus the highlighted option.
pub struct QuestionView {
    pub req: QuestionRequest,
    pub selected: usize,
}

impl QuestionView {
    fn move_sel(&mut self, delta: i32) {
        let n = self.req.options.len() as i32;
        if n > 0 {
            self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
        }
    }
}

/// Reverse-search overlay state (Ctrl+R). `matches` are indices into
/// `AppState.history`, most-recent first, narrowed by `query`.
pub struct HistorySearch {
    pub query: String,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl HistorySearch {
    fn move_sel(&mut self, delta: i32) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as i32;
        self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
    }
}

/// `/settings` overlay state: the snapshot plus the focused tab index.
pub struct SettingsView {
    pub snapshot: SettingsSnapshotView,
    pub tab: usize,
}

impl SettingsView {
    fn move_tab(&mut self, delta: i32) {
        let n = self.snapshot.tabs.len() as i32;
        if n > 0 {
            self.tab = (((self.tab as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// The slash command to open the focused tab's dedicated editor, if any.
    fn jump(&self) -> Option<Action> {
        self.snapshot.tabs.get(self.tab)?.jump.as_ref().map(|name| Action::SlashCommand {
            name: name.clone(),
            args: String::new(),
        })
    }
}

/// Color-theme editor state. Row 0 is the preset selector; rows `1..=colors.len`
/// edit each color role's value string. The live theme is derived from this.
pub struct ThemeState {
    presets: Vec<String>,
    pub preset_idx: usize,
    /// `(role name, editable color string)` per editable color, in display order.
    pub colors: Vec<(String, String)>,
    /// 0 = preset row; `1..=colors.len()` = color rows.
    pub selected: usize,
    /// True until the first keystroke after (re)selecting a color row, so typing
    /// replaces the existing value instead of appending to it.
    fresh: bool,
    /// The theme at open time, restored on cancel (Esc).
    saved: crate::theme::Theme,
}

impl ThemeState {
    /// Seed the editor from the current preset name + live theme colors.
    pub fn new(preset: &str, theme: &crate::theme::Theme) -> Self {
        let presets: Vec<String> = crate::theme::PRESET_NAMES.iter().map(|s| s.to_string()).collect();
        let preset_idx = presets.iter().position(|p| p == preset).unwrap_or(0);
        let colors = theme
            .color_fields()
            .iter()
            .map(|(name, c)| (name.to_string(), crate::theme::Theme::color_to_string(*c)))
            .collect();
        ThemeState { presets, preset_idx, colors, selected: 0, fresh: true, saved: theme.clone() }
    }

    pub fn preset_name(&self) -> &str {
        &self.presets[self.preset_idx]
    }

    /// Total rows: the preset selector plus one per color.
    pub fn rows(&self) -> usize {
        1 + self.colors.len()
    }

    pub fn move_sel(&mut self, delta: i32) {
        let n = self.rows() as i32;
        self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
        self.fresh = true;
    }

    /// On the preset row, cycle the preset and reset the colors to its palette so
    /// the editor reflects the chosen base.
    pub fn cycle_preset(&mut self, delta: i32) {
        if self.selected != 0 {
            return;
        }
        let n = self.presets.len() as i32;
        self.preset_idx = (((self.preset_idx as i32 + delta) % n + n) % n) as usize;
        if let Some(theme) = crate::theme::Theme::preset(self.preset_name()) {
            self.colors = theme
                .color_fields()
                .iter()
                .map(|(name, c)| (name.to_string(), crate::theme::Theme::color_to_string(*c)))
                .collect();
        }
    }

    /// Edit the selected color row's value (no-op on the preset row). The first
    /// keystroke after selecting a row replaces the existing value.
    pub fn push_char(&mut self, c: char) {
        let fresh = self.fresh;
        if let Some(row) = self.selected.checked_sub(1)
            && let Some(entry) = self.colors.get_mut(row)
        {
            if fresh {
                entry.1.clear();
            }
            entry.1.push(c);
            self.fresh = false;
        }
    }

    pub fn backspace(&mut self) {
        if let Some(row) = self.selected.checked_sub(1)
            && let Some(entry) = self.colors.get_mut(row)
        {
            entry.1.pop();
            self.fresh = false;
        }
    }

    /// The live theme described by the editor (edited values win over the preset).
    pub fn build_theme(&self) -> crate::theme::Theme {
        crate::theme::Theme::resolve(Some(self.preset_name()), &self.colors)
    }

    /// Persistable per-role overrides: only the colors that differ from the
    /// chosen preset's value (keeps `setting.json` minimal).
    pub fn overrides(&self) -> Vec<(String, String)> {
        let preset = crate::theme::Theme::preset(self.preset_name()).unwrap_or_default();
        let base: std::collections::HashMap<&str, String> = preset
            .color_fields()
            .iter()
            .map(|(n, c)| (*n, crate::theme::Theme::color_to_string(*c)))
            .collect();
        self.colors
            .iter()
            .filter(|(name, value)| {
                // Keep an override only when it parses AND differs from the preset.
                crate::theme::Theme::parse_color(value).is_some()
                    && base.get(name.as_str()).map(|b| b != value).unwrap_or(true)
            })
            .cloned()
            .collect()
    }
}

/// Shell-view overlay state: which process row is highlighted.
pub struct ShellView {
    pub selected: usize,
}

/// Status of a `!cmd &` background process in the shell view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcStatus {
    Running,
    Exited(Option<i32>),
}

/// A background process tracked for the shell view: its command, status, and a
/// bounded tail of its console output.
pub struct ProcView {
    pub id: u64,
    pub command: String,
    pub status: ProcStatus,
    pub output: VecDeque<String>,
}

/// How many recent output lines each background process retains (a ring buffer,
/// so a chatty `bun dev` can't grow unbounded).
const PROC_OUTPUT_TAIL: usize = 500;

/// The masked API-key entry overlay state.
pub struct ApiKeyOverlay {
    pub provider: String,
    pub input: String,
}

/// Wire-type choices of the custom-provider form, in display order: `openai` =
/// OpenAI-compatible chat/completions, `claude` = Anthropic Messages, `custom`
/// = start as openai-compat and hand-edit `setting.json` for anything else.
pub const CUSTOM_PROVIDER_FLAVORS: [&str; 3] = ["openai", "claude", "custom"];

/// Rows of the custom-provider form: name, host, type.
const CUSTOM_PROVIDER_FIELDS: usize = 3;

/// Cap on the form's text fields — longer than any real name/URL, short enough
/// that an accidental huge paste cannot thrash the per-tick re-render.
const CUSTOM_PROVIDER_FIELD_MAX: usize = 2048;

/// Provider-name length cap, mirroring core's `scaffold::is_safe_name` (the TUI
/// cannot import stepper-config — protocol-only isolation) so a too-long name
/// is caught while the form is still open instead of by a core warn after it
/// closed and the typed input was lost.
const CUSTOM_PROVIDER_NAME_MAX: usize = 64;

/// The `/connect` custom-provider form state: two text fields + a type selector.
#[derive(Default)]
pub struct ConnectCustomOverlay {
    pub name: String,
    pub host: String,
    /// Index into [`CUSTOM_PROVIDER_FLAVORS`].
    pub flavor_idx: usize,
    /// Focused row: 0 = name, 1 = host, 2 = type.
    pub field: usize,
}

/// What a list-picker selection turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// `/rewind` checkpoint picker; carries the restore scope from `/rewind
    /// [code|conversation]` so the selection applies it.
    Rewind(RewindScope),
    Resume,
    Model,
    Connect,
    /// The auth-method chooser for a multi-route provider (`/connect openai`):
    /// api key · ChatGPT OAuth · access token. Item ids are full `/connect`
    /// argument strings (`openai --auth api-key`).
    Auth,
    Effort,
}

/// The generic list-picker overlay shared by `/rewind` (checkpoints) and
/// `/resume` (sessions): up/down moves with wrap, Enter sends the selection to
/// core as an `Action`, Esc cancels.
pub struct ListPicker {
    pub kind: PickerKind,
    pub items: Vec<ListPickerItem>,
    /// Type-to-filter query (searchable kinds only). `matches` indexes `items`.
    pub query: String,
    pub matches: Vec<usize>,
    /// Index into `matches` (the filtered view), not `items`.
    pub selected: usize,
}

pub struct ListPickerItem {
    /// The id sent back to core (`turn-N` checkpoint id / session id).
    pub id: String,
    /// The rendered row.
    pub label: String,
    /// Whether this row can be acted on. Disabled rows (e.g. a `/connect`
    /// provider with no resolvable API base) render dimmed and ignore Enter.
    pub connectable: bool,
}

impl ListPicker {
    pub fn new(kind: PickerKind, items: Vec<ListPickerItem>) -> Self {
        let matches = (0..items.len()).collect();
        ListPicker {
            kind,
            items,
            query: String::new(),
            matches,
            selected: 0,
        }
    }

    pub fn title(&self) -> &'static str {
        match self.kind {
            PickerKind::Rewind(_) => " rewind ",
            PickerKind::Resume => " resume ",
            PickerKind::Model => " models ",
            PickerKind::Connect => " connect ",
            PickerKind::Auth => " auth method ",
            PickerKind::Effort => " effort ",
        }
    }

    /// Whether typing filters this picker. The model/provider lists are large
    /// and session labels carry names + digests, so they are type-to-filter;
    /// rewind rows are just "turn N" and stay strict.
    pub fn searchable(&self) -> bool {
        matches!(self.kind, PickerKind::Model | PickerKind::Connect | PickerKind::Resume)
    }

    /// Append a char to the filter query and re-filter (searchable kinds only).
    pub fn push_query(&mut self, c: char) {
        if self.searchable() {
            self.query.push(c);
            self.update_filter();
        }
    }

    /// Drop the last query char and re-filter. Returns whether anything changed.
    pub fn pop_query(&mut self) -> bool {
        if self.searchable() && self.query.pop().is_some() {
            self.update_filter();
            return true;
        }
        false
    }

    fn update_filter(&mut self) {
        let needle = self.query.to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| needle.is_empty() || it.label.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as i32;
        self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
    }

    /// Whether the row under the cursor exists but is disabled (an unsupported
    /// `/connect` provider). Distinct from an empty match set, so the caller can
    /// keep the picker open on a no-op Enter yet still close an empty picker.
    fn is_selected_disabled(&self) -> bool {
        match self.matches.get(self.selected) {
            Some(&idx) => self.items.get(idx).map(|it| !it.connectable).unwrap_or(false),
            None => false,
        }
    }

    fn selection(&self) -> Option<Action> {
        let item = self.items.get(*self.matches.get(self.selected)?)?;
        // A disabled row (an unsupported `/connect` provider) is shown for context
        // but can't be acted on — Enter is a no-op rather than firing a doomed
        // connect that the resolver would only reject.
        if !item.connectable {
            return None;
        }
        Some(match self.kind {
            PickerKind::Rewind(scope) => Action::Rewind {
                checkpoint_id: item.id.clone(),
                scope,
            },
            PickerKind::Resume => Action::Resume {
                session_id: item.id.clone(),
            },
            // Reuse the existing `/model <ref>` switch path (validate + emit
            // ModelChanged); the item id is the `provider/model-id` ref.
            PickerKind::Model => Action::SlashCommand {
                name: "model".into(),
                args: item.id.clone(),
            },
            // Route back through `/connect <id>` (register + prompt for the key);
            // the item id is the catalog provider id.
            PickerKind::Connect => Action::SlashCommand {
                name: "connect".into(),
                args: item.id.clone(),
            },
            // The item id is the full argument string (`openai --auth chatgpt`).
            PickerKind::Auth => Action::SlashCommand {
                name: "connect".into(),
                args: item.id.clone(),
            },
            // Reuse `/effort <level>`; the item id is the effort level string.
            PickerKind::Effort => Action::SlashCommand {
                name: "effort".into(),
                args: item.id.clone(),
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

/// `#`-triggered named-agent autocomplete picker. Unlike the file picker it does
/// no IO — it filters a static list (the configured `.stepper/agents/`) by a
/// substring query on the agent name; selecting an entry inserts `#name ` into
/// the prompt (the `#agent` sub-agent trigger that core already routes).
pub struct AgentPicker {
    pub query: String,
    items: Vec<AgentInfo>,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl AgentPicker {
    fn new(agents: &[AgentInfo]) -> Self {
        let items = agents.to_vec();
        let matches = (0..items.len()).collect();
        AgentPicker { query: String::new(), items, matches, selected: 0 }
    }

    fn move_sel(&mut self, delta: i32) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as i32;
        self.selected = (((self.selected as i32 + delta) % n + n) % n) as usize;
    }

    fn refilter(&mut self) {
        let needle = self.query.to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, a)| needle.is_empty() || a.name.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = 0;
        }
    }

    fn current(&self) -> Option<&AgentInfo> {
        self.matches.get(self.selected).map(|&i| &self.items[i])
    }

    /// The agent at filtered row `row`, for the renderer.
    pub fn entry(&self, row: usize) -> Option<&AgentInfo> {
        self.matches.get(row).map(|&i| &self.items[i])
    }
}

/// Pure render state. No IO, no awaits — `apply_event` / `apply_action` mutate it
/// and return `Effects` for the loop to run.
pub struct AppState {
    pub live: StreamBuf,
    pub tool_lines: Vec<String>,
    /// Maps a tool-call id to the index of its `▸ …` line in `tool_lines`, so a
    /// `ToolCallFinished` flips that line's glyph (▸→✓/✗) in place instead of
    /// appending a separate `✓ tool <uuid> finished` row.
    tool_index: std::collections::HashMap<String, usize>,
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
    /// `ask_user_question` requests that arrived while another overlay was open;
    /// surfaced one at a time after approvals (their oneshots are time-sensitive).
    pub pending_questions: VecDeque<QuestionRequest>,
    /// API-key prompts (provider names) that arrived while another overlay was on
    /// screen — surfaced one at a time on `overlay_close`, after approvals, so a
    /// prompt racing an approval (e.g. the auto `/login` on launch) is never lost.
    pub pending_prompts: VecDeque<String>,
    pub picker: Option<FilePicker>,
    /// Named sub-agents for the `#`-agent autocomplete picker (static, from init).
    pub agents: Vec<AgentInfo>,
    /// The open `#`-agent picker (IO-free; filters `agents`), if any.
    pub agent_picker: Option<AgentPicker>,
    /// Background processes (`!cmd &`) shown in the shell view (Down key).
    pub processes: Vec<ProcView>,
    /// Images pasted (Ctrl+V) and staged for the next prompt — shown as an input
    /// indicator. Cleared on submit (they ride with that turn).
    pub pending_image_count: usize,
    pub queue: VecDeque<Queued>,
    pub notice: Option<Notice>,
    pub spinner: usize,
    /// Live-region scrollback offset: rows scrolled UP from the bottom (0 =
    /// pinned to the latest output). PgUp/PgDn and the mouse wheel adjust it.
    pub scroll_offset: u16,
    /// Mirror of the input `textarea`'s internal vertical scroll offset, tracked
    /// with the widget's own `next_scroll_top` rule so the real terminal cursor
    /// lands on the row the widget actually drew it — the widget scrolls when a
    /// long multi-line prompt overflows the box, and its viewport is not public.
    pub input_scroll_top: std::cell::Cell<u16>,
    pub turn_active: bool,
    pub cwd: PathBuf,
    /// Known slash commands (name + description) for the `/` palette.
    pub commands: Vec<CommandInfo>,
    pub palette_selected: usize,
    /// A first Esc on empty input arms this; a second Esc (before any other
    /// action/typing) opens the rewind picker (Esc-Esc, Claude-Code-style).
    pub esc_armed: bool,
    /// A first Ctrl+C with something to lose (turn/queue/draft) arms this; the
    /// second one quits. Any other key disarms it.
    ctrl_c_armed: bool,
    pub should_quit: bool,
    /// The live color theme (rendered everywhere) and the preset it derives from.
    /// The `/theme` editor mutates these; persistence rides an `Action::SetTheme`.
    pub theme: crate::theme::Theme,
    pub theme_preset: String,
    /// Session reasoning-effort level (`None` = off), shown in the status footer;
    /// updated by `AppEvent::EffortChanged` from `/effort`.
    pub effort: Option<String>,
    /// Terminal-bell triggers from the `notification` setting (turn-complete /
    /// approval-awaited / turn-error). Each gates an `Effect::Bell`.
    pub notify_on_complete: bool,
    pub notify_on_approval: bool,
    pub notify_on_error: bool,
    /// Set when the current turn emitted an error, so the `TurnComplete` that
    /// always follows it does not ALSO ring the complete-bell (core sends both —
    /// a failed turn would otherwise double-beep). Error and complete bells are
    /// mutually exclusive per turn; the error wins.
    errored_this_turn: bool,
    /// Pending request to open the external editor (`/editor` or Ctrl+E), with the
    /// seed text for the temp file. The event loop drains it via
    /// [`AppState::take_editor_request`] because spawning the editor needs the
    /// terminal/reader handles it owns. `None` when no request is pending.
    editor_request: Option<String>,
    /// Submitted prompts (oldest → newest), recalled with ↑/↓ and Ctrl+R. Loaded
    /// from `history_path` at startup and appended on each submit.
    pub history: Vec<String>,
    /// Where the history persists (`~/.stepper/history/<project>.json`). `None`
    /// keeps history in-memory only (no `$HOME`); the event loop does the IO.
    history_path: Option<PathBuf>,
    /// Current position while browsing history with ↑/↓ (index into `history`);
    /// `None` when editing fresh input rather than recalling.
    history_nav: Option<usize>,
    /// The in-progress input stashed when ↑ first enters history browsing, so ↓
    /// past the newest entry restores what the user was typing.
    history_draft: Option<String>,
    /// Custom status-line command (`settings.statusLine`); `None` = built-in footer.
    pub status_line_cmd: Option<Vec<String>>,
    /// Latest stdout (first line) from the status-line command, rendered in place
    /// of the built-in footer. `None` until the first run produces output.
    pub status_line: Option<String>,
    /// User key bindings (additive over the built-in defaults), consulted first
    /// by `lower_event`.
    pub keybindings: crate::keybindings::KeyBindings,
}

/// Cap on persisted prompt history (newest kept). Bounds the on-disk file and
/// the in-memory list.
const HISTORY_MAX: usize = 500;

/// Longest notice that still reads on one status row; anything longer (or any
/// multi-line text, e.g. /help) is committed to scrollback in full instead of
/// being truncated into uselessness.
const NOTICE_INLINE_MAX: usize = 160;

/// Cap on the API-key overlay input: real keys are far shorter, and the cap
/// stops an accidental huge paste from thrashing the per-tick masked re-render.
const API_KEY_INPUT_MAX: usize = 8192;

/// The armed-quit prompt (matched on disarm so it never lingers).
const QUIT_CONFIRM_NOTICE: &str = "press Ctrl+C again to quit";

/// Markdown-proof a plain-text notice for scrollback: escape `<` (tui-markdown
/// drops `<arg>` hints as inline HTML) and turn newlines into hard breaks (a
/// bare newline is a CommonMark soft break — the whole text would collapse
/// into one space-joined paragraph).
fn scrollback_notice_md(text: &str) -> String {
    text.replace('<', "\\<").lines().collect::<Vec<_>>().join("  \n")
}

/// A blank input textarea configured the way every fresh prompt needs it:
/// no emulated cursor (the event loop draws the real one for correct CJK / wide
/// alignment) and soft word-wrapping so long input flows onto extra visual rows
/// instead of overflowing the box (the input area grows to fit — see
/// `render::input_height`).
pub(crate) fn fresh_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_cursor_style(Style::default());
    textarea.set_cursor_line_style(Style::default());
    textarea.set_wrap_mode(WrapMode::WordOrGlyph);
    textarea
}

impl AppState {
    pub fn new(init: TuiInit) -> Self {
        Self {
            live: StreamBuf::default(),
            tool_lines: Vec::new(),
            tool_index: std::collections::HashMap::new(),
            textarea: fresh_textarea(),
            mode: init.mode,
            model: init.model,
            usage: UsageView::default(),
            active_layer: None,
            workers: Vec::new(),
            todos: Vec::new(),
            overlay: None,
            pending_approvals: VecDeque::new(),
            pending_questions: VecDeque::new(),
            pending_prompts: VecDeque::new(),
            picker: None,
            agents: init.agents,
            agent_picker: None,
            processes: Vec::new(),
            pending_image_count: 0,
            queue: VecDeque::new(),
            notice: None,
            spinner: 0,
            scroll_offset: 0,
            input_scroll_top: std::cell::Cell::new(0),
            turn_active: false,
            cwd: init.cwd,
            theme: crate::theme::Theme::resolve(init.theme_preset.as_deref(), &init.theme_colors),
            theme_preset: init.theme_preset.unwrap_or_else(|| "dark".to_string()),
            effort: init.effort,
            notify_on_complete: init.notify_on_complete,
            notify_on_approval: init.notify_on_approval,
            notify_on_error: init.notify_on_error,
            errored_this_turn: false,
            editor_request: None,
            history: Vec::new(),
            history_path: init.history_path,
            history_nav: None,
            history_draft: None,
            status_line_cmd: init.status_line_cmd,
            status_line: None,
            keybindings: crate::keybindings::KeyBindings::from_overrides(&init.keybindings),
            commands: init.commands,
            palette_selected: 0,
            esc_armed: false,
            ctrl_c_armed: false,
            should_quit: false,
        }
    }

    /// Open the `/theme` color editor seeded from the live theme.
    pub fn open_theme_editor(&mut self) {
        let editor = ThemeState::new(&self.theme_preset, &self.theme);
        self.open_overlay(Overlay::Theme(editor));
    }

    /// Move the editor cursor and refresh the live preview.
    pub fn theme_editor_move(&mut self, delta: i32) {
        if let Some(Overlay::Theme(ts)) = &mut self.overlay {
            ts.move_sel(delta);
        }
    }

    /// Cycle the preset (preset row only) and refresh the live preview.
    pub fn theme_editor_cycle(&mut self, delta: i32) {
        let preview = if let Some(Overlay::Theme(ts)) = &mut self.overlay {
            ts.cycle_preset(delta);
            ts.build_theme()
        } else {
            return;
        };
        self.theme = preview;
    }

    /// Type into / edit the selected color row and refresh the live preview.
    pub fn theme_editor_edit(&mut self, c: Option<char>) {
        let preview = if let Some(Overlay::Theme(ts)) = &mut self.overlay {
            match c {
                Some(ch) => ts.push_char(ch),
                None => ts.backspace(),
            }
            ts.build_theme()
        } else {
            return;
        };
        self.theme = preview;
    }

    /// Commit the edited theme: apply it live and return the `Action::SetTheme`
    /// to persist (preset + minimal overrides). Closes the editor.
    pub fn theme_editor_save(&mut self) -> Option<Action> {
        let (preset, overrides, built) = if let Some(Overlay::Theme(ts)) = &self.overlay {
            (ts.preset_name().to_string(), ts.overrides(), ts.build_theme())
        } else {
            return None;
        };
        self.theme = built;
        self.theme_preset = preset.clone();
        self.overlay_close();
        Some(Action::SetTheme { preset: Some(preset), colors: overrides })
    }

    /// Abandon the edits, restoring the theme that was live when the editor opened.
    pub fn theme_editor_cancel(&mut self) {
        let saved = match &self.overlay {
            Some(Overlay::Theme(ts)) => Some(ts.saved.clone()),
            _ => None,
        };
        if let Some(theme) = saved {
            self.theme = theme;
        }
        self.overlay_close();
    }

    pub fn input_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// The JSON context piped to the custom status-line command's stdin: the live
    /// model, mode, cwd, token usage, and session cost (Claude-Code-style).
    pub fn status_line_context(&self) -> serde_json::Value {
        serde_json::json!({
            "model": { "provider": self.model.provider, "model": self.model.model },
            "mode": self.mode.label(),
            "cwd": self.cwd.display().to_string(),
            "tokens": {
                "used": self.usage.context_used,
                "limit": self.usage.context_limit,
            },
            "cost_usd": self.usage.cost_usd,
            "turn_active": self.turn_active,
        })
    }

    /// Take the pending `/editor`/Ctrl+E request (the seed text), if any. The
    /// event loop calls this each iteration and, when it returns `Some`, hands the
    /// terminal to `$EDITOR`.
    pub fn take_editor_request(&mut self) -> Option<String> {
        self.editor_request.take()
    }

    /// Replace the input box with `text` (the external editor's saved buffer).
    pub fn set_input(&mut self, text: &str) {
        self.textarea = fresh_textarea();
        if !text.is_empty() {
            self.textarea.insert_str(text);
        }
    }

    /// Show a one-line info notice in the status area (e.g. editor diagnostics).
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(info_notice(text));
    }

    // ── prompt history (↑/↓ recall + Ctrl+R reverse search) ──

    /// Seed the in-memory history from disk (called once at startup, after the
    /// event loop has read the file). Already-capped on save, but re-cap defensively.
    pub fn set_history(&mut self, mut history: Vec<String>) {
        if history.len() > HISTORY_MAX {
            let overflow = history.len() - HISTORY_MAX;
            history.drain(0..overflow);
        }
        self.history = history;
    }

    /// Record a submitted prompt: reset the browse cursor, drop consecutive
    /// duplicates, cap, and return the persistence effect (when a path is set).
    fn record_history(&mut self, entry: String) -> Option<Effect> {
        self.history_nav = None;
        self.history_draft = None;
        if entry.trim().is_empty() || self.history.last() == Some(&entry) {
            return None;
        }
        self.history.push(entry);
        if self.history.len() > HISTORY_MAX {
            let overflow = self.history.len() - HISTORY_MAX;
            self.history.drain(0..overflow);
        }
        self.history_path
            .clone()
            .map(|path| Effect::PersistHistory { path, lines: self.history.clone() })
    }

    /// Recall the previous (older) history entry into the input. Returns whether
    /// the key was consumed (`false` → the caller forwards ↑ to the textarea so it
    /// still moves the cursor when there's no history).
    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let idx = match self.history_nav {
            // Entering browse mode: stash the in-progress draft first.
            None => {
                self.history_draft = Some(self.input_text());
                self.history.len() - 1
            }
            // Already at the oldest entry — stay put (don't fall off the end).
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_nav = Some(idx);
        self.set_input(&self.history[idx].clone());
        // Park the cursor on the first row so a subsequent ↑ (gated on row 0) keeps
        // walking older even through a multi-line recalled entry, instead of the
        // caret getting stuck mid-entry (`set_input` leaves it at the end).
        self.textarea.move_cursor(ratatui_textarea::CursorMove::Top);
        true
    }

    /// Recall the next (newer) history entry; past the newest, restore the stashed
    /// draft and leave browse mode. Returns whether the key was consumed.
    pub fn history_next(&mut self) -> bool {
        let Some(i) = self.history_nav else {
            return false;
        };
        if i + 1 < self.history.len() {
            self.history_nav = Some(i + 1);
            self.set_input(&self.history[i + 1].clone());
        } else {
            self.history_nav = None;
            let draft = self.history_draft.take().unwrap_or_default();
            self.set_input(&draft);
        }
        true
    }

    /// Open the Ctrl+R reverse-search overlay (a no-op notice when history is empty).
    pub fn open_history_search(&mut self) {
        if self.history.is_empty() {
            self.set_notice("no prompt history yet");
            return;
        }
        let mut search = HistorySearch { query: String::new(), matches: Vec::new(), selected: 0 };
        self.refilter_history_search(&mut search);
        self.open_overlay(Overlay::HistorySearch(search));
    }

    /// Recompute a search's matches (most-recent first, case-insensitive substring).
    fn refilter_history_search(&self, search: &mut HistorySearch) {
        let needle = search.query.to_lowercase();
        search.matches = self
            .history
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| needle.is_empty() || e.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if search.selected >= search.matches.len() {
            search.selected = 0;
        }
    }

    /// Type a char into the reverse-search query and re-filter.
    pub fn history_search_push(&mut self, c: char) {
        if let Some(Overlay::HistorySearch(mut search)) = self.overlay.take() {
            search.query.push(c);
            self.refilter_history_search(&mut search);
            self.overlay = Some(Overlay::HistorySearch(search));
        }
    }

    /// Backspace the reverse-search query and re-filter.
    pub fn history_search_backspace(&mut self) {
        if let Some(Overlay::HistorySearch(mut search)) = self.overlay.take() {
            search.query.pop();
            self.refilter_history_search(&mut search);
            self.overlay = Some(Overlay::HistorySearch(search));
        }
    }

    /// Move the reverse-search selection (Ctrl+R / ↑ / ↓).
    pub fn history_search_move(&mut self, delta: i32) {
        if let Some(Overlay::HistorySearch(search)) = &mut self.overlay {
            search.move_sel(delta);
        }
    }

    /// Accept the highlighted entry: load it into the input and close the overlay.
    pub fn history_search_accept(&mut self) {
        let pick = match &self.overlay {
            Some(Overlay::HistorySearch(s)) => s.matches.get(s.selected).map(|&i| self.history[i].clone()),
            _ => None,
        };
        if let Some(text) = pick {
            self.set_input(&text);
        }
        self.overlay_close();
    }

    /// The (entry, is_selected) rows for the reverse-search overlay renderer.
    pub fn history_search_rows(&self) -> Vec<(&str, bool)> {
        match &self.overlay {
            Some(Overlay::HistorySearch(s)) => s
                .matches
                .iter()
                .enumerate()
                .filter_map(|(row, &i)| self.history.get(i).map(|e| (e.as_str(), row == s.selected)))
                .collect(),
            _ => Vec::new(),
        }
    }

    // ── `/` slash-command palette (pure; the command list is supplied at init) ──

    /// Command names whose prefix matches the `/<partial>` currently typed.
    /// Empty unless the input is a single `/`-prefixed token with no whitespace
    /// yet (and no other overlay is open).
    pub fn command_matches(&self) -> Vec<&CommandInfo> {
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
            .filter(|c| c.name.starts_with(rest))
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
        let name = matches[idx].name.clone();
        self.textarea = fresh_textarea();
        self.textarea.insert_str(format!("/{name} "));
        self.palette_selected = 0;
    }

    /// The currently-highlighted palette command name (clamped to the match set),
    /// for Enter-to-run. None when the palette has no matches.
    pub fn palette_selected_command(&self) -> Option<String> {
        let matches = self.command_matches();
        if matches.is_empty() {
            return None;
        }
        let idx = self.palette_selected.min(matches.len() - 1);
        Some(matches[idx].name.clone())
    }

    /// Run the highlighted palette command now: clear the typed `/partial`, reset
    /// the selection, and emit the `SlashCommand` effect (Enter in the palette).
    pub fn palette_run_selected(&mut self) -> Effects {
        let mut effects = Effects::new();
        if let Some(name) = self.palette_selected_command() {
            self.textarea = fresh_textarea();
            self.palette_selected = 0;
            effects.push(Effect::Send(Action::SlashCommand { name, args: String::new() }));
        }
        effects
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

    /// Commit the highlighted entry as a path verbatim — directory OR file —
    /// without drilling in. Enter on a directory yields `dir/` (trailing slash);
    /// the caller inserts it as `@dir/ ` and closes the picker. This is what gives
    /// the user an escape from the @-picker (Tab drills in, Enter commits + exits).
    pub fn picker_commit(&self) -> Option<String> {
        let p = self.picker.as_ref()?;
        let name = p.current()?;
        let prefix = match p.query.rfind('/') {
            Some(i) => &p.query[..=i],
            None => "",
        };
        Some(format!("{prefix}{name}"))
    }

    pub fn insert_picker_path(&mut self, path: &str) {
        self.textarea.insert_str(format!("@{path} "));
        self.picker = None;
    }

    // ── # named-agent picker (IO-free; filters the static `agents` list) ──

    /// Whether typing `#` should open the agent picker now. The `#agent` route is
    /// prefix-only — core matches it only when the *whole* prompt begins with
    /// `#name` — so the picker arms only at the start of an empty prompt (a
    /// `#name` inserted mid-prompt would silently never route). It also never
    /// arms over an open overlay (e.g. an approval, whose keys it would steal),
    /// and only when named agents exist (else a bare `#` stays a literal heading).
    pub fn agent_trigger_armed(&self) -> bool {
        self.input_text().is_empty() && self.overlay.is_none() && !self.agents.is_empty()
    }

    /// Open the `#`-agent autocomplete picker (no-op without configured agents,
    /// so a bare `#` stays available for a markdown heading).
    pub fn open_agent_picker(&mut self) {
        if self.agents.is_empty() {
            return;
        }
        self.agent_picker = Some(AgentPicker::new(&self.agents));
    }

    pub fn agent_picker_move(&mut self, delta: i32) {
        if let Some(p) = &mut self.agent_picker {
            p.move_sel(delta);
        }
    }

    pub fn agent_picker_cancel(&mut self) {
        self.agent_picker = None;
    }

    pub fn agent_picker_push(&mut self, c: char) {
        if let Some(p) = &mut self.agent_picker {
            p.query.push(c);
            p.refilter();
        }
    }

    /// Backspace in the `#`-agent picker: drop the last query char, or — when the
    /// query is already empty — close the picker (Backspace past the `#` trigger
    /// exits, mirroring the file picker).
    pub fn agent_picker_backspace(&mut self) {
        let Some(p) = &mut self.agent_picker else {
            return;
        };
        if p.query.pop().is_some() {
            p.refilter();
            return;
        }
        // Empty query: Backspace past the `#` trigger closes the picker.
        self.agent_picker = None;
    }

    /// Insert the highlighted agent as `#name ` into the prompt and close the
    /// picker (an empty picker just closes).
    pub fn agent_picker_select(&mut self) {
        let name = self
            .agent_picker
            .as_ref()
            .and_then(|p| p.current())
            .map(|a| a.name.clone());
        if let Some(name) = name {
            self.textarea.insert_str(format!("#{name} "));
        }
        self.agent_picker = None;
    }

    // ── builtin overlays (context / permissions / list picker) ──

    /// Whether the current overlay is one whose keys the event loop must route
    /// to the overlay methods below (the approval overlay keeps its own y/a/n
    /// path through `lower_event`).
    pub fn overlay_captures_keys(&self) -> bool {
        matches!(
            self.overlay,
            Some(
                Overlay::Context(_)
                    | Overlay::Permissions(_)
                    | Overlay::Picker(_)
                    | Overlay::ApiKey(_)
                    | Overlay::ConnectCustom(_)
                    | Overlay::Shell(_)
                    | Overlay::Theme(_)
                    | Overlay::Settings(_)
                    | Overlay::HistorySearch(_)
                    | Overlay::Question(_)
            )
        )
    }

    /// `/settings` overlay: move the focused tab by `delta` (wrapping).
    pub fn settings_tab_move(&mut self, delta: i32) {
        if let Some(Overlay::Settings(v)) = &mut self.overlay {
            v.move_tab(delta);
        }
    }

    /// `/settings` overlay: the slash command for the focused tab's editor, if any.
    pub fn settings_jump(&self) -> Option<Action> {
        match &self.overlay {
            Some(Overlay::Settings(v)) => v.jump(),
            _ => None,
        }
    }

    /// Drop a stale Info notice ("fetching models…") when the thing it
    /// announced has arrived; warnings/errors stay until replaced.
    fn clear_info_notice(&mut self) {
        if matches!(&self.notice, Some(n) if n.level == NoticeLevel::Info) {
            self.notice = None;
        }
    }

    /// Ctrl+C: quit immediately when nothing would be lost; otherwise arm a
    /// confirmation (shell habit makes Ctrl+C easy to hit with a queue, a
    /// draft, or a running turn on screen) and quit on the second press.
    pub fn request_quit(&mut self) -> Effects {
        let effects = Effects::new();
        let at_risk = self.turn_active || !self.queue.is_empty() || !self.input_text().is_empty();
        if at_risk && !self.ctrl_c_armed {
            self.ctrl_c_armed = true;
            self.notice = Some(info_notice(QUIT_CONFIRM_NOTICE));
            return effects;
        }
        self.should_quit = true;
        effects
    }

    /// Any input other than Ctrl+C (keys, paste, mouse) breaks the
    /// quit-confirmation chord — and takes its stale prompt off the screen.
    pub fn disarm_quit(&mut self) {
        if !self.ctrl_c_armed {
            return;
        }
        self.ctrl_c_armed = false;
        if matches!(&self.notice, Some(n) if n.text == QUIT_CONFIRM_NOTICE) {
            self.notice = None;
        }
    }

    /// Clear the live tool-call lines and the id→line index together (they must
    /// always reset in lockstep, or a stale index would point past the cleared vec).
    fn clear_tool_lines(&mut self) {
        self.tool_lines.clear();
        self.tool_index.clear();
    }

    // ── background-process shell view (Down key) ──

    /// Open the shell view if any process is being tracked (else a no-op so Down
    /// stays free for the textarea). Won't clobber a live approval's oneshot.
    pub fn open_shell_view(&mut self) {
        if self.processes.is_empty() {
            return;
        }
        self.open_overlay(Overlay::Shell(ShellView { selected: 0 }));
    }

    pub fn shell_move(&mut self, delta: i32) {
        if let Some(Overlay::Shell(s)) = &mut self.overlay {
            let n = self.processes.len() as i32;
            if n > 0 {
                s.selected = (((s.selected as i32 + delta) % n + n) % n) as usize;
            }
        }
    }

    /// Kill the highlighted process: emit `KillProcess` for core to terminate it.
    pub fn shell_kill_selected(&mut self) -> Effects {
        let mut effects = Effects::new();
        if let Some(Overlay::Shell(s)) = &self.overlay
            && let Some(p) = self.processes.get(s.selected)
            && p.status == ProcStatus::Running
        {
            effects.push(Effect::Send(Action::KillProcess(p.id)));
        }
        effects
    }

    /// Type into the API-key overlay (a printable char).
    pub fn api_key_push(&mut self, c: char) {
        if let Some(Overlay::ApiKey(o)) = &mut self.overlay
            && o.input.len() < API_KEY_INPUT_MAX
        {
            o.input.push(c);
        }
    }

    /// Cancel the API-key overlay (Esc / Ctrl+C): close it and say what
    /// skipping means, so a keyless-local-server user knows nothing broke.
    pub fn api_key_cancel(&mut self) {
        let provider = match &self.overlay {
            Some(Overlay::ApiKey(o)) => Some(o.provider.clone()),
            _ => None,
        };
        self.overlay_close();
        if let Some(p) = provider {
            self.notice = Some(info_notice(format!(
                "skipped the key for '{p}' — keyless endpoints work as-is; /login {p} any time"
            )));
        }
    }

    /// Route a bracketed paste to whatever owns the keyboard. Overlay text
    /// fields take a control-char-stripped insert (a newline inside a pasted
    /// key/URL must not submit the form); the composer takes the text verbatim
    /// (newlines stay literal — never an early submit).
    pub fn paste_text(&mut self, text: &str) {
        // Terminals transmit line breaks inside a bracketed paste as CR (not
        // LF) — normalize first, or the composer would swallow them and the
        // pasted "lines" would run together with literal \r bytes.
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let text = normalized.as_str();
        let single_line: String = text.chars().filter(|c| !c.is_control()).collect();
        if matches!(self.overlay, Some(Overlay::ApiKey(_))) {
            for c in single_line.chars() {
                self.api_key_push(c);
            }
            return;
        }
        if matches!(self.overlay, Some(Overlay::ConnectCustom(_))) {
            for c in single_line.chars() {
                self.connect_custom_push(c);
            }
            return;
        }
        if matches!(self.overlay, Some(Overlay::HistorySearch(_))) {
            for c in single_line.chars() {
                self.history_search_push(c);
            }
            return;
        }
        if matches!(self.overlay, Some(Overlay::Theme(_))) {
            for c in single_line.chars() {
                self.theme_editor_edit(Some(c));
            }
            return;
        }
        if self.overlay_picker_searchable() {
            for c in single_line.chars() {
                self.overlay_picker_push(c);
            }
            return;
        }
        if self.overlay.is_none() && self.picker.is_none() && self.agent_picker.is_none() {
            self.textarea.insert_str(text);
        }
    }

    /// Backspace in the API-key overlay.
    pub fn api_key_backspace(&mut self) {
        if let Some(Overlay::ApiKey(o)) = &mut self.overlay {
            o.input.pop();
        }
    }

    /// Submit the API-key overlay: send `SetApiKey` for a non-empty key and close.
    /// An empty key just cancels (closes without sending).
    pub fn api_key_submit(&mut self) -> Effects {
        let mut effects = Effects::new();
        if let Some(Overlay::ApiKey(o)) = &self.overlay {
            let key = o.input.trim().to_string();
            if !key.is_empty() {
                effects.push(Effect::Send(Action::SetApiKey {
                    provider: o.provider.clone(),
                    key,
                }));
            }
            self.overlay_close();
        }
        effects
    }

    /// Move the custom-provider form focus (Tab/↑↓), wrapping across its rows.
    pub fn connect_custom_move(&mut self, delta: i32) {
        if let Some(Overlay::ConnectCustom(o)) = &mut self.overlay {
            let n = CUSTOM_PROVIDER_FIELDS as i32;
            o.field = (((o.field as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// Cycle the wire type (←/→ on the type row; the text rows ignore it).
    pub fn connect_custom_cycle(&mut self, delta: i32) {
        if let Some(Overlay::ConnectCustom(o)) = &mut self.overlay
            && o.field == 2
        {
            let n = CUSTOM_PROVIDER_FLAVORS.len() as i32;
            o.flavor_idx = (((o.flavor_idx as i32 + delta) % n + n) % n) as usize;
        }
    }

    /// Type into the focused text field of the custom-provider form.
    pub fn connect_custom_push(&mut self, c: char) {
        if let Some(Overlay::ConnectCustom(o)) = &mut self.overlay {
            let field = match o.field {
                0 => &mut o.name,
                1 => &mut o.host,
                _ => return,
            };
            if field.len() < CUSTOM_PROVIDER_FIELD_MAX {
                field.push(c);
            }
        }
    }

    /// Backspace in the focused text field of the custom-provider form.
    pub fn connect_custom_backspace(&mut self) {
        if let Some(Overlay::ConnectCustom(o)) = &mut self.overlay {
            match o.field {
                0 => {
                    o.name.pop();
                }
                1 => {
                    o.host.pop();
                }
                _ => {}
            }
        }
    }

    /// Submit the custom-provider form: a valid one closes and sends
    /// `Action::ConnectCustom`; an invalid one stays open with a notice naming
    /// what to fix. Core re-validates fail-closed — this is only the fast local
    /// feedback loop.
    pub fn connect_custom_submit(&mut self) -> Effects {
        let mut effects = Effects::new();
        let Some(Overlay::ConnectCustom(o)) = &self.overlay else {
            return effects;
        };
        let name = o.name.trim().to_string();
        let host = o.host.trim().to_string();
        if name.is_empty()
            || name.len() > CUSTOM_PROVIDER_NAME_MAX
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            self.notice = Some(Notice {
                level: NoticeLevel::Warn,
                text: "provider name: letters, digits, - or _ only (max 64 chars)".into(),
            });
            return effects;
        }
        // Mirror core's normalize-then-check (trailing slashes trimmed first),
        // and require a non-empty host after the scheme — "https://" alone
        // would close the form only to be bounced by core with the input lost.
        let normalized = host.trim_end_matches('/');
        let host_after_scheme = normalized
            .strip_prefix("http://")
            .or_else(|| normalized.strip_prefix("https://"));
        if !matches!(host_after_scheme, Some(rest) if !rest.is_empty()) {
            self.notice = Some(Notice {
                level: NoticeLevel::Warn,
                text: "host must start with http:// or https:// (e.g. https://localhost:11111/v1)"
                    .into(),
            });
            return effects;
        }
        let flavor = CUSTOM_PROVIDER_FLAVORS[o.flavor_idx % CUSTOM_PROVIDER_FLAVORS.len()];
        effects.push(Effect::Send(Action::ConnectCustom {
            name,
            base_url: host,
            flavor: flavor.to_string(),
        }));
        self.overlay_close();
        effects
    }

    /// Close the current non-approval overlay; a queued approval (one that
    /// arrived while it was open) surfaces immediately so it is never stranded.
    pub fn overlay_close(&mut self) {
        // A queued approval wins (its oneshot is time-sensitive); then a queued
        // question (also a live oneshot); then a queued api-key prompt — so none
        // is ever stranded behind the overlay that just closed.
        self.overlay = self.pending_approvals.pop_front().map(Overlay::Approval);
        if self.overlay.is_none()
            && let Some(req) = self.pending_questions.pop_front()
        {
            self.overlay = Some(Overlay::Question(QuestionView { req, selected: 0 }));
        }
        if self.overlay.is_none()
            && let Some(provider) = self.pending_prompts.pop_front()
        {
            self.overlay = Some(Overlay::ApiKey(ApiKeyOverlay { provider, input: String::new() }));
        }
    }

    /// Reply to the open question with the chosen option index (or `None` on
    /// cancel), then surface the next queued overlay.
    pub fn answer_question(&mut self, choice: Option<usize>) {
        if let Some(Overlay::Question(q)) = self.overlay.take() {
            let _ = q.req.reply.send(choice);
        }
        self.overlay_close();
    }

    /// Move the highlighted option in the question overlay.
    pub fn question_move(&mut self, delta: i32) {
        if let Some(Overlay::Question(q)) = &mut self.overlay {
            q.move_sel(delta);
        }
    }

    /// The currently highlighted option index in the question overlay, if open.
    pub fn question_selected(&self) -> Option<usize> {
        match &self.overlay {
            Some(Overlay::Question(q)) => Some(q.selected),
            _ => None,
        }
    }

    /// Number of options on the open question overlay (0 when none), so a
    /// number-key press past the last option can be ignored rather than cancel.
    pub fn question_option_count(&self) -> usize {
        match &self.overlay {
            Some(Overlay::Question(q)) => q.req.options.len(),
            _ => 0,
        }
    }

    pub fn overlay_picker_move(&mut self, delta: i32) {
        if let Some(Overlay::Picker(p)) = &mut self.overlay {
            p.move_sel(delta);
        }
    }

    /// Whether the open picker filters on typed input (the model list).
    pub fn overlay_picker_searchable(&self) -> bool {
        matches!(&self.overlay, Some(Overlay::Picker(p)) if p.searchable())
    }

    /// Append a char to a searchable picker's filter query.
    pub fn overlay_picker_push(&mut self, c: char) {
        if let Some(Overlay::Picker(p)) = &mut self.overlay {
            p.push_query(c);
        }
    }

    /// Drop the last char of a searchable picker's filter query.
    pub fn overlay_picker_backspace(&mut self) {
        if let Some(Overlay::Picker(p)) = &mut self.overlay {
            p.pop_query();
        }
    }

    /// Resolve the highlighted picker entry into its `Action` (Rewind/Resume),
    /// closing the picker. Empty pickers just close.
    pub fn overlay_picker_select(&mut self) -> Effects {
        let mut effects = Effects::new();
        if let Some(Overlay::Picker(p)) = &self.overlay {
            // A disabled row under the cursor (unsupported `/connect` provider) is a
            // no-op: keep the picker open so the user can move to a usable row. An
            // empty picker (no matches) still closes on Enter, as before.
            if p.is_selected_disabled() {
                return effects;
            }
            // An over-narrowed filter (query matches nothing but the list has
            // rows) keeps the picker open so Backspace can widen it — Enter
            // closing here would silently discard the whole picker.
            if p.matches.is_empty() && !p.items.is_empty() && !p.query.is_empty() {
                return effects;
            }
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
        // Don't clobber a live oneshot overlay (approval / question) with a
        // user-opened one — its awaiting tool would be stranded.
        if matches!(self.overlay, Some(Overlay::Approval(_) | Overlay::Question(_))) {
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
                self.errored_this_turn = false;
                self.notice = None;
                // The previous turn's block is retained on screen while idle
                // (the fullscreen viewport would scroll an eager commit out of
                // sight) — commit it now. `submit`/`dispatch_queued` already
                // flushed on the prompt-echo paths, so this is the safety net
                // for turn starts with no echo (slash-command turns), and it
                // leaves the live buffers clear either way.
                self.flush_block(&mut effects);
                self.workers.clear();
                // Snap back to live output for the new turn (but not on every
                // token delta — that would fight a user scrolled up to read).
                self.scroll_offset = 0;
            }
            AppEvent::AssistantTokenDelta(s) => self.live.assistant.push_str(&s),
            AppEvent::ReasoningTokenDelta(s) => self.live.reasoning.push_str(&s),
            AppEvent::ToolCallStarted(v) => {
                self.tool_index.insert(v.id, self.tool_lines.len());
                self.tool_lines.push(format!("▸ {}: {}", v.name, v.summary));
            }
            AppEvent::ToolCallOutputDelta { .. } => {}
            AppEvent::ToolCallFinished { id, ok } => {
                let mark = if ok { '✓' } else { '✗' };
                // Flip the started line's leading glyph in place (no UUID row).
                match self.tool_index.get(&id).and_then(|&i| self.tool_lines.get_mut(i)) {
                    Some(line) => {
                        if let Some(rest) = line.strip_prefix('▸') {
                            *line = format!("{mark}{rest}");
                        }
                    }
                    // No matching start (shouldn't happen) — a standalone fallback.
                    None => self.tool_lines.push(format!("{mark} tool finished")),
                }
            }
            AppEvent::DiffProposed { .. } => {}
            AppEvent::ApprovalRequested(req) => {
                if self.overlay.is_none() {
                    // The @-file and #-agent pickers live outside `self.overlay`
                    // and both draw over and capture keys away from an overlay — so
                    // an approval arriving while one is open would be invisible AND
                    // unanswerable (y/a/n types into the picker filter), hanging the
                    // turn. Drop the transient pickers so the approval surfaces.
                    self.picker = None;
                    self.agent_picker = None;
                    self.overlay = Some(Overlay::Approval(req));
                    if self.notify_on_approval {
                        effects.push(Effect::Bell);
                    }
                } else {
                    self.pending_approvals.push_back(req);
                }
            }
            AppEvent::QuestionAsked(req) => {
                if self.overlay.is_none() {
                    // Same rationale as approvals: drop the transient @/# pickers so
                    // the question is visible and answerable, not hidden behind them.
                    self.picker = None;
                    self.agent_picker = None;
                    self.overlay = Some(Overlay::Question(QuestionView { req, selected: 0 }));
                    if self.notify_on_approval {
                        effects.push(Effect::Bell);
                    }
                } else {
                    self.pending_questions.push_back(req);
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
            AppEvent::LayerFinished { index, status } => {
                let total = self.active_layer.as_ref().map(|l| l.total).unwrap_or(0);
                if let Some(layer) = self.active_layer.as_mut() {
                    layer.status = status;
                }
                // Only INTERMEDIATE layers commit eagerly (progressive
                // transcript while the pipeline runs). The last layer's block
                // IS the turn's final block — core follows with TurnComplete —
                // so it must be retained like TurnComplete retains it, or the
                // retention never engages at all (the default pipeline is a
                // single layer, making every reply "the last layer").
                if index + 1 < total {
                    self.flush_block(&mut effects);
                }
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
            AppEvent::CompactionStarted => self.notice = Some(info_notice("compacting context…")),
            AppEvent::CompactionDone { freed_tokens } => {
                self.notice = Some(info_notice(format!("compacted (-{freed_tokens} tok)")));
            }
            AppEvent::Notice { level, text } => {
                // A multi-line or over-wide notice (e.g. /help, the /connect
                // guidance) cannot fit the one-row notice line — commit the
                // full text to scrollback and keep the first line inline.
                // Display WIDTH, not chars: CJK text is two columns per char.
                if text.contains('\n') || UnicodeWidthStr::width(text.as_str()) > NOTICE_INLINE_MAX
                {
                    // The retained final block (if any) commits first so the
                    // transcript keeps chronological order; skipped mid-turn,
                    // where flushing would split the streaming block.
                    if !self.turn_active {
                        self.flush_block(&mut effects);
                    }
                    effects.push(Effect::CommitToScrollback(scrollback_notice_md(&text)));
                    let first = text.lines().next().unwrap_or_default().to_string();
                    self.notice = Some(Notice { level, text: first });
                } else {
                    self.notice = Some(Notice { level, text });
                }
            }
            AppEvent::ContextBreakdown(breakdown) => {
                self.open_overlay(Overlay::Context(breakdown));
            }
            AppEvent::PermissionsSnapshot(snapshot) => {
                self.open_overlay(Overlay::Permissions(snapshot));
            }
            AppEvent::SettingsSnapshot(snapshot) => {
                self.open_overlay(Overlay::Settings(SettingsView { snapshot, tab: 0 }));
            }
            AppEvent::CheckpointList { checkpoints, scope } => {
                self.clear_info_notice();
                let items = checkpoints
                    .into_iter()
                    .map(|c: CheckpointView| ListPickerItem {
                        label: format!("turn {}", c.turn),
                        id: c.id,
                        connectable: true,
                    })
                    .collect();
                self.open_overlay(Overlay::Picker(ListPicker::new(PickerKind::Rewind(scope), items)));
            }
            AppEvent::SessionList(sessions) => {
                self.clear_info_notice();
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
                        connectable: true,
                    })
                    .collect();
                self.open_overlay(Overlay::Picker(ListPicker::new(PickerKind::Resume, items)));
            }
            AppEvent::ModelList(models) => {
                // The list may arrive UNPROMPTED (auto-opened seconds after a
                // key save) — it may replace a previous picker or fill an empty
                // slot, but never clobber a TUI-local overlay (Ctrl+R search,
                // the theme editor, a form) the user opened meanwhile.
                if !(self.overlay.is_none() || matches!(self.overlay, Some(Overlay::Picker(_)))) {
                    return effects;
                }
                self.clear_info_notice();
                let current_ref = format!("{}/{}", self.model.provider, self.model.model);
                let items: Vec<ListPickerItem> = models
                    .into_iter()
                    .map(|m: ModelChoiceView| ListPickerItem {
                        label: if m.model_ref == current_ref {
                            format!("{}  ·  current", m.label)
                        } else {
                            m.label
                        },
                        id: m.model_ref,
                        connectable: m.selectable,
                    })
                    .collect();
                let mut picker = ListPicker::new(PickerKind::Model, items);
                // Open on the model in use (like the effort picker) so "which
                // one am I on?" needs no scanning.
                if let Some(idx) = picker.items.iter().position(|i| i.id == current_ref) {
                    picker.selected = idx;
                }
                self.open_overlay(Overlay::Picker(picker));
            }
            AppEvent::ProviderList(providers) => {
                self.clear_info_notice();
                let items = providers
                    .into_iter()
                    .map(|p: ProviderChoiceView| ListPickerItem {
                        label: p.label,
                        id: p.id,
                        connectable: p.connectable,
                    })
                    .collect();
                self.open_overlay(Overlay::Picker(ListPicker::new(PickerKind::Connect, items)));
            }
            AppEvent::CustomProviderPrompt { name, base_url, flavor } => {
                // Like an API-key prompt: drop the transient @/# pickers so the
                // form is visible and receives the keys, not hidden behind them.
                self.picker = None;
                self.agent_picker = None;
                // A non-empty prefill is a bounced submission (core's pre-save
                // probe failed) — focus the host field, the usual culprit.
                let flavor_idx =
                    CUSTOM_PROVIDER_FLAVORS.iter().position(|f| *f == flavor).unwrap_or(0);
                let field = if base_url.is_empty() { 0 } else { 1 };
                self.open_overlay(Overlay::ConnectCustom(ConnectCustomOverlay {
                    name,
                    host: base_url,
                    flavor_idx,
                    field,
                }));
            }
            AppEvent::AuthMethodPrompt { provider } => {
                self.clear_info_notice();
                let items = vec![
                    ListPickerItem {
                        id: format!("{provider} --auth api-key"),
                        label: "api key  ·  paste a platform API key".to_string(),
                        connectable: true,
                    },
                    ListPickerItem {
                        id: format!("{provider} --auth chatgpt"),
                        label: "ChatGPT OAuth  ·  use your ChatGPT subscription (browser sign-in)"
                            .to_string(),
                        connectable: true,
                    },
                    ListPickerItem {
                        id: format!("{provider} --auth access-token"),
                        label: "access token  ·  paste a bearer/OAuth token (gateways, proxies)"
                            .to_string(),
                        connectable: true,
                    },
                ];
                self.open_overlay(Overlay::Picker(ListPicker::new(PickerKind::Auth, items)));
            }
            AppEvent::OpenThemeEditor => self.open_theme_editor(),
            AppEvent::OpenEffortPicker { current } => {
                const LEVELS: [&str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];
                let items = LEVELS
                    .iter()
                    .map(|&lvl| ListPickerItem {
                        label: lvl.to_string(),
                        id: lvl.to_string(),
                        connectable: true,
                    })
                    .collect();
                let mut picker = ListPicker::new(PickerKind::Effort, items);
                picker.selected = LEVELS.iter().position(|&l| l == current).unwrap_or(0);
                self.open_overlay(Overlay::Picker(picker));
            }
            // `/editor [text]` — the event loop opens $EDITOR (it owns the
            // terminal); the slash argument is the seed.
            AppEvent::OpenEditor { seed } => self.editor_request = Some(seed),
            AppEvent::CopyToClipboard(text) => {
                self.notice = Some(info_notice("copied the last reply to the clipboard"));
                effects.push(Effect::CopyToClipboard(text));
            }
            AppEvent::EffortChanged(level) => self.effort = level,
            AppEvent::ApiKeyPrompt { provider } => {
                // Never clobber a live overlay (esp. an approval's oneshot) and
                // never drop the prompt — queue it if something is on screen.
                if self.overlay.is_none() {
                    // Drop the transient pickers first: like an approval, they draw
                    // over and capture keys away from an overlay, so a key prompt
                    // racing an open @/# picker would be invisible AND unanswerable.
                    self.picker = None;
                    self.agent_picker = None;
                    self.overlay =
                        Some(Overlay::ApiKey(ApiKeyOverlay { provider, input: String::new() }));
                } else {
                    self.pending_prompts.push_back(provider);
                }
            }
            AppEvent::SessionResumed { id, name, turns } => {
                // The displayed block belongs to the previous session's view —
                // preserve it in scrollback before resetting (it clears the
                // live buffers as a side effect; a no-op when nothing is held).
                self.flush_block(&mut effects);
                self.todos.clear();
                self.workers.clear();
                self.usage = UsageView::default();
                self.notice = Some(info_notice(format!(
                    "resumed session {} ({turns} turn(s))",
                    name.unwrap_or(id)
                )));
            }
            AppEvent::TurnComplete { .. } => {
                self.turn_active = false;
                self.workers.clear();
                // Deliberately NOT flushed here: the viewport spans the whole
                // terminal, so an eager commit would scroll the final block
                // straight out of sight and leave an empty screen. It stays in
                // the live box while idle and commits on the next dispatch /
                // turn start / exit (`take_final_commit`).
                // An errored turn already rang (or deliberately stayed silent); the
                // trailing TurnComplete must not add a "success" beep on top.
                if self.notify_on_complete && !self.errored_this_turn {
                    effects.push(Effect::Bell);
                }
                self.errored_this_turn = false;
                self.dispatch_queued(&mut effects);
            }
            AppEvent::Cleared => {
                // Fresh session: drop all live + queued state and purge the
                // terminal scrollback so the previous conversation is gone.
                self.live.clear();
                self.clear_tool_lines();
                self.todos.clear();
                self.workers.clear();
                self.queue.clear();
                self.usage = UsageView::default();
                self.turn_active = false;
                self.notice = Some(info_notice("cleared — new session"));
                effects.push(Effect::ClearScreen);
            }
            AppEvent::ProcessStarted { id, command } => {
                self.processes.push(ProcView {
                    id,
                    command,
                    status: ProcStatus::Running,
                    output: VecDeque::new(),
                });
                self.notice = Some(info_notice("background process started — press ↓ for the shell view"));
            }
            AppEvent::ProcessOutput { id, line } => {
                if let Some(p) = self.processes.iter_mut().find(|p| p.id == id) {
                    p.output.push_back(line);
                    while p.output.len() > PROC_OUTPUT_TAIL {
                        p.output.pop_front();
                    }
                }
            }
            AppEvent::ProcessExited { id, code } => {
                if let Some(p) = self.processes.iter_mut().find(|p| p.id == id) {
                    p.status = ProcStatus::Exited(code);
                }
            }
            AppEvent::Error(text) => {
                self.notice = Some(Notice { level: NoticeLevel::Error, text: format!("error: {text}") });
                // Mark the turn errored so the trailing TurnComplete won't also
                // beep (opencode likewise suppresses the done-notification here).
                self.errored_this_turn = true;
                if self.notify_on_error {
                    effects.push(Effect::Bell);
                }
            }
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
            // While a turn is active, Esc-Esc must stay an interrupt: arming is
            // gated on `!turn_active` so a user mashing Esc to stop the agent
            // never trips rewind mid-turn.
            Action::Interrupt => {
                if self.input_text().is_empty() && was_armed && !self.turn_active {
                    effects.push(Effect::Send(Action::SlashCommand {
                        name: "rewind".into(),
                        args: String::new(),
                    }));
                } else {
                    self.esc_armed = self.input_text().is_empty() && !self.turn_active;
                    effects.push(Effect::Send(Action::Interrupt));
                }
            }
            Action::Approve { request_id, decision } => {
                if let Some(Overlay::Approval(req)) = self.overlay.take() {
                    if req.id == request_id {
                        let _ = req.reply.send(decision);
                        // Surface the next queued overlay (approval → question →
                        // api-key prompt). Must go through `overlay_close` so a
                        // question/prompt queued behind this approval isn't
                        // stranded (its tool would hang on its oneshot).
                        self.overlay_close();
                    } else {
                        self.overlay = Some(Overlay::Approval(req));
                    }
                }
            }
            Action::Quit => self.should_quit = true,
            other @ (Action::SlashCommand { .. }
            | Action::Rewind { .. }
            | Action::Resume { .. }
            | Action::SetApiKey { .. }
            | Action::ConnectCustom { .. }
            | Action::SetTheme { .. }
            | Action::KillProcess(_)
            | Action::AttachImage { .. }) => {
                effects.push(Effect::Send(other));
            }
            // TUI-local scrollback: adjust the offset and repaint; never sent to
            // core. The upper bound is clamped in render_live, where the view
            // height is known (saturating_sub keeps the raw value recoverable).
            Action::ScrollUp(n) => self.scroll_offset = self.scroll_offset.saturating_add(n),
            Action::ScrollDown(n) => self.scroll_offset = self.scroll_offset.saturating_sub(n),
            // Ctrl+E: seed the editor with the current buffer; the event loop opens
            // it (it owns the terminal). TUI-local, never forwarded to core.
            Action::OpenEditor => self.editor_request = Some(self.input_text()),
            Action::Redraw => {}
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
        // Record the prompt as the user typed it (a shell line keeps its `!`), so
        // ↑/Ctrl+R recall reproduces the original input.
        let entry = match &item {
            Queued::Chat(t) => t.clone(),
            Queued::Shell(c) => format!("!{c}"),
        };
        if let Some(eff) = self.record_history(entry) {
            effects.push(eff);
        }
        self.textarea = fresh_textarea();
        // Pasted images were forwarded to core as they were pasted; the next turn
        // consumes them, so clear the staged indicator now.
        self.pending_image_count = 0;
        if self.turn_active {
            self.queue.push_back(item);
        } else {
            self.turn_active = true;
            // The previous turn's block (retained on screen while idle) commits
            // first, then the prompt echo, so the transcript keeps its
            // chat-style order: reply N, prompt N+1, reply N+1.
            self.flush_block(effects);
            effects.push(Effect::CommitToScrollback(item.echo_md()));
            effects.push(Effect::Send(item.into_action()));
        }
    }

    fn dispatch_queued(&mut self, effects: &mut Effects) {
        if let Some(next) = self.queue.pop_front() {
            self.turn_active = true;
            // Commit the just-finished turn's retained block, then echo the
            // queued prompt at dispatch time (not enqueue) so both land in
            // chronological order, right above the reply the prompt produces.
            self.flush_block(effects);
            effects.push(Effect::CommitToScrollback(next.echo_md()));
            effects.push(Effect::Send(next.into_action()));
        }
    }

    /// Flush the block still displayed in the live box (the final block of a
    /// turn is retained on screen while idle — see `TurnComplete`) so every
    /// exit path ends with the full transcript in scrollback. Called by the
    /// event loop once, right before the terminal is restored.
    pub fn take_final_commit(&mut self) -> Effects {
        let mut effects = Effects::new();
        self.flush_block(&mut effects);
        effects
    }

    /// Commit the current tool lines + assistant markdown as one scrollback block
    /// and clear the live buffer. Called when a layer finishes mid-turn, and for
    /// a turn's final block lazily — on the next dispatch / turn start / exit —
    /// so the finished reply stays visible in the full-screen viewport while idle.
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
        // Preserve the thinking as a dimmed blockquote ahead of the answer.
        if !self.live.reasoning.is_empty() {
            for line in self.live.reasoning.lines() {
                md.push_str("> ");
                md.push_str(line);
                md.push('\n');
            }
            md.push('\n');
        }
        md.push_str(&self.live.assistant);
        effects.push(Effect::CommitToScrollback(md));
        self.live.clear();
        self.clear_tool_lines();
    }
}

impl Queued {
    fn into_action(self) -> Action {
        match self {
            Queued::Chat(t) => Action::SubmitInput(t),
            Queued::Shell(c) => Action::RunShell(c),
        }
    }

    /// The chat-style transcript line committed to scrollback when this item is
    /// sent, so the user's own prompt is visible above the assistant's reply. A
    /// leading `❯` prompt glyph marks it as input — no redundant role label like
    /// "you", and distinct from both the label-less assistant reply and the
    /// model's reasoning (which renders as a blockquote). The glyph also keeps the
    /// text from being reinterpreted as a markdown heading/list.
    fn echo_md(&self) -> String {
        match self {
            Queued::Chat(t) => format!("❯ {t}"),
            Queued::Shell(c) => format!("❯ `!{c}`"),
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
            commands: vec![
                CommandInfo::named("review"),
                CommandInfo::named("rewind"),
                CommandInfo::named("resume"),
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

    fn state_with_agents() -> AppState {
        let mut s = test_state();
        s.agents = vec![
            AgentInfo { name: "reviewer".into(), description: "code review".into() },
            AgentInfo { name: "researcher".into(), description: "research".into() },
            AgentInfo { name: "refactorer".into(), description: "refactor".into() },
        ];
        s
    }

    #[test]
    fn ctrl_e_requests_editor_seeded_with_the_current_buffer() {
        let mut s = test_state();
        s.textarea.insert_str("draft prompt");
        // Ctrl+E lowers to Action::OpenEditor (TUI-local: no Effect::Send).
        let effects = s.apply_action(Action::OpenEditor);
        assert!(effects.is_empty(), "OpenEditor is TUI-local, nothing forwarded to core");
        assert_eq!(s.take_editor_request().as_deref(), Some("draft prompt"));
        // Drained — a second take yields nothing.
        assert!(s.take_editor_request().is_none());
    }

    #[test]
    fn slash_editor_requests_editor_seeded_with_its_argument() {
        let mut s = test_state();
        // `/editor foo bar` arrives from core as OpenEditor { seed: "foo bar" }.
        s.apply_event(AppEvent::OpenEditor { seed: "foo bar".into() });
        assert_eq!(s.take_editor_request().as_deref(), Some("foo bar"));
    }

    #[test]
    fn set_input_replaces_the_buffer_with_the_editor_result() {
        let mut s = test_state();
        s.textarea.insert_str("old");
        s.set_input("edited\nmulti-line");
        assert_eq!(s.input_text(), "edited\nmulti-line");
        // An empty result clears the box.
        s.set_input("");
        assert_eq!(s.input_text(), "");
    }

    #[test]
    fn history_records_submitted_prompts_dedups_and_keeps_shell_prefix() {
        let mut s = test_state();
        s.apply_action(Action::SubmitInput("first".into()));
        // A consecutive duplicate is not stored twice.
        s.apply_action(Action::SubmitInput("first".into()));
        s.apply_action(Action::SubmitInput("second".into()));
        // A shell line is recorded with its `!` so recall reproduces the input.
        s.apply_action(Action::RunShell("ls -la".into()));
        assert_eq!(s.history, vec!["first", "second", "!ls -la"]);
        // Blank input records nothing.
        s.apply_action(Action::SubmitInput("   ".into()));
        assert_eq!(s.history.len(), 3);
    }

    #[test]
    fn history_up_down_recalls_entries_and_restores_the_draft() {
        let mut s = test_state();
        s.set_history(vec!["one".into(), "two".into()]);
        s.textarea.insert_str("draft");
        // ↑ walks older; stays at the oldest.
        assert!(s.history_prev());
        assert_eq!(s.input_text(), "two");
        s.history_prev();
        assert_eq!(s.input_text(), "one");
        s.history_prev();
        assert_eq!(s.input_text(), "one", "stays at the oldest entry");
        // ↓ walks newer; past the newest restores the stashed draft.
        s.history_next();
        assert_eq!(s.input_text(), "two");
        s.history_next();
        assert_eq!(s.input_text(), "draft", "the in-progress draft is restored");
        // ↓ with nothing being browsed is a no-op (not consumed).
        assert!(!s.history_next());
    }

    #[test]
    fn history_prev_on_empty_history_is_not_consumed() {
        let mut s = test_state();
        assert!(!s.history_prev(), "no history → ↑ falls through to the textarea");
    }

    #[test]
    fn history_prev_parks_cursor_on_first_row_so_multiline_entries_can_be_walked() {
        let mut s = test_state();
        s.set_history(vec!["older".into(), "a\nb\nc".into()]);
        // Recall the newest (multi-line) entry; the caret must land on row 0 so the
        // next ↑ (gated on row 0 in app.rs) keeps walking older.
        s.history_prev();
        assert_eq!(s.input_text(), "a\nb\nc");
        assert_eq!(s.textarea.cursor().0, 0, "caret parked on the first row after recall");
        // A second ↑ walks to the older entry rather than getting stuck mid-entry.
        s.history_prev();
        assert_eq!(s.input_text(), "older");
    }

    #[test]
    fn ctrl_r_search_filters_and_accepts_into_the_input() {
        let mut s = test_state();
        s.set_history(vec!["build".into(), "test all".into(), "deploy".into()]);
        s.open_history_search();
        assert!(matches!(s.overlay, Some(Overlay::HistorySearch(_))));
        for c in "all".chars() {
            s.history_search_push(c);
        }
        let rows = s.history_search_rows();
        assert_eq!(rows.len(), 1, "only 'test all' contains 'all'");
        assert_eq!(rows[0].0, "test all");
        s.history_search_accept();
        assert_eq!(s.input_text(), "test all");
        assert!(s.overlay.is_none(), "accepting closes the overlay");
    }

    #[test]
    fn question_overlay_opens_navigates_and_replies_with_the_choice() {
        let mut s = test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<usize>>();
        s.apply_event(AppEvent::QuestionAsked(stepper_protocol::QuestionRequest {
            id: uuid::Uuid::new_v4(),
            question: "Which?".into(),
            options: vec!["a".into(), "b".into(), "c".into()],
            reply: tx,
        }));
        assert!(matches!(s.overlay, Some(Overlay::Question(_))), "question overlay opened");
        s.question_move(1); // highlight option 2 (index 1)
        assert_eq!(s.question_selected(), Some(1));
        s.answer_question(s.question_selected());
        assert!(s.overlay.is_none(), "answering closes the overlay");
        assert_eq!(rx.blocking_recv().unwrap(), Some(1), "the chosen index is sent back");
    }

    #[test]
    fn question_option_count_reports_the_open_question_size() {
        let mut s = test_state();
        assert_eq!(s.question_option_count(), 0, "no question open");
        let (tx, _rx) = tokio::sync::oneshot::channel::<Option<usize>>();
        s.apply_event(AppEvent::QuestionAsked(stepper_protocol::QuestionRequest {
            id: uuid::Uuid::new_v4(),
            question: "Which?".into(),
            options: vec!["a".into(), "b".into()],
            reply: tx,
        }));
        // The app.rs digit handler uses this to ignore out-of-range numbers.
        assert_eq!(s.question_option_count(), 2);
    }

    #[test]
    fn question_esc_replies_none_without_a_choice() {
        let mut s = test_state();
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<usize>>();
        s.apply_event(AppEvent::QuestionAsked(stepper_protocol::QuestionRequest {
            id: uuid::Uuid::new_v4(),
            question: "Which?".into(),
            options: vec!["a".into(), "b".into()],
            reply: tx,
        }));
        s.answer_question(None);
        assert_eq!(rx.blocking_recv().unwrap(), None, "cancel sends None");
    }

    #[test]
    fn status_line_context_carries_model_mode_cwd_and_usage() {
        let mut s = test_state();
        s.usage.context_used = 1234;
        s.usage.context_limit = 200_000;
        s.usage.cost_usd = 0.5;
        let ctx = s.status_line_context();
        assert_eq!(ctx["model"]["provider"], "p");
        assert_eq!(ctx["model"]["model"], "m");
        assert_eq!(ctx["mode"], "auto");
        assert_eq!(ctx["cwd"], "/tmp");
        assert_eq!(ctx["tokens"]["used"], 1234);
        assert_eq!(ctx["tokens"]["limit"], 200_000);
        assert_eq!(ctx["cost_usd"], 0.5);
    }

    #[test]
    fn submit_emits_a_persist_effect_carrying_the_path_and_lines() {
        let mut s = test_state();
        s.history_path = Some(PathBuf::from("/tmp/hist.json"));
        let effects = s.apply_action(Action::SubmitInput("remember me".into()));
        let persisted = effects.iter().find_map(|e| match e {
            Effect::PersistHistory { path, lines } => Some((path.clone(), lines.clone())),
            _ => None,
        });
        let (path, lines) = persisted.expect("a PersistHistory effect when a path is set");
        assert_eq!(path, PathBuf::from("/tmp/hist.json"));
        assert_eq!(lines, vec!["remember me"]);
    }

    #[test]
    fn agent_picker_opens_filters_and_inserts_hash_name() {
        let mut s = state_with_agents();
        s.open_agent_picker();
        // An empty query matches every configured agent.
        assert_eq!(s.agent_picker.as_ref().unwrap().matches.len(), 3);

        // Typing narrows by a substring of the name: "res" → only "researcher".
        for c in "res".chars() {
            s.agent_picker_push(c);
        }
        assert_eq!(s.agent_picker.as_ref().unwrap().matches.len(), 1);

        // Enter inserts `#researcher ` (the `#agent` trigger core routes) + closes.
        s.agent_picker_select();
        assert!(s.agent_picker.is_none());
        assert_eq!(s.input_text(), "#researcher ");
    }

    #[test]
    fn agent_picker_is_a_noop_without_configured_agents() {
        let mut s = test_state(); // no named agents
        s.open_agent_picker();
        assert!(s.agent_picker.is_none(), "a bare # stays literal without agents");
    }

    #[test]
    fn agent_picker_backspace_past_the_query_exits() {
        let mut s = state_with_agents();
        s.open_agent_picker();
        s.agent_picker_push('x');
        s.agent_picker_backspace(); // removes the 'x', the picker stays open
        assert!(s.agent_picker.is_some(), "still open with an empty query");
        s.agent_picker_backspace(); // empty query → Backspace closes it
        assert!(s.agent_picker.is_none(), "Backspace past the trigger exits the picker");
    }

    #[test]
    fn agent_trigger_arms_only_at_the_start_of_an_empty_prompt() {
        let mut s = state_with_agents();
        assert!(s.agent_trigger_armed(), "empty prompt + agents + no overlay arms");
        // Mid-prompt: the #agent route is prefix-only, so a #name inserted here
        // would never route — the picker must not arm.
        s.textarea.insert_str("fix the bug ");
        assert!(!s.agent_trigger_armed(), "a non-empty prompt disarms the trigger");
        // No configured agents → a bare # stays a literal heading.
        assert!(!test_state().agent_trigger_armed(), "no agents → no picker");
    }

    #[test]
    fn agent_trigger_is_disarmed_over_an_open_overlay() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = state_with_agents();
        let (reply, _rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Command { cmd: "ls".into(), outside_project: false },
            reply,
        }));
        assert!(!s.agent_trigger_armed(), "# must not steal keys from a live overlay");
    }

    #[test]
    fn api_key_prompt_closes_an_open_agent_picker_so_it_is_answerable() {
        let mut s = state_with_agents();
        s.open_agent_picker();
        assert!(s.agent_picker.is_some());
        s.apply_event(AppEvent::ApiKeyPrompt { provider: "anthropic".into() });
        assert!(s.agent_picker.is_none(), "the picker is dropped");
        assert!(matches!(s.overlay, Some(Overlay::ApiKey(_))), "the key prompt surfaces");
    }

    #[test]
    fn command_palette_matches_by_prefix_only_for_a_bare_slash_token() {
        let mut s = test_state();
        assert!(!s.palette_active(), "no `/` typed yet");

        s.textarea.insert_str("/re");
        let names: Vec<&str> = s.command_matches().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["review", "rewind", "resume"]);
        assert!(s.palette_active());

        s.textarea = TextArea::default();
        s.textarea.insert_str("/rev");
        let names: Vec<&str> = s.command_matches().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["review"]);

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
    fn palette_enter_runs_the_highlighted_command_and_clears_input() {
        let mut s = test_state();
        s.textarea.insert_str("/re");
        s.palette_move(1); // highlight "rewind" (index 1 of review/rewind/resume)
        let effects = s.palette_run_selected();
        match effects.as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "rewind");
                assert!(args.is_empty());
            }
            _ => panic!("Enter should run the highlighted command"),
        }
        assert!(s.input_text().is_empty(), "the typed /partial is cleared");
        assert_eq!(s.palette_selected, 0, "selection resets after running");
    }

    #[test]
    fn palette_run_selected_is_a_noop_without_matches() {
        let mut s = test_state();
        s.textarea.insert_str("/nope");
        assert!(s.palette_run_selected().is_empty(), "no matches → nothing runs");
    }

    #[test]
    fn turn_complete_retains_the_block_until_the_next_submit_commits_it_first() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("hello ".into()));
        s.apply_event(AppEvent::AssistantTokenDelta("world".into()));
        assert_eq!(s.live.assistant, "hello world");
        assert!(s.turn_active);

        // TurnComplete retains the final block on screen (the full-height
        // viewport would scroll an eager commit out of sight) — no commit yet.
        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::CommitToScrollback(_))),
            "the final block must stay displayed while idle"
        );
        assert_eq!(s.live.assistant, "hello world");
        assert!(!s.turn_active);

        // The next submit flushes the retained block FIRST, then echoes the new
        // prompt, so scrollback keeps chat order: reply N, prompt N+1.
        let effects = s.apply_action(Action::SubmitInput("next".into()));
        let commits: Vec<&str> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::CommitToScrollback(md) => Some(md.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(commits.len(), 2);
        assert!(commits[0].contains("hello world"));
        assert!(commits[1].contains("next"));
        assert!(s.live.assistant.is_empty());
    }

    #[test]
    fn turn_started_flushes_a_retained_block_when_no_prompt_echo_ran() {
        // A slash-command turn (e.g. /code-review) starts with no submit(), so
        // the retained block must commit at TurnStarted, not be lost.
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("first reply".into()));
        s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert_eq!(s.live.assistant, "first reply");

        let effects = s.apply_event(AppEvent::TurnStarted { turn_id: 2 });
        match effects.as_slice() {
            [Effect::CommitToScrollback(md)] => assert!(md.contains("first reply")),
            other => panic!("expected exactly the retained-block commit, got {} effects", other.len()),
        }
        assert!(s.live.assistant.is_empty());
    }

    #[test]
    fn take_final_commit_flushes_the_retained_block_for_exit() {
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::AssistantTokenDelta("last words".into()));
        s.apply_event(AppEvent::TurnComplete { turn_id: 1 });

        let effects = s.take_final_commit();
        match effects.as_slice() {
            [Effect::CommitToScrollback(md)] => assert!(md.contains("last words")),
            _ => panic!("exit must commit the retained block"),
        }
        // Nothing retained → nothing to commit (no duplicate on exit).
        assert!(s.take_final_commit().is_empty());
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
    fn the_last_layer_is_retained_not_flushed_so_single_layer_turns_keep_the_reply() {
        // Core emits LayerFinished for EVERY layer (including the last) and then
        // TurnComplete. The default pipeline is a single layer, so an eager
        // flush here would defeat the retention on every ordinary chat turn.
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::LayerStarted { index: 0, total: 1, name: "default".into() });
        s.apply_event(AppEvent::AssistantTokenDelta("the reply".into()));
        let effects = s.apply_event(AppEvent::LayerFinished { index: 0, status: LayerStatus::Done });
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::CommitToScrollback(_))),
            "the sole (last) layer must be retained"
        );
        let effects = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(!effects.iter().any(|e| matches!(e, Effect::CommitToScrollback(_))));
        assert_eq!(s.live.assistant, "the reply", "still on screen while idle");

        // Two-layer pipeline: layer 0 commits eagerly, the final layer is retained.
        let mut s = test_state();
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        s.apply_event(AppEvent::LayerStarted { index: 0, total: 2, name: "plan".into() });
        s.apply_event(AppEvent::AssistantTokenDelta("plan out".into()));
        let mid = s.apply_event(AppEvent::LayerFinished { index: 0, status: LayerStatus::Done });
        assert!(mid.iter().any(|e| matches!(e, Effect::CommitToScrollback(_))));
        s.apply_event(AppEvent::LayerStarted { index: 1, total: 2, name: "implement".into() });
        s.apply_event(AppEvent::AssistantTokenDelta("impl out".into()));
        let last = s.apply_event(AppEvent::LayerFinished { index: 1, status: LayerStatus::Done });
        assert!(!last.iter().any(|e| matches!(e, Effect::CommitToScrollback(_))));
        assert_eq!(s.live.assistant, "impl out");
    }

    #[test]
    fn submit_when_idle_echoes_prompt_then_sends_immediately() {
        let mut s = test_state();
        let effects = s.apply_action(Action::SubmitInput("hi".into()));
        // The prompt is echoed to scrollback first, then sent — chat-style trace.
        match effects.as_slice() {
            [Effect::CommitToScrollback(md), Effect::Send(Action::SubmitInput(t))] => {
                // A `❯ ` prompt glyph marks input — no "you" role label.
                assert_eq!(md, "❯ hi", "echo is the prompt glyph + text, no 'you' label");
                assert!(!md.to_lowercase().contains("you"), "no role label");
                assert_eq!(t.as_str(), "hi");
            }
            _ => panic!("expected an echo commit then an immediate send"),
        }
        assert!(s.turn_active);
    }

    #[test]
    fn shell_submit_echoes_the_bang_command() {
        let mut s = test_state();
        let effects = s.apply_action(Action::RunShell("ls -la".into()));
        match effects.as_slice() {
            [Effect::CommitToScrollback(md), Effect::Send(Action::RunShell(c))] => {
                assert!(md.contains("!ls -la"), "shell echo shows the bang command: {md}");
                assert_eq!(c.as_str(), "ls -la");
            }
            _ => panic!("expected an echo commit then a shell send"),
        }
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
    fn fresh_input_has_no_cursor_line_underline() {
        // ratatui-textarea 0.9.1 defaults the cursor line to UNDERLINED, which
        // drew a line under the whole prompt row. fresh_textarea must clear it.
        assert_eq!(fresh_textarea().cursor_line_style(), Style::default());
    }

    #[test]
    fn esc_esc_on_idle_opens_rewind() {
        let mut s = test_state();
        s.esc_armed = true; // a prior Esc armed it; input is empty; no turn running
        let effects = s.apply_action(Action::Interrupt);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Send(Action::SlashCommand { name, .. })] if name == "rewind"
        ));
    }

    #[test]
    fn esc_esc_during_active_turn_interrupts_not_rewind() {
        let mut s = test_state();
        s.turn_active = true;
        s.esc_armed = true; // even if armed, an in-flight turn must interrupt
        let effects = s.apply_action(Action::Interrupt);
        assert!(matches!(effects.as_slice(), [Effect::Send(Action::Interrupt)]));
        // and it must not re-arm mid-turn, so a third Esc also interrupts.
        assert!(!s.esc_armed);
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
    fn scroll_actions_move_offset_locally_and_redraw_is_a_noop() {
        let mut s = test_state();
        // Scroll actions are TUI-local: they move the offset, emit no effects.
        assert!(s.apply_action(Action::ScrollUp(3)).is_empty());
        assert_eq!(s.scroll_offset, 3);
        assert!(s.apply_action(Action::ScrollDown(1)).is_empty());
        assert_eq!(s.scroll_offset, 2);
        // ScrollDown saturates at the bottom (0 = pinned to live).
        assert!(s.apply_action(Action::ScrollDown(5)).is_empty());
        assert_eq!(s.scroll_offset, 0);
        assert!(s.apply_action(Action::Redraw).is_empty());
        assert_eq!(s.scroll_offset, 0, "Redraw leaves the offset untouched");
    }

    #[test]
    fn a_new_turn_snaps_scroll_back_to_live() {
        let mut s = test_state();
        s.apply_action(Action::ScrollUp(4));
        assert_eq!(s.scroll_offset, 4);
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        assert_eq!(s.scroll_offset, 0, "a new turn jumps back to the latest output");
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
    fn question_queued_behind_an_approval_surfaces_when_the_approval_is_answered() {
        use stepper_protocol::{ApprovalDecision, ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let mut s = test_state();
        let id = Uuid::new_v4();
        let (reply, _rx) = oneshot::channel();
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id,
            kind: ApprovalKind::Mcp { server: "a".into(), tool: "x".into() },
            reply,
        }));
        // A question arrives while the approval is on screen → it queues.
        let (qtx, qrx) = oneshot::channel::<Option<usize>>();
        s.apply_event(AppEvent::QuestionAsked(stepper_protocol::QuestionRequest {
            id: Uuid::new_v4(),
            question: "Which?".into(),
            options: vec!["a".into(), "b".into()],
            reply: qtx,
        }));
        assert_eq!(s.pending_questions.len(), 1);
        // Answering the approval (via the real Action path) must surface the queued
        // question, not strand it (which would hang the asking tool's oneshot).
        s.apply_action(Action::Approve { request_id: id, decision: ApprovalDecision::AllowOnce });
        assert!(matches!(&s.overlay, Some(Overlay::Question(_))), "question now shown");
        assert!(s.pending_questions.is_empty());
        // And the question's oneshot is still live (not dropped).
        s.answer_question(Some(0));
        assert_eq!(qrx.blocking_recv().unwrap(), Some(0));
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
        s.apply_event(AppEvent::TurnComplete { turn_id: 7 });
        // The block is retained at TurnComplete; the exit flush carries it.
        let effects = s.take_final_commit();
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
    fn notification_bell_fires_per_enabled_trigger_only() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        let has_bell = |fx: &Effects| fx.iter().any(|e| matches!(e, Effect::Bell));

        // Turn-complete rings only when enabled (default state is all-false).
        let mut s = test_state();
        s.notify_on_complete = true;
        assert!(has_bell(&s.apply_event(AppEvent::TurnComplete { turn_id: 1 })));
        let mut silent = test_state();
        assert!(!has_bell(&silent.apply_event(AppEvent::TurnComplete { turn_id: 1 })));

        // Error rings only when enabled.
        let mut s = test_state();
        s.notify_on_error = true;
        assert!(has_bell(&s.apply_event(AppEvent::Error("boom".into()))));
        assert!(!has_bell(&silent.apply_event(AppEvent::Error("boom".into()))));

        // A failed turn rings AT MOST once: core emits Error then TurnComplete, so
        // an enabled complete-bell must be suppressed after an error (no double-beep).
        let mut s = test_state();
        s.notify_on_complete = true;
        s.notify_on_error = true;
        s.apply_event(AppEvent::TurnStarted { turn_id: 1 });
        let err = s.apply_event(AppEvent::Error("boom".into()));
        let done = s.apply_event(AppEvent::TurnComplete { turn_id: 1 });
        assert!(has_bell(&err), "the error rings");
        assert!(!has_bell(&done), "the trailing TurnComplete does not also ring");
        // A clean turn (no error) still rings the complete-bell.
        s.apply_event(AppEvent::TurnStarted { turn_id: 2 });
        assert!(has_bell(&s.apply_event(AppEvent::TurnComplete { turn_id: 2 })));

        // An approval that surfaces immediately rings; a second one queued behind
        // it (the user is clearly present, mid-interaction) stays silent.
        let mut s = test_state();
        s.notify_on_approval = true;
        let (reply1, _rx1) = oneshot::channel();
        let surfaced = s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Mcp { server: "a".into(), tool: "x".into() },
            reply: reply1,
        }));
        assert!(has_bell(&surfaced), "an approval that surfaces rings");
        let (reply2, _rx2) = oneshot::channel();
        let queued = s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Mcp { server: "b".into(), tool: "y".into() },
            reply: reply2,
        }));
        assert!(!has_bell(&queued), "a queued approval stays silent");
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
    fn approval_closes_an_open_agent_picker_so_it_is_answerable() {
        use stepper_protocol::{ApprovalKind, ApprovalRequest};
        use tokio::sync::oneshot;
        use uuid::Uuid;
        // An approval can race an open #-agent picker (a queued prompt typed while
        // a prior turn runs). The picker draws over and captures keys, so it must
        // be dropped or the approval is invisible AND unanswerable (turn hang).
        let mut s = state_with_agents();
        s.open_agent_picker();
        assert!(s.agent_picker.is_some());
        let (reply, _rx) = oneshot::channel();
        s.apply_event(AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::Command { cmd: "ls".into(), outside_project: false },
            reply,
        }));
        assert!(s.agent_picker.is_none(), "the picker is dropped");
        assert!(matches!(s.overlay, Some(Overlay::Approval(_))), "the approval surfaces");
    }

    #[test]
    fn notice_and_error_events_set_notice_text() {
        let mut s = test_state();
        s.apply_event(AppEvent::Notice {
            level: NoticeLevel::Warn,
            text: "saved".into(),
        });
        let n = s.notice.as_ref().unwrap();
        assert_eq!((n.level, n.text.as_str()), (NoticeLevel::Warn, "saved"), "level is preserved");
        s.apply_event(AppEvent::Error("boom".into()));
        let n = s.notice.as_ref().unwrap();
        assert_eq!((n.level, n.text.as_str()), (NoticeLevel::Error, "error: boom"));
    }

    #[test]
    fn tool_call_finished_flips_glyph_in_place_without_a_uuid_line() {
        let mut s = test_state();
        let started = |id: &str, name: &str| {
            AppEvent::ToolCallStarted(stepper_protocol::ToolCallView {
                id: id.into(),
                name: name.into(),
                summary: "x".into(),
            })
        };
        s.apply_event(started("1", "read_file"));
        s.apply_event(started("2", "bash"));
        s.apply_event(AppEvent::ToolCallFinished { id: "1".into(), ok: true });
        s.apply_event(AppEvent::ToolCallFinished { id: "2".into(), ok: false });
        // Two lines only — the finish flips each started line's glyph in place,
        // it does not append a separate `✓ tool <uuid> finished` row.
        assert_eq!(s.tool_lines.len(), 2, "no extra UUID lines: {:?}", s.tool_lines);
        assert!(s.tool_lines[0].starts_with('✓') && s.tool_lines[0].contains("read_file"));
        assert!(s.tool_lines[1].starts_with('✗') && s.tool_lines[1].contains("bash"));
        // No UUID leaked into the rendered lines.
        assert!(!s.tool_lines.iter().any(|l| l.contains("finished")), "no UUID/finished text");
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
    fn picker_commit_returns_directory_path_so_enter_can_escape() {
        let mut s = test_state();
        s.set_picker(
            "/home/user/".to_string(),
            vec!["src/".into(), "notes.md".into()],
            "",
        );
        // Enter on a directory commits the dir path verbatim (trailing slash kept)
        // instead of drilling in — that is the escape from the infinite drill.
        match s.picker_commit() {
            Some(path) => assert_eq!(path, "/home/user/src/"),
            None => panic!("expected the directory path to commit"),
        }
        s.insert_picker_path("/home/user/src/");
        assert_eq!(s.input_text(), "@/home/user/src/ ");
        assert!(s.picker.is_none(), "committing closes the picker");
    }

    #[test]
    fn picker_commit_also_returns_file_paths() {
        let mut s = test_state();
        s.set_picker("/a/".to_string(), vec!["b.txt".into()], "");
        assert_eq!(s.picker_commit().as_deref(), Some("/a/b.txt"));
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
        assert_eq!(s.notice.as_ref().unwrap().text, "compacting context…");
    }

    #[test]
    fn compaction_done_sets_freed_tokens_notice() {
        let mut s = test_state();
        s.apply_event(AppEvent::CompactionStarted);
        let effects = s.apply_event(AppEvent::CompactionDone { freed_tokens: 4096 });
        assert!(effects.is_empty());
        assert_eq!(s.notice.as_ref().unwrap().text, "compacted (-4096 tok)");
    }

    fn checkpoint_list() -> AppEvent {
        AppEvent::CheckpointList {
            checkpoints: vec![
                CheckpointView { id: "turn-3".into(), turn: 3 },
                CheckpointView { id: "turn-2".into(), turn: 2 },
                CheckpointView { id: "turn-1".into(), turn: 1 },
            ],
            scope: RewindScope::Both,
        }
    }

    #[test]
    fn checkpoint_list_opens_rewind_picker_and_select_sends_rewind() {
        let mut s = test_state();
        assert!(s.apply_event(checkpoint_list()).is_empty());
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.kind, PickerKind::Rewind(RewindScope::Both));
                assert_eq!(p.items.len(), 3);
                assert_eq!(p.items[0].label, "turn 3");
                assert_eq!(p.selected, 0);
            }
            _ => panic!("expected the rewind picker overlay"),
        }

        s.overlay_picker_move(1);
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            [Effect::Send(Action::Rewind { checkpoint_id, .. })] => {
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
    fn model_list_opens_picker_and_select_routes_through_model_switch() {
        let mut s = test_state();
        s.apply_event(AppEvent::ModelList(vec![
            ModelChoiceView {
                model_ref: "anthropic/claude-opus-4-8".into(),
                label: "anthropic/claude-opus-4-8  ·  1M  ·  $5.00/$25.00".into(),
                selectable: true,
            },
            ModelChoiceView {
                model_ref: "openai/gpt-5".into(),
                label: "openai/gpt-5".into(),
                selectable: true,
            },
        ]));
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.kind, PickerKind::Model);
                assert_eq!(p.items.len(), 2);
                assert!(p.items[0].label.contains("claude-opus-4-8"));
            }
            _ => panic!("expected the models picker overlay"),
        }
        s.overlay_picker_move(1);
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            // Selection reuses the /model switch path, carrying the chosen ref.
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "model");
                assert_eq!(args, "openai/gpt-5");
            }
            _ => panic!("expected a /model SlashCommand send"),
        }
        assert!(s.overlay.is_none(), "selection closes the picker");
    }

    #[test]
    fn effort_picker_opens_with_current_and_selects_level() {
        let mut s = test_state();
        s.apply_event(AppEvent::OpenEffortPicker { current: "high".into() });
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.kind, PickerKind::Effort);
                assert_eq!(p.items.len(), 6);
                // [off, low, medium, high, xhigh, max] → "high" is index 3.
                assert_eq!(p.selected, 3, "the current level starts highlighted");
            }
            _ => panic!("expected the effort picker overlay"),
        }
        // Move to "xhigh" (index 4) and select → routes back through /effort.
        s.overlay_picker_move(1);
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "effort");
                assert_eq!(args, "xhigh");
            }
            _ => panic!("expected an /effort SlashCommand send"),
        }
        assert!(s.overlay.is_none(), "selection closes the picker");
    }

    #[test]
    fn settings_snapshot_opens_tabbed_overlay_and_jumps() {
        use stepper_protocol::{SettingsRowView, SettingsTabView};
        let mut s = test_state();
        s.apply_event(AppEvent::SettingsSnapshot(SettingsSnapshotView {
            tabs: vec![
                SettingsTabView {
                    title: "General".into(),
                    rows: vec![SettingsRowView { label: "mode".into(), value: "auto".into() }],
                    jump: None,
                },
                SettingsTabView { title: "Permissions".into(), rows: vec![], jump: Some("permissions".into()) },
            ],
        }));
        match &s.overlay {
            Some(Overlay::Settings(v)) => {
                assert_eq!(v.tab, 0, "starts on the first tab");
                assert_eq!(v.snapshot.tabs.len(), 2);
            }
            _ => panic!("expected the settings overlay"),
        }
        // The General tab has no editor, so Enter would just close (no jump).
        assert!(s.settings_jump().is_none());
        // Switch to Permissions → Enter routes through /permissions.
        s.settings_tab_move(1);
        match s.settings_jump() {
            Some(Action::SlashCommand { name, .. }) => assert_eq!(name, "permissions"),
            other => panic!("expected a /permissions jump, got {other:?}"),
        }
    }

    #[test]
    fn model_picker_filters_on_typed_query_and_selects_the_match() {
        let mut s = test_state();
        s.apply_event(AppEvent::ModelList(vec![
            ModelChoiceView {
                model_ref: "anthropic/claude-opus-4-8".into(),
                label: "anthropic/claude-opus-4-8".into(),
                selectable: true,
            },
            ModelChoiceView { model_ref: "openai/gpt-5".into(), label: "openai/gpt-5".into(), selectable: true },
            ModelChoiceView { model_ref: "openai/gpt-5-mini".into(), label: "openai/gpt-5-mini".into(), selectable: true },
        ]));
        assert!(s.overlay_picker_searchable(), "the model picker is searchable");
        // Type "gpt" → only the two gpt rows remain.
        for c in "gpt".chars() {
            s.overlay_picker_push(c);
        }
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.query, "gpt");
                assert_eq!(p.matches.len(), 2, "filtered to gpt-* rows");
                assert_eq!(p.items.len(), 3, "the full candidate list is retained");
            }
            _ => panic!("picker expected"),
        }
        // Selecting the first match sends the right ref through the /model path.
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "model");
                assert_eq!(args, "openai/gpt-5");
            }
            _ => panic!("expected a /model send for the filtered selection"),
        }
    }

    #[test]
    fn provider_picker_filters_on_query_and_selects_via_connect() {
        let mut s = test_state();
        s.apply_event(AppEvent::ProviderList(vec![
            ProviderChoiceView { id: "anthropic".into(), label: "anthropic  ·  Anthropic".into(), connectable: true },
            ProviderChoiceView { id: "openai".into(), label: "openai  ·  OpenAI".into(), connectable: true },
            ProviderChoiceView { id: "openrouter".into(), label: "openrouter  ·  OpenRouter".into(), connectable: true },
        ]));
        assert!(s.overlay_picker_searchable(), "the connect picker is searchable");
        // Type "openr" → only openrouter remains.
        for c in "openr".chars() {
            s.overlay_picker_push(c);
        }
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.kind, PickerKind::Connect);
                assert_eq!(p.matches.len(), 1, "filtered to openrouter");
                assert_eq!(p.items.len(), 3, "the full provider seed is retained");
            }
            _ => panic!("connect picker expected"),
        }
        // Selecting routes back through `/connect <id>` (register + key prompt).
        let effects = s.overlay_picker_select();
        match effects.as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "connect");
                assert_eq!(args, "openrouter");
            }
            _ => panic!("expected a /connect send for the filtered selection"),
        }
    }

    #[test]
    fn unconnectable_provider_row_is_not_selectable() {
        let mut s = test_state();
        s.apply_event(AppEvent::ProviderList(vec![
            ProviderChoiceView {
                id: "google-vertex-anthropic".into(),
                label: "google-vertex-anthropic  ·  (unsupported — set baseUrl manually)".into(),
                connectable: false,
            },
            ProviderChoiceView { id: "anthropic".into(), label: "anthropic  ·  Anthropic".into(), connectable: true },
        ]));
        // The disabled row sits at index 0 (selected by default) → Enter is a no-op.
        let effects = s.overlay_picker_select();
        assert!(effects.is_empty(), "selecting an unconnectable row emits nothing");
        assert!(matches!(s.overlay, Some(Overlay::Picker(_))), "the picker stays open");
        // The connectable row still fires `/connect`.
        s.overlay_picker_move(1);
        match s.overlay_picker_select().as_slice() {
            [Effect::Send(Action::SlashCommand { name, args })] => {
                assert_eq!(name, "connect");
                assert_eq!(args, "anthropic");
            }
            _ => panic!("expected /connect for the connectable row"),
        }
    }

    #[test]
    fn custom_provider_prompt_opens_the_form_and_submit_sends_connect_custom() {
        let mut s = test_state();
        s.apply_event(AppEvent::CustomProviderPrompt {
            name: String::new(),
            base_url: String::new(),
            flavor: String::new(),
        });
        assert!(matches!(s.overlay, Some(Overlay::ConnectCustom(_))), "form opens");
        assert!(s.overlay_captures_keys(), "the form captures keys");

        for c in "my-local".chars() {
            s.connect_custom_push(c);
        }
        s.connect_custom_move(1);
        for c in "https://localhost:11111/v1".chars() {
            s.connect_custom_push(c);
        }
        // Move to the type row and cycle openai → claude.
        s.connect_custom_move(1);
        s.connect_custom_cycle(1);

        let effects = s.connect_custom_submit();
        match effects.as_slice() {
            [Effect::Send(Action::ConnectCustom { name, base_url, flavor })] => {
                assert_eq!(name, "my-local");
                assert_eq!(base_url, "https://localhost:11111/v1");
                assert_eq!(flavor, "claude");
            }
            _ => panic!("expected a ConnectCustom send"),
        }
        assert!(s.overlay.is_none(), "a valid submit closes the form");
    }

    #[test]
    fn custom_provider_form_rejects_bad_input_and_stays_open() {
        let mut s = test_state();
        s.apply_event(AppEvent::CustomProviderPrompt {
            name: String::new(),
            base_url: String::new(),
            flavor: String::new(),
        });
        // Empty name → warn, stays open, nothing sent.
        assert!(s.connect_custom_submit().is_empty());
        assert!(matches!(s.overlay, Some(Overlay::ConnectCustom(_))));
        assert!(matches!(&s.notice, Some(n) if n.level == NoticeLevel::Warn));

        // Valid name but a schemeless host → still rejected.
        for c in "local".chars() {
            s.connect_custom_push(c);
        }
        s.connect_custom_move(1);
        for c in "localhost:11111".chars() {
            s.connect_custom_push(c);
        }
        assert!(s.connect_custom_submit().is_empty());
        assert!(matches!(&s.notice, Some(n) if n.text.contains("http")));
        assert!(matches!(s.overlay, Some(Overlay::ConnectCustom(_))));

        // The type row wraps in both directions and ignores typed chars.
        s.connect_custom_move(1);
        s.connect_custom_cycle(-1);
        s.connect_custom_push('x');
        match &s.overlay {
            Some(Overlay::ConnectCustom(o)) => {
                assert_eq!(CUSTOM_PROVIDER_FLAVORS[o.flavor_idx], "custom");
                assert_eq!(o.host, "localhost:11111", "typing on the type row is ignored");
            }
            _ => panic!("form expected"),
        }
    }

    #[test]
    fn long_or_multiline_notices_commit_to_scrollback_with_the_first_line_inline() {
        let mut s = test_state();
        let effects = s.apply_event(AppEvent::Notice {
            level: NoticeLevel::Info,
            text: "line one\nline two".into(),
        });
        assert!(
            matches!(effects.as_slice(), [Effect::CommitToScrollback(md)] if md.contains("line two")),
            "the full text reaches scrollback"
        );
        assert_eq!(s.notice.as_ref().unwrap().text, "line one");
        // Short single-line notices stay inline only (no scrollback noise).
        let effects = s.apply_event(AppEvent::Notice { level: NoticeLevel::Warn, text: "short".into() });
        assert!(effects.is_empty());
        assert_eq!(s.notice.as_ref().unwrap().text, "short");
    }

    #[test]
    fn ctrl_c_confirms_before_quitting_when_something_would_be_lost() {
        // Idle + empty: quit at once (no nagging).
        let mut s = test_state();
        s.request_quit();
        assert!(s.should_quit);

        // A draft arms a confirmation; the second consecutive press quits.
        let mut s = test_state();
        s.textarea.insert_str("draft");
        assert!(s.request_quit().is_empty());
        assert!(!s.should_quit, "the first Ctrl+C only arms");
        assert!(s.notice.as_ref().unwrap().text.contains("again"));
        // Any other key disarms — the next Ctrl+C re-arms instead of quitting.
        s.disarm_quit();
        s.request_quit();
        assert!(!s.should_quit);
        s.request_quit();
        assert!(s.should_quit);
    }

    #[test]
    fn paste_strips_newlines_in_form_fields_but_keeps_them_in_the_composer() {
        let mut s = test_state();
        s.apply_event(AppEvent::ApiKeyPrompt { provider: "p".into() });
        s.paste_text("sk-abc\ndef");
        match &s.overlay {
            Some(Overlay::ApiKey(o)) => assert_eq!(o.input, "sk-abcdef", "newline never submits"),
            _ => panic!("api-key overlay expected"),
        }
        s.overlay_close();
        // The composer takes the text verbatim — a newline is a literal newline.
        s.paste_text("line1\nline2");
        assert_eq!(s.input_text(), "line1\nline2");

        // The custom-provider form's focused field gets the sanitized insert.
        s.set_input("");
        s.apply_event(AppEvent::CustomProviderPrompt {
            name: String::new(),
            base_url: String::new(),
            flavor: String::new(),
        });
        s.connect_custom_move(1);
        s.paste_text("https://localhost:11111/v1\n");
        match &s.overlay {
            Some(Overlay::ConnectCustom(o)) => assert_eq!(o.host, "https://localhost:11111/v1"),
            _ => panic!("form expected"),
        }
    }

    #[test]
    fn model_picker_marks_and_preselects_the_current_model_and_clears_the_fetch_notice() {
        let mut s = test_state();
        s.set_notice("fetching models…");
        s.apply_event(AppEvent::ModelList(vec![
            ModelChoiceView { model_ref: "a/x".into(), label: "a/x".into(), selectable: true },
            ModelChoiceView { model_ref: "p/m".into(), label: "p/m".into(), selectable: true },
        ]));
        assert!(s.notice.is_none(), "the stale fetching notice is cleared");
        match &s.overlay {
            Some(Overlay::Picker(p)) => {
                assert_eq!(p.selected, 1, "opens on the model in use");
                assert!(p.items[1].label.contains("current"));
                assert!(!p.items[0].label.contains("current"));
            }
            _ => panic!("model picker expected"),
        }
    }

    #[test]
    fn resume_picker_filters_and_an_over_narrow_filter_does_not_close_on_enter() {
        let mut s = test_state();
        let session = |id: &str, digest: &str| SessionView {
            id: id.into(),
            name: None,
            digest: digest.into(),
            turns: 1,
            age: "1m".into(),
        };
        s.apply_event(AppEvent::SessionList(vec![
            session("s1", "fix the parser"),
            session("s2", "write docs"),
        ]));
        assert!(s.overlay_picker_searchable(), "the resume picker is type-to-filter");
        for c in "docs".chars() {
            s.overlay_picker_push(c);
        }
        match &s.overlay {
            Some(Overlay::Picker(p)) => assert_eq!(p.matches.len(), 1),
            _ => panic!("picker expected"),
        }
        // Over-narrow the filter: Enter must keep the picker open (Backspace
        // can widen it), not silently discard it.
        for c in "zzz".chars() {
            s.overlay_picker_push(c);
        }
        assert!(s.overlay_picker_select().is_empty());
        assert!(matches!(s.overlay, Some(Overlay::Picker(_))), "picker stays open");
    }

    #[test]
    fn api_key_esc_explains_the_keyless_path() {
        let mut s = test_state();
        s.apply_event(AppEvent::ApiKeyPrompt { provider: "omlx".into() });
        s.api_key_cancel();
        assert!(s.overlay.is_none());
        assert!(s.notice.as_ref().unwrap().text.contains("/login omlx"));
    }

    #[test]
    fn theme_editor_cycles_preset_edits_color_and_saves_set_theme() {
        let mut s = test_state();
        s.open_theme_editor();
        assert!(matches!(s.overlay, Some(Overlay::Theme(_))), "editor opens");

        // Cycle the preset (row 0) to `light` and apply it live.
        s.theme_editor_cycle(1);
        assert_eq!(s.theme.accent, crate::theme::Theme::preset("light").unwrap().accent);

        // Select the first color row (accent) and type a fresh value (replaces).
        s.theme_editor_move(1);
        for c in "#ff0000".chars() {
            s.theme_editor_edit(Some(c));
        }
        assert_eq!(s.theme.accent, ratatui::style::Color::Rgb(0xff, 0, 0), "live preview");

        // Save → applies + returns the persist action; overlay closes.
        let action = s.theme_editor_save().expect("save yields a SetTheme");
        match action {
            Action::SetTheme { preset, colors } => {
                assert_eq!(preset.as_deref(), Some("light"));
                // ONLY the changed role is persisted (minimal overrides), not all 11.
                assert_eq!(colors, vec![("accent".to_string(), "#ff0000".to_string())]);
            }
            _ => panic!("expected SetTheme"),
        }
        assert!(s.overlay.is_none(), "save closes the editor");
        assert_eq!(s.theme_preset, "light");
    }

    #[test]
    fn theme_editor_cancel_reverts_the_live_preview() {
        let mut s = test_state();
        let original = s.theme.accent;
        s.open_theme_editor();
        s.theme_editor_cycle(1); // changes the live theme
        assert_ne!(s.theme.accent, original, "preview changed");
        s.theme_editor_cancel();
        assert_eq!(s.theme.accent, original, "cancel restores the original theme");
        assert!(s.overlay.is_none());
    }

    #[test]
    fn api_key_prompt_opens_overlay_and_submit_sends_set_api_key() {
        let mut s = test_state();
        s.apply_event(AppEvent::ApiKeyPrompt { provider: "anthropic".into() });
        match &s.overlay {
            Some(Overlay::ApiKey(o)) => {
                assert_eq!(o.provider, "anthropic");
                assert!(o.input.is_empty());
            }
            _ => panic!("expected the api-key overlay"),
        }
        s.api_key_push('s');
        s.api_key_push('k');
        s.api_key_push('x');
        s.api_key_backspace();
        let effects = s.api_key_submit();
        match effects.as_slice() {
            [Effect::Send(Action::SetApiKey { provider, key })] => {
                assert_eq!(provider, "anthropic");
                assert_eq!(key, "sk");
            }
            _ => panic!("expected a SetApiKey send"),
        }
        assert!(s.overlay.is_none(), "submit closes the overlay");
    }

    #[test]
    fn api_key_empty_submit_cancels_without_sending() {
        let mut s = test_state();
        s.apply_event(AppEvent::ApiKeyPrompt { provider: "openai".into() });
        let effects = s.api_key_submit();
        assert!(effects.is_empty(), "an empty key sends nothing");
        assert!(s.overlay.is_none(), "and still closes");
    }

    #[test]
    fn api_key_prompt_queues_behind_an_approval_and_surfaces_on_close() {
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
        // Arrives while the approval is on screen — must queue, not clobber or drop.
        s.apply_event(AppEvent::ApiKeyPrompt { provider: "anthropic".into() });
        assert!(
            matches!(s.overlay, Some(Overlay::Approval(_))),
            "the approval (and its oneshot) stays on screen"
        );
        assert_eq!(s.pending_prompts.len(), 1, "the prompt is queued, not lost");
        // Closing the approval surfaces the queued key prompt.
        s.overlay_close();
        match &s.overlay {
            Some(Overlay::ApiKey(o)) => assert_eq!(o.provider, "anthropic"),
            _ => panic!("the queued api-key prompt must surface after the approval closes"),
        }
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
    fn cleared_resets_live_state_and_purges_the_screen() {
        let mut s = test_state();
        s.apply_event(AppEvent::AssistantTokenDelta("stale answer".into()));
        s.turn_active = true;
        s.queue.push_back(Queued::Chat("queued".into()));
        let effects = s.apply_event(AppEvent::Cleared);
        assert!(s.live.assistant.is_empty(), "live buffer cleared");
        assert!(s.queue.is_empty(), "queue cleared");
        assert!(!s.turn_active);
        assert!(
            effects.iter().any(|e| matches!(e, Effect::ClearScreen)),
            "Cleared purges the terminal scrollback"
        );
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
        assert_eq!(s.notice.as_ref().unwrap().text, "resumed session earlier (3 turn(s))");
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
