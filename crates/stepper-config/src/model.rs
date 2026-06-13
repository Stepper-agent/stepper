/// A model reference resolved to its `provider` and `model_id`. Accepts the
/// `provider/model-id` string form (splitting on the first `/`) or an explicit
/// split form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub provider: String,
    pub model_id: String,
}

impl ResolvedModel {
    pub fn parse(reference: &str) -> Option<Self> {
        let (provider, model_id) = reference.split_once('/')?;
        if provider.is_empty() || model_id.is_empty() {
            return None;
        }
        Some(ResolvedModel {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    pub fn from_split(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        ResolvedModel {
            provider: provider.into(),
            model_id: model_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_on_first_slash_only() {
        let m = ResolvedModel::parse("anthropic/claude-3/extra").unwrap();
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.model_id, "claude-3/extra");
    }

    #[test]
    fn parse_rejects_missing_slash() {
        assert_eq!(ResolvedModel::parse("no-slash"), None);
    }

    #[test]
    fn parse_rejects_empty_provider() {
        assert_eq!(ResolvedModel::parse("/model-id"), None);
    }

    #[test]
    fn parse_rejects_empty_model_id() {
        assert_eq!(ResolvedModel::parse("provider/"), None);
    }

    #[test]
    fn parse_rejects_both_empty() {
        assert_eq!(ResolvedModel::parse("/"), None);
        assert_eq!(ResolvedModel::parse(""), None);
    }

    #[test]
    fn from_split_accepts_str_and_string() {
        let a = ResolvedModel::from_split("omlx", "deepseek");
        let b = ResolvedModel::from_split(String::from("omlx"), String::from("deepseek"));
        assert_eq!(a, b);
        assert_eq!(
            a,
            ResolvedModel {
                provider: "omlx".into(),
                model_id: "deepseek".into()
            }
        );
    }
}
