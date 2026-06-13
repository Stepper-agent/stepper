use thiserror::Error;
use stepper_provider::ProviderError;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error("config error: {0}")]
    Config(String),

    #[error("no model configured for layer '{0}'")]
    NoModel(String),

    #[error("layer '{layer}' hit its step cap of {cap} without finishing")]
    StepCapHit { layer: String, cap: usize },

    #[error("every worker of parallel layer '{layer}' failed")]
    ParallelLayerFailed { layer: String },

    #[error("turn aborted: the --max-turns cap of {cap} ReAct step(s) was reached")]
    MaxTurnsExceeded { cap: u32 },

    #[error("turn aborted: session cost ${spent:.4} reached the --max-budget-usd cap of ${cap}")]
    BudgetExceeded { cap: f64, spent: f64 },

    #[error("turn cancelled")]
    Cancelled,

    #[error("io error: {0}")]
    Io(String),

    #[error("session error: {0}")]
    Session(String),
}

impl CoreError {
    /// Whether re-running the layer might succeed. Provider failures delegate to
    /// `ProviderError::is_retryable` (429/5xx/transport/truncated stream); a step
    /// cap is model nondeterminism so another attempt may converge. Everything
    /// else (config, auth, cancellation, session caps) is deterministic.
    pub fn is_retryable(&self) -> bool {
        match self {
            CoreError::Provider(e) => e.is_retryable(),
            CoreError::StepCapHit { .. } => true,
            _ => false,
        }
    }
}

impl From<stepper_config::ConfigError> for CoreError {
    fn from(e: stepper_config::ConfigError) -> Self {
        CoreError::Config(e.to_string())
    }
}
