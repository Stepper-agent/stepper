use crate::error::CoreError;
use crate::model::ModelInfo;
use stepper_protocol::ModelChoiceView;
use stepper_provider::LlmProvider;

/// Turns a `provider/model-id` reference into a live provider + its metadata.
/// Implemented by `ConfigProviderResolver` (config-driven) or by the CLI's
/// convention router when there is no `.stepper/`.
#[async_trait::async_trait]
pub trait ProviderResolver: Send + Sync {
    fn resolve(&self, model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError>;
    fn model_info(&self, model_ref: &str) -> ModelInfo;
    /// List selectable models across the configured providers — each provider's
    /// live list endpoint merged with the models.dev catalog. Defaults to none so
    /// convention/test resolvers need not implement discovery.
    async fn list_models(&self) -> Vec<ModelChoiceView> {
        Vec::new()
    }
}
