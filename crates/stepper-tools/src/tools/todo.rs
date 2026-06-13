use crate::context::ToolCx;
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use stepper_provider::{ToolContent, ToolError, ToolResult, ToolSpec};

pub struct TodoWrite {
    spec: ToolSpec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

#[derive(Deserialize)]
struct Args {
    todos: Vec<TodoItem>,
}

impl Default for TodoWrite {
    fn default() -> Self {
        TodoWrite {
            spec: ToolSpec {
                name: "todo_write".into(),
                description: "Replace the session's todo list. Each item has `content` and \
                              `status` (pending|in_progress|completed)."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": {"type": "string"},
                                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }),
                read_only: false,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for TodoWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: Args = parse_args(args)?;
        let in_progress = a.todos.iter().filter(|t| t.status == "in_progress").count();
        if in_progress > 1 {
            return Err(ToolError::InvalidArgs(
                "only one todo may be in_progress at a time".into(),
            ));
        }
        // The structured payload lets the orchestrator emit `TodoUpdated` to the
        // TUI; the text line is what the model sees.
        Ok(ToolResult {
            content: vec![
                ToolContent::text(format!("updated {} todo(s)", a.todos.len())),
                ToolContent::Json {
                    json: json!({ "todos": a.todos }),
                },
            ],
            is_error: false,
            truncated: false,
        })
    }
}
