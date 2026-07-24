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
        /// What to restore (files, conversation, or both).
        scope: RewindScope,
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
    /// Register a user-defined provider (submitted from the `/connect` custom
    /// form). `flavor` is the UI wire-type choice (`openai` | `claude` |
    /// `custom`); core maps it to a provider kind, injects the provider into the
    /// live config, persists it to `setting.json`, and (for the API flavors)
    /// prompts for a key. Plain strings only — the protocol stays UI-agnostic.
    ConnectCustom {
        name: String,
        base_url: String,
        flavor: String,
    },
    /// Persist the TUI color theme (chosen in the `/theme` editor). `preset` is a
    /// built-in palette name; `colors` are per-role `(name, color-string)`
    /// overrides. Core writes them to `setting.json`; the TUI applies live. Plain
    /// strings only — the protocol stays UI-framework-agnostic.
    SetTheme {
        preset: Option<String>,
        colors: Vec<(String, String)>,
    },
    /// Kill a running background process (`!cmd &`) by its id, from the shell view.
    KillProcess(u64),
    /// Stage a pasted image (Ctrl+V) for the next prompt. `media_type` is a MIME
    /// type ("image/png"); `data` is base64. Core buffers these and attaches them
    /// to the next `SubmitInput` user message.
    AttachImage {
        media_type: String,
        data: String,
    },
    /// Ctrl+E — open the external editor (`$VISUAL`/`$EDITOR`) seeded with the
    /// current input box. TUI-local: handled entirely in the event loop (terminal
    /// handoff), never forwarded to core. The `/editor` slash takes the core path
    /// (`AppEvent::OpenEditor`) so its argument survives as the seed.
    OpenEditor,
    Quit,
    Redraw,
}

/// What a `/rewind` restores. Threaded from `/rewind [code|conversation]` through
/// the checkpoint picker to the eventual [`Action::Rewind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RewindScope {
    /// Restore both the file tree and the conversation (the default).
    #[default]
    Both,
    /// Restore the file tree only; keep the conversation transcript.
    CodeOnly,
    /// Truncate the conversation only; keep the working tree as-is.
    ConversationOnly,
}
