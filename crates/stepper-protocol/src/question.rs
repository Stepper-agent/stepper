use tokio::sync::oneshot;
use uuid::Uuid;

/// A model-initiated multiple-choice question (the `ask_user_question` tool). Like
/// [`crate::ApprovalRequest`], the embedded `reply` oneshot IS the suspension
/// mechanism: the tool awaits it while the TUI shows a picker, and the chosen
/// option index (or `None` on cancel / no UI) comes back through it.
///
/// Not `Clone`/`Serialize` on purpose — it carries a live channel end.
#[derive(Debug)]
pub struct QuestionRequest {
    pub id: Uuid,
    pub question: String,
    pub options: Vec<String>,
    pub reply: oneshot::Sender<Option<usize>>,
}
