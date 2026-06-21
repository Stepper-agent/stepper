//! The built-in tool implementations.

pub mod apply_patch;
pub mod ask;
pub mod bash;
pub mod fetch;
pub mod files;
pub mod memory;
pub mod search;
pub mod todo;

use serde::de::DeserializeOwned;
use serde_json::Value;
use stepper_provider::ToolError;

/// Deserialize a tool's JSON arguments, mapping failures to `InvalidArgs`.
pub(crate) fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))
}
