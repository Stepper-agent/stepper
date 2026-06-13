//! `stepper-provider` — the normalized, dialect-agnostic LLM provider contract.
//!
//! This crate defines the `LlmProvider` trait plus the neutral request/response
//! types and the `StreamAccumulator` that turns wire fragments into unified
//! `ChatEvent`s. It is deliberately HTTP-free (no reqwest, no async runtime
//! beyond `tokio` `sync` for `CancellationToken`): the actual SSE adapters live
//! in `stepper-providers`, and the accumulator is unit-tested without a network.

pub mod accumulator;
pub mod error;
pub mod event;
pub mod message;
pub mod provider;
pub mod request;
pub mod response;
pub mod tool;
pub mod usage;
pub mod wire;

pub use accumulator::StreamAccumulator;
pub use error::ProviderError;
pub use event::{ChatEvent, StopReason};
pub use message::{ContentBlock, Message, Role};
pub use provider::{ChatStream, LlmProvider};
pub use request::{ChatRequest, ThinkingConfig, ToolChoice};
pub use response::ChatResponse;
pub use tool::{ToolContent, ToolError, ToolResult, ToolSpec};
pub use usage::Usage;
pub use wire::WireDelta;
