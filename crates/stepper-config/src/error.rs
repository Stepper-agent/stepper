use thiserror::Error;
use std::path::PathBuf;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid setting.json at {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("invalid model reference '{0}' (expected 'provider/model-id')")]
    InvalidModel(String),

    #[error("model '{model}' names provider '{provider}' which is not in providers")]
    UnknownProvider { model: String, provider: String },

    #[error("invalid frontmatter in {which}: {message}")]
    Frontmatter { which: String, message: String },

    #[error("substitution failed: {0}")]
    Substitution(String),
}
