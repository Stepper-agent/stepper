use crate::error::ProviderError;
use crate::event::ChatEvent;
use crate::request::ChatRequest;
use crate::response::ChatResponse;
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

/// The result of `chat_stream`: a boxed async stream of normalized events.
pub type ChatStream = BoxStream<'static, Result<ChatEvent, ProviderError>>;

/// A model endpoint. Adapters implement only `chat_stream` (the streaming path);
/// `chat` is a default that folds the stream into a `ChatResponse`, so there is
/// one source of truth for normalization.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// The provider config name (e.g. `ollama-cloud`, `anthropic`). Drives the
    /// status-line model segment via `ModelChanged`.
    fn provider(&self) -> &str;

    /// The model id sent on the wire (e.g. `qwen3-coder:480b`).
    fn model(&self) -> &str;

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError>;

    /// Drain the stream into a folded response. A stream that ends without a
    /// terminal `Done` event (a clean upstream close mid-turn — the most common
    /// transient failure) is an error, never a fake successful `EndTurn`.
    async fn chat(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, ProviderError> {
        let mut stream = self.chat_stream(request, cancel).await?;
        let mut events = Vec::new();
        let mut saw_terminal_done = false;
        while let Some(item) = stream.next().await {
            let event = item?;
            if matches!(event, ChatEvent::Done(_)) {
                saw_terminal_done = true;
            }
            events.push(event);
        }
        if !saw_terminal_done {
            return Err(ProviderError::UnexpectedEnd);
        }
        Ok(ChatResponse::from_events(events))
    }
}
