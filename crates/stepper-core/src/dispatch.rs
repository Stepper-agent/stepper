use crate::fanout::{run_parallel, FanoutTask};
use crate::hooks::HookHost;
use crate::ports::ProviderResolver;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use stepper_permission::{PermissionMode, RuleSet};
use stepper_protocol::EventTx;
use stepper_provider::{LlmProvider, Message, ToolContent, ToolError, ToolResult, ToolSpec};
use stepper_tools::{Approver, Tool, ToolCx, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// One sub-agent to dispatch.
#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub label: String,
    pub prompt: String,
    pub model_ref: Option<String>,
}

/// A dispatched sub-agent's free-text outcome.
#[derive(Debug, Clone)]
pub struct DispatchResult {
    pub label: String,
    pub ok: bool,
    pub summary: String,
}

/// Runs a batch of sub-agents in parallel. Abstracted so the `dispatch` tool can
/// be unit-tested without a live orchestrator.
#[async_trait]
pub trait Dispatcher: Send + Sync {
    async fn dispatch(&self, requests: Vec<DispatchRequest>) -> Vec<DispatchResult>;
}

/// A model-callable tool that fans out parallel sub-agents (each its own context
/// window) and threads their summaries back. It owns its `Dispatcher` directly,
/// so `ToolCx` is unchanged.
pub struct DispatchTool {
    spec: ToolSpec,
    dispatcher: Arc<dyn Dispatcher>,
}

impl DispatchTool {
    pub fn new(dispatcher: Arc<dyn Dispatcher>) -> Self {
        let spec = ToolSpec {
            name: "dispatch".into(),
            description: "Run independent sub-agents in parallel, each with a fresh context window, \
                          and get their summaries back. Use for work that splits into independent \
                          subtasks."
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
                                "prompt": { "type": "string", "description": "the subtask instruction" },
                                "model": { "type": "string", "description": "optional model ref override" }
                            },
                            "required": ["prompt"]
                        }
                    }
                },
                "required": ["tasks"]
            }),
            read_only: false,
            parallel_safe: false,
        };
        DispatchTool { spec, dispatcher }
    }
}

#[async_trait]
impl Tool for DispatchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, _cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let tasks = args
            .get("tasks")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidArgs("`tasks` must be an array".into()))?;
        let requests: Vec<DispatchRequest> = tasks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let prompt = t.get("prompt")?.as_str()?.to_string();
                Some(DispatchRequest {
                    label: t
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("task-{i}")),
                    prompt,
                    model_ref: t.get("model").and_then(Value::as_str).map(str::to_string),
                })
            })
            .collect();
        if requests.is_empty() {
            return Err(ToolError::InvalidArgs(
                "no valid tasks (each task needs a `prompt`)".into(),
            ));
        }
        if requests.len() > crate::fanout::MAX_FANOUT_WORKERS {
            return Err(ToolError::InvalidArgs(format!(
                "too many tasks ({}); max {} per dispatch — split into fewer, broader subtasks",
                requests.len(),
                crate::fanout::MAX_FANOUT_WORKERS
            )));
        }

        let results = self.dispatcher.dispatch(requests).await;
        let any_error = results.iter().any(|r| !r.ok);
        let text = results
            .iter()
            .map(|r| {
                format!(
                    "## {} ({})\n{}",
                    r.label,
                    if r.ok { "ok" } else { "failed" },
                    r.summary
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(ToolResult {
            content: vec![ToolContent::text(text)],
            is_error: any_error,
            truncated: false,
        })
    }
}

/// The production `Dispatcher`: builds a `FanoutTask` per request from the
/// orchestrator's resolver + base tools and runs them with a concurrency cap.
/// Sub-agents get the base tool set only (no `dispatch`), so there is no
/// recursion.
pub struct OrchestratorDispatcher {
    pub resolver: Arc<dyn ProviderResolver>,
    pub base_tools: ToolRegistry,
    pub hooks: Arc<HookHost>,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    pub home: Option<PathBuf>,
    pub rules: Arc<RuleSet>,
    pub mode: PermissionMode,
    pub default_model: String,
    pub event_tx: EventTx,
    pub approver: Arc<dyn Approver>,
    pub cancel: CancellationToken,
    pub compaction_provider: Option<Arc<dyn LlmProvider>>,
    pub concurrency: usize,
    pub step_cap: usize,
}

#[async_trait]
impl Dispatcher for OrchestratorDispatcher {
    async fn dispatch(&self, requests: Vec<DispatchRequest>) -> Vec<DispatchResult> {
        let mut tasks = Vec::new();
        let mut failed = Vec::new();
        // Dense worker_index over the sub-agents that actually spawn — a model
        // override that fails to resolve must not leave a gap in the panel.
        let mut next_index = 0;
        for req in requests {
            let model_ref = req
                .model_ref
                .clone()
                .unwrap_or_else(|| self.default_model.clone());
            match self.resolver.resolve(&model_ref) {
                Ok(provider) => {
                    let model_info = self.resolver.model_info(&model_ref);
                    let worker_index = next_index;
                    next_index += 1;
                    tasks.push(FanoutTask {
                        label: req.label,
                        worker_index,
                        provider,
                        tools: self.base_tools.clone(),
                        cx: ToolCx {
                            cwd: self.cwd.clone(),
                            project_root: self.project_root.clone(),
                            home: self.home.clone(),
                            mode: self.mode,
                            rules: self.rules.clone(),
                            approver: self.approver.clone(),
                            cancel: self.cancel.clone(),
                        },
                        model_info,
                        step_cap: self.step_cap,
                        hooks: self.hooks.clone(),
                        compaction_provider: self.compaction_provider.clone(),
                        system: "You are a dispatched sub-agent. Complete the subtask and end with a \
                                 concise summary of what you did."
                            .into(),
                        messages: vec![Message::user(req.prompt)],
                    });
                }
                Err(e) => failed.push(DispatchResult {
                    label: req.label,
                    ok: false,
                    summary: format!("could not resolve model '{model_ref}': {e}"),
                }),
            }
        }

        let mut results: Vec<DispatchResult> =
            run_parallel(tasks, self.concurrency.max(1), self.event_tx.clone())
                .await
                .into_iter()
                .map(|(label, outcome)| match outcome {
                    Ok(o) => DispatchResult {
                        label,
                        ok: true,
                        summary: o.summary,
                    },
                    Err(e) => DispatchResult {
                        label,
                        ok: false,
                        summary: format!("failed: {e}"),
                    },
                })
                .collect();
        results.extend(failed);
        results
    }
}
