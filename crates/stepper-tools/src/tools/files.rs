use crate::context::{Approval, ToolCx};
use crate::secret::is_secret_path_resolved;
use crate::tools::parse_args;
use crate::Tool;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use similar::TextDiff;
use stepper_permission::PermissionRequest;
use stepper_provider::{ToolError, ToolResult, ToolSpec};

const MAX_READ_BYTES: usize = 256 * 1024;

pub struct ReadFile {
    spec: ToolSpec,
}
pub struct WriteFile {
    spec: ToolSpec,
}
pub struct EditFile {
    spec: ToolSpec,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

impl Default for ReadFile {
    fn default() -> Self {
        ReadFile {
            spec: ToolSpec {
                name: "read_file".into(),
                description: "Read a UTF-8 text file. Supports line offset/limit.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer", "description": "1-based start line"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["path"]
                }),
                read_only: true,
                parallel_safe: true,
            },
        }
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: ReadArgs = parse_args(args)?;
        let path = cx.resolve(&a.path);
        if is_secret_path_resolved(&path) {
            return Err(ToolError::Denied(format!(
                "refusing to read secret file {}",
                path.display()
            )));
        }
        cx.gate(
            PermissionRequest::Read(path.clone()),
            Approval::OutsideProject {
                path: path.clone(),
                action: "read".into(),
            },
        )
        .await?;

        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", path.display())))?;
        let text = String::from_utf8_lossy(&raw);

        let body = match (a.offset, a.limit) {
            // A whole-file read of a large file is rejected (it would blow the
            // context window), but `offset`/`limit` slice FIRST so a window of a
            // big file is still readable; only the slice is size-capped.
            (None, None) => {
                if raw.len() > MAX_READ_BYTES {
                    return Err(ToolError::Execution(format!(
                        "file is {} bytes (> {MAX_READ_BYTES} limit); pass offset/limit to read a slice",
                        raw.len()
                    )));
                }
                text.into_owned()
            }
            (offset, limit) => {
                let start = offset.unwrap_or(1).saturating_sub(1);
                let mut sliced = text
                    .lines()
                    .skip(start)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect::<Vec<_>>()
                    .join("\n");
                if sliced.len() > MAX_READ_BYTES {
                    crate::truncate_on_char_boundary(&mut sliced, MAX_READ_BYTES);
                    sliced.push_str("\n… [truncated at the read-size limit — narrow the offset/limit]");
                }
                sliced
            }
        };
        Ok(ToolResult::text(body))
    }
}

impl Default for WriteFile {
    fn default() -> Self {
        WriteFile {
            spec: ToolSpec {
                name: "write_file".into(),
                description: "Create or overwrite a file with the given content.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
                read_only: false,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: WriteArgs = parse_args(args)?;
        let path = cx.resolve(&a.path);
        if is_secret_path_resolved(&path) {
            return Err(ToolError::Denied(format!(
                "refusing to write secret file {}",
                path.display()
            )));
        }
        let old = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        cx.gate(
            PermissionRequest::Write(path.clone()),
            Approval::FileEdit {
                path: path.clone(),
                old,
                new: a.content.clone(),
            },
        )
        .await?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", parent.display())))?;
        }
        tokio::fs::write(&path, &a.content)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", path.display())))?;
        Ok(ToolResult::text(format!(
            "wrote {} bytes to {}",
            a.content.len(),
            path.display()
        )))
    }
}

impl Default for EditFile {
    fn default() -> Self {
        EditFile {
            spec: ToolSpec {
                name: "edit_file".into(),
                description: "Replace an exact string in a file. `old_string` must be unique \
                              unless `replace_all` is set."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"},
                        "replace_all": {"type": "boolean"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
                read_only: false,
                parallel_safe: false,
            },
        }
    }
}

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let a: EditArgs = parse_args(args)?;
        let path = cx.resolve(&a.path);
        if is_secret_path_resolved(&path) {
            return Err(ToolError::Denied(format!(
                "refusing to edit secret file {}",
                path.display()
            )));
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", path.display())))?;

        let occurrences = content.matches(&a.old_string).count();
        if occurrences == 0 {
            return Err(ToolError::InvalidArgs("old_string not found in file".into()));
        }
        if occurrences > 1 && !a.replace_all {
            return Err(ToolError::InvalidArgs(format!(
                "old_string is not unique ({occurrences} matches); set replace_all"
            )));
        }
        let updated = if a.replace_all {
            content.replace(&a.old_string, &a.new_string)
        } else {
            content.replacen(&a.old_string, &a.new_string, 1)
        };

        cx.gate(
            PermissionRequest::Edit(path.clone()),
            Approval::FileEdit {
                path: path.clone(),
                old: content.clone(),
                new: updated.clone(),
            },
        )
        .await?;

        tokio::fs::write(&path, &updated)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", path.display())))?;

        let diff = TextDiff::from_lines(&content, &updated)
            .unified_diff()
            .header("before", "after")
            .to_string();
        Ok(ToolResult::text(format!(
            "edited {} ({occurrences} replacement(s))\n{diff}",
            path.display()
        )))
    }
}
