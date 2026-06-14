//! `stepper-providers` — concrete SSE adapters over the `stepper-provider`
//! contract (OpenAI-compat, Anthropic, OpenAI Responses) plus auth resolution
//! and the Codex ChatGPT-OAuth flow. This is one of the only crates allowed to
//! pull `reqwest`.

pub mod anthropic;
pub mod auth;
pub mod codex;
mod error;
pub mod factory;
pub mod models;
pub mod openai_compat;
pub mod responses;
pub mod sse;
pub mod wire;

pub use anthropic::AnthropicAdapter;
pub use auth::{
    delete_key_from_keyring, key_from_keyring, resolve_key, store_key_in_keyring, AuthSource,
};
pub use codex::{CodexTokenStore, CODEX_BASE_URL};
pub use factory::{ProviderFactory, ProviderKind, ProviderSpec};
pub use models::{list_models, Catalog, CatalogMeta, ModelEntry};
pub use openai_compat::OpenAiCompatAdapter;
pub use responses::OpenAiResponsesAdapter;

// Re-export the contract so downstream crates can depend on just this crate.
pub use stepper_provider::{
    ChatEvent, ChatRequest, ChatResponse, ChatStream, ContentBlock, LlmProvider, Message,
    ProviderError, Role, StopReason, ThinkingConfig, ToolChoice, ToolSpec, Usage,
};
