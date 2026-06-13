use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp config error: {0}")]
    Config(String),

    #[error("mcp connection error: {0}")]
    Connect(String),
}
