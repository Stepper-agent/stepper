//! `stepper-mcp` — MCP (Model Context Protocol) client. Connects to configured
//! servers (stdio child process or streamable HTTP) and bridges their tools into
//! the native `Tool` trait with `mcp__server__tool` namespacing, so the model
//! and the permission engine treat them like built-in tools.

pub mod bridge;
pub mod error;
pub mod manager;
pub mod oauth;
pub mod tool;

pub use bridge::namespaced_name;
pub use error::McpError;
pub use manager::McpManager;
pub use oauth::{authenticate, logout, status, McpOAuthStatus};
pub use tool::McpTool;
