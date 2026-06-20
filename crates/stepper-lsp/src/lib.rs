//! `stepper-lsp` — a minimal Language Server Protocol client that feeds
//! post-edit diagnostics back to the agent.
//!
//! After a file-editing tool runs, [`manager::LspManager`] routes the file to a
//! matching language server (detected on `PATH`, or a custom one from config),
//! syncs the document, and collects the server-pushed diagnostics so the model
//! sees compile/type errors it introduced. Servers are **not** downloaded — only
//! ones already installed are used (plus user-configured custom servers).
//!
//! Scope: the **push** diagnostics model (`textDocument/publishDiagnostics`),
//! which covers rust-analyzer, gopls, pyright, typescript-language-server, clangd,
//! etc. Pull/workspace diagnostics (`textDocument/diagnostic`) are a follow-up.

pub mod catalog;
pub mod client;
pub mod diagnostic;
pub mod language;
pub mod manager;
pub mod protocol;

pub use catalog::{builtin_catalog, which, BuiltinServer, ServerSpec};
pub use client::{LspClient, LspError};
pub use diagnostic::{report, Diagnostic};
pub use manager::LspManager;
