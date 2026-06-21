use crate::context::ToolCx;
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use stepper_provider::{ToolError, ToolResult, ToolSpec};

/// `ask_user_question` — let the model surface a multiple-choice clarifying
/// question to the user and read back their pick. Routed through the approver
/// (oneshot → TUI picker); when no UI is attached (headless / sub-agent) it
/// resolves to "no answer" so the model proceeds on its best assumption.
pub struct AskUserQuestion {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct Args {
    question: String,
    options: Vec<String>,
}

impl Default for AskUserQuestion {
    fn default() -> Self {
        AskUserQuestion {
            spec: ToolSpec {
                name: "ask_user_question".into(),
                description: "Ask the user a single multiple-choice question to resolve a genuine \
                              ambiguity, and read back their choice. Provide 2-4 concise `options`. \
                              Use sparingly — only when the answer materially changes the work and \
                              you cannot reasonably decide yourself. Returns the option the user \
                              picked (or that they dismissed it)."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "the question to ask" },
                        "options": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 2,
                            "maxItems": 4,
                            "description": "2-4 distinct answer choices"
                        }
                    },
                    "required": ["question", "options"]
                }),
                // It changes nothing and is safe to run, but it blocks on the user,
                // so it is not parallel-safe.
                read_only: true,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for AskUserQuestion {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: Args = parse_args(args)?;
        let options: Vec<String> = a.options.into_iter().filter(|o| !o.trim().is_empty()).collect();
        if options.len() < 2 {
            return Err(ToolError::InvalidArgs(
                "ask_user_question needs at least 2 non-empty options".into(),
            ));
        }
        match cx.approver.ask(&a.question, &options).await {
            Some(i) if i < options.len() => {
                Ok(ToolResult::text(format!("The user selected: {}", options[i])))
            }
            _ => Ok(ToolResult::text(
                "The user did not answer (no interactive UI, or they dismissed the question). \
                 Proceed with your best judgment.",
            )),
        }
    }
}
