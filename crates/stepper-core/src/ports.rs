use crate::error::CoreError;
use crate::model::ModelInfo;
use stepper_provider::LlmProvider;

/// Turns a `provider/model-id` reference into a live provider + its metadata.
/// Implemented by `ConfigProviderResolver` (config-driven) or by the CLI's
/// convention router when there is no `.stepper/`.
pub trait ProviderResolver: Send + Sync {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError>;
    fn model_info(&self, model_ref: &str) -> ModelInfo;
}
