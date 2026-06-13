//! Per-dialect wire encoders/decoders. Each module builds a request body from
//! the neutral `ChatRequest` and parses one raw SSE frame into `Vec<WireDelta>`;
//! the dialect-agnostic `StreamAccumulator` (in `stepper-provider`) does the
//! fragment reassembly so there is one place that can be wrong.

pub mod anthropic;
pub mod openai;
pub mod responses;
