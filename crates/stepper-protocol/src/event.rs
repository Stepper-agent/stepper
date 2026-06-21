use crate::approval::ApprovalRequest;
use crate::view::{
    CheckpointView, ContextBreakdownView, DiffView, LayerStatus, ModelChoiceView, ModelView,
    NoticeLevel, PermissionsSnapshotView, ProviderChoiceView, SessionView, SettingsSnapshotView,
    TodoItemView, ToolCallView, UsageView,
};

/// Everything core tells the TUI. The single item type of the core->TUI mpsc
/// channel that the TUI's `tokio::select!` loop renders.
///
/// `LayerStarted` / `LayerFinished` / `UsageUpdated` / `ModelChanged` drive the
/// active-layer indicator and the tokens/context%/cost footer (§4.6.1).
///
/// Not `Clone`/`Serialize`: `ApprovalRequested` carries a live oneshot.
#[derive(Debug)]
pub enum AppEvent {
    TurnStarted {
        turn_id: u64,
    },
    AssistantTokenDelta(String),
    ReasoningTokenDelta(String),
    ToolCallStarted(ToolCallView),
    ToolCallOutputDelta {
        id: String,
        chunk: String,
    },
    ToolCallFinished {
        id: String,
        ok: bool,
    },
    DiffProposed {
        id: String,
        diff: DiffView,
    },
    ApprovalRequested(ApprovalRequest),
    TodoUpdated(Vec<TodoItemView>),
    LayerStarted {
        index: usize,
        total: usize,
        name: String,
    },
    LayerFinished {
        index: usize,
        status: LayerStatus,
    },
    /// A fan-out worker (parallel layer or `dispatch` sub-agent) began. Drives a
    /// new row in the live worker panel. `total` is the worker count of this
    /// batch; the parent layer is whichever is currently active.
    WorkerStarted {
        index: usize,
        total: usize,
        label: String,
        model: ModelView,
    },
    /// Per-worker progress: `tokens` (cumulative) and/or the worker's latest tool
    /// call. `None` fields leave the worker row's existing value untouched. Sent
    /// in place of the global token/tool/usage events while inside a worker.
    WorkerActivity {
        index: usize,
        tokens: Option<u64>,
        tool: Option<String>,
    },
    WorkerFinished {
        index: usize,
        status: LayerStatus,
    },
    UsageUpdated(UsageView),
    ModelChanged(ModelView),
    CompactionStarted,
    CompactionDone {
        freed_tokens: u64,
    },
    Notice {
        level: NoticeLevel,
        text: String,
    },
    /// `/context` — the estimated category decomposition; the TUI shows it as a
    /// dismissable panel.
    ContextBreakdown(ContextBreakdownView),
    /// `/permissions` — the read-only rules/approvals snapshot overlay.
    PermissionsSnapshot(PermissionsSnapshotView),
    /// `/settings` — the consolidated tabbed settings overview. Enter on a tab
    /// with a jump opens that setting's editor (e.g. `/permissions`, `/theme`).
    SettingsSnapshot(SettingsSnapshotView),
    /// `/rewind` (or Esc-Esc) — available checkpoints, newest first; the TUI
    /// opens a picker whose selection comes back as `Action::Rewind`.
    CheckpointList(Vec<CheckpointView>),
    /// `/resume` — recent sessions, newest first; the TUI opens a picker whose
    /// selection comes back as `Action::Resume`.
    SessionList(Vec<SessionView>),
    /// `/models` — selectable models (live list merged with the catalog); the TUI
    /// opens a picker whose selection comes back as `Action::SlashCommand`
    /// `/model <ref>` (reusing the existing switch path).
    ModelList(Vec<ModelChoiceView>),
    /// `/connect` (no arg) — the models.dev provider seed; the TUI opens a
    /// searchable picker whose selection comes back as `Action::SlashCommand`
    /// `/connect <id>` (registers the provider, then prompts for its key).
    ProviderList(Vec<ProviderChoiceView>),
    /// `/theme` (no preset arg) — ask the TUI to open its color-theme editor. The
    /// TUI fills the editor from its own current theme (colors live TUI-side); a
    /// save comes back as `Action::SetTheme`.
    OpenThemeEditor,
    /// `/effort` (no arg) — ask the TUI to open its reasoning-effort picker.
    /// `current` is the active level (`off|low|medium|high|xhigh|max`) so the TUI
    /// highlights it; the choice comes back as `Action::SlashCommand { effort }`.
    OpenEffortPicker {
        current: String,
    },
    /// `/editor [text]` — ask the TUI to compose the next prompt in `$VISUAL`/
    /// `$EDITOR`. `seed` is the text after `/editor` (empty for a bare `/editor`),
    /// written to the temp file as the starting buffer. The edited result replaces
    /// the TUI input box. Spawning the editor (terminal handoff) is TUI-only.
    OpenEditor {
        seed: String,
    },
    /// The session reasoning-effort level changed (`/effort`); `None` = off. The
    /// TUI shows it in the status footer.
    EffortChanged(Option<String>),
    /// Ask the TUI to open the API-key entry overlay for `provider` (from
    /// `/login`, or when a model switch failed for lack of a key). The entered
    /// key comes back as `Action::SetApiKey`.
    ApiKeyPrompt {
        provider: String,
    },
    /// An in-session `Action::Resume` succeeded: the TUI clears its live state
    /// and shows the resumed session.
    SessionResumed {
        id: String,
        name: Option<String>,
        turns: u64,
    },
    /// `/clear` started a fresh session: the TUI resets its live state AND purges
    /// the terminal scrollback so the previous conversation disappears.
    Cleared,
    TurnComplete {
        turn_id: u64,
    },
    /// A `!cmd &` background process was spawned — opens a row in the shell view
    /// (reachable with the Down key). `id` is the stable process handle.
    ProcessStarted {
        id: u64,
        command: String,
    },
    /// A line of stdout/stderr from a background process, for its console pane.
    ProcessOutput {
        id: u64,
        line: String,
    },
    /// A background process ended (with its exit code, if any).
    ProcessExited {
        id: u64,
        code: Option<i32>,
    },
    Error(String),
}
