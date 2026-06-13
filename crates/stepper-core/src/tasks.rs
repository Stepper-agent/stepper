use crate::layer::SubTask;
use async_trait::async_trait;
use serde_json::{json, Value};
use stepper_provider::{ToolError, ToolResult, ToolSpec};
use stepper_tools::{Tool, ToolCx};

/// Parse the `assign_tasks` tool input `{ "tasks": [{ label?, prompt }] }` into
/// the worker task list. Tasks missing a non-empty `prompt` are dropped; a
/// missing `label` is auto-named by index.
pub fn parse_subtasks(input: &Value) -> Vec<SubTask> {
    let Some(tasks) = input.get("tasks").and_then(Value::as_array) else {
        return Vec::new();
    };
    tasks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let prompt = t.get("prompt")?.as_str()?.trim();
            if prompt.is_empty() {
                return None;
            }
            Some(SubTask {
                label: t
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("worker-{}", i + 1)),
                prompt: prompt.to_string(),
            })
        })
        .collect()
}

/// A model-callable tool the orchestrator registers on the layer that precedes a
/// `parallel` layer. The model calls it to declare the subtasks; the captured
/// list (read off the tool input by the agent loop) becomes one worker each in
/// the next layer. The tool itself just acknowledges — it has no side effects and
/// needs no permission gate.
pub struct AssignTasksTool {
    spec: ToolSpec,
}

impl Default for AssignTasksTool {
    fn default() -> Self {
        let spec = ToolSpec {
            name: "assign_tasks".into(),
            description: "Split the upcoming parallel layer's work into independent subtasks. \
                          Each subtask becomes one concurrent worker (its own fresh context \
                          window). Call this once with the full list before finishing your turn."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": { "type": "string", "description": "short name for the subtask" },
                                "prompt": { "type": "string", "description": "the subtask instruction" }
                            },
                            "required": ["prompt"]
                        }
                    }
                },
                "required": ["tasks"]
            }),
            read_only: true,
            parallel_safe: false,
        };
        AssignTasksTool { spec }
    }
}

#[async_trait]
impl Tool for AssignTasksTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let tasks = parse_subtasks(&args);
        if tasks.is_empty() {
            return Err(ToolError::InvalidArgs(
                "no valid tasks (each task needs a non-empty `prompt`)".into(),
            ));
        }
        if tasks.len() > crate::fanout::MAX_FANOUT_WORKERS {
            return Err(ToolError::InvalidArgs(format!(
                "too many tasks ({}); max {} parallel workers — split into fewer, broader subtasks",
                tasks.len(),
                crate::fanout::MAX_FANOUT_WORKERS
            )));
        }
        let labels = tasks
            .iter()
            .map(|t| t.label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Ok(ToolResult::text(format!(
            "recorded {} subtask(s) for the next parallel layer: {labels}",
            tasks.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_labels_and_auto_names_missing_ones() {
        let input = json!({
            "tasks": [
                { "label": "api", "prompt": "build the api" },
                { "prompt": "build the db" },
                { "label": "  ", "prompt": "build the cli" }
            ]
        });
        let tasks = parse_subtasks(&input);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0], SubTask { label: "api".into(), prompt: "build the api".into() });
        assert_eq!(tasks[1].label, "worker-2", "missing label is auto-named by index");
        assert_eq!(tasks[2].label, "worker-3", "blank label is auto-named too");
    }

    #[test]
    fn drops_tasks_without_a_prompt() {
        let input = json!({ "tasks": [{ "label": "x" }, { "prompt": "   " }, { "prompt": "ok" }] });
        let tasks = parse_subtasks(&input);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].prompt, "ok");
    }

    #[test]
    fn missing_tasks_key_yields_empty() {
        assert!(parse_subtasks(&json!({})).is_empty());
        assert!(parse_subtasks(&json!({ "tasks": "nope" })).is_empty());
    }
}
