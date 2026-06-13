use crate::approval::ApprovalRequest;
use crate::view::{
    CheckpointView, ContextBreakdownView, DiffView, LayerStatus, ModelView, NoticeLevel,
    PermissionsSnapshotView, SessionView, TodoItemView, ToolCallView, UsageView,
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
    /// `/rewind` (or Esc-Esc) — available checkpoints, newest first; the TUI
    /// opens a picker whose selection comes back as `Action::Rewind`.
    CheckpointList(Vec<CheckpointView>),
    /// `/resume` — recent sessions, newest first; the TUI opens a picker whose
    /// selection comes back as `Action::Resume`.
    SessionList(Vec<SessionView>),
    /// An in-session `Action::Resume` succeeded: the TUI clears its live state
    /// and shows the resumed session.
    SessionResumed {
        id: String,
        name: Option<String>,
        turns: u64,
    },
    TurnComplete {
        turn_id: u64,
    },
    Error(String),
}
