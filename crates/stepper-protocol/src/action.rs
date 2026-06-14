use crate::approval::ApprovalDecision;
use crate::mode::Mode;
use uuid::Uuid;

/// User intent, already lowered from a raw terminal event and resolved against
/// the current `Mode`. Sent from the TUI to core over an mpsc channel.
///
/// Some variants (`Scroll*`, `CycleMode`, `Redraw`) are handled entirely inside
/// the TUI and never reach core.
#[derive(Debug, Clone)]
pub enum Action {
    SubmitInput(String),
    /// `!`-prefixed direct shell command (Claude-Code-style). Real execution is
    /// gated by the permission engine + sandbox in core.
    RunShell(String),
    InsertNewline,
    Interrupt,
    CycleMode,
    SetMode(Mode),
    ScrollUp(u16),
    ScrollDown(u16),
    Approve {
        request_id: Uuid,
        decision: ApprovalDecision,
    },
    SlashCommand {
        name: String,
        args: String,
    },
    /// Remove the most recently queued (not-yet-sent) message. TUI-local.
    RemoveLastQueued,
    Rewind {
        checkpoint_id: String,
    },
    Resume {
        session_id: String,
    },
    /// Store an API key for `provider` (entered in the TUI key overlay). Core
    /// writes it to the OS keyring; the next provider resolve picks it up.
    SetApiKey {
        provider: String,
        key: String,
    },
    Quit,
    Redraw,
}
