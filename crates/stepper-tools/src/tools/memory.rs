use crate::context::ToolCx;
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;
use stepper_provider::{ToolContent, ToolError, ToolResult, ToolSpec};

/// The project memory file the agent appends durable learnings to. Read back into
/// the base context on the next session by `stepper-core`'s `load_base_context`.
pub const MEMORY_REL_PATH: &str = ".stepper/memory/MEMORY.md";

pub struct MemoryWrite {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct Args {
    note: String,
}

impl Default for MemoryWrite {
    fn default() -> Self {
        MemoryWrite {
            spec: ToolSpec {
                name: "memory_write".into(),
                description: "Append a durable learning to the project's memory file \
                              (.stepper/memory/MEMORY.md), which is loaded into your context at \
                              the start of every future session. Record things worth remembering \
                              across sessions — the build/test/lint commands, project conventions, \
                              hard-won debugging insights, gotchas — one concise note per call. Do \
                              not record secrets or anything already obvious from the code."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "note": { "type": "string", "description": "One concise learning to remember." }
                    },
                    "required": ["note"]
                }),
                read_only: false,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for MemoryWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: Args = parse_args(args)?;
        let note = a.note.trim();
        if note.is_empty() {
            return Err(ToolError::InvalidArgs("note is empty".into()));
        }
        // Fixed, safe, append-only path under the project — never an arbitrary
        // write, so this stays ungated (like `todo_write`): the agent must always
        // be able to record its own memory.
        let path = cx.project_root.join(MEMORY_REL_PATH);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| ToolError::Execution(e.to_string()))?;
        }
        let fresh = !path.exists();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        if fresh {
            file.write_all(b"# Project memory\n\n")
                .map_err(|e| ToolError::Execution(e.to_string()))?;
        }
        // Collapse newlines so one note stays one bullet.
        let line = note.split_whitespace().collect::<Vec<_>>().join(" ");
        writeln!(file, "- {line}").map_err(|e| ToolError::Execution(e.to_string()))?;
        Ok(ToolResult {
            content: vec![ToolContent::text("remembered")],
            is_error: false,
            truncated: false,
        })
    }
}
