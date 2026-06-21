use crate::fanout::{run_parallel, FanoutTask};
use crate::hooks::HookHost;
use crate::ports::ProviderResolver;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use stepper_permission::{PermissionMode, PermissionRequest, RuleSet};
use stepper_protocol::EventTx;
use stepper_provider::{LlmProvider, Message, ToolContent, ToolError, ToolResult, ToolSpec};
use stepper_tools::{Approval, Approver, Formatter, Tool, ToolCx, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// One sub-agent to dispatch.
#[derive(Debug, Clone, Default)]
pub struct DispatchRequest {
    pub label: String,
    pub prompt: String,
    pub model_ref: Option<String>,
    /// Named-agent role prompt (`None` = the generic dispatched-sub-agent role).
    /// Set by the `task` tool; composed with the project context into the system.
    pub role: Option<String>,
    /// Named-agent tool view (empty = inherit the dispatcher's full base tools).
    pub tool_allow: Vec<String>,
    pub tool_deny: Vec<String>,
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
                    ..Default::default()
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

/// A model-callable tool that delegates a subtask to a **named** sub-agent
/// (`.stepper/agents/<name>`), running it with that agent's own model, tool view,
/// and role prompt, gated by `permission` `Task(<name>)` rules. Reuses the same
/// `Dispatcher` as [`DispatchTool`] (a single-item batch).
pub struct TaskTool {
    spec: ToolSpec,
    dispatcher: Arc<dyn Dispatcher>,
    agents: Arc<Vec<crate::layer::AgentDef>>,
}

impl TaskTool {
    pub fn new(agents: Arc<Vec<crate::layer::AgentDef>>, dispatcher: Arc<dyn Dispatcher>) -> Self {
        let list = agents
            .iter()
            .map(|a| format!("- {}: {}", a.name, a.description))
            .collect::<Vec<_>>()
            .join("\n");
        let spec = ToolSpec {
            name: "task".into(),
            description: format!(
                "Delegate a self-contained subtask to a named sub-agent that runs autonomously in \
                 its own fresh context (its own model, tools, and role) and returns a summary. \
                 Choose the agent via `subagent_type`.\n\nAvailable agents:\n{list}"
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subagent_type": { "type": "string", "description": "which named agent to run" },
                    "description": { "type": "string", "description": "a short (3-5 word) task label" },
                    "prompt": { "type": "string", "description": "the full, self-contained task for the agent" }
                },
                "required": ["subagent_type", "prompt"]
            }),
            read_only: false,
            parallel_safe: false,
        };
        TaskTool {
            spec,
            dispatcher,
            agents,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn call(&self, args: Value, cx: &ToolCx) -> Result<ToolResult, ToolError> {
        let subagent_type = args
            .get("subagent_type")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("`subagent_type` is required".into()))?;
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgs("`prompt` is required".into()))?
            .to_string();
        let agent = self
            .agents
            .iter()
            .find(|a| a.name == subagent_type)
            .ok_or_else(|| {
                ToolError::InvalidArgs(format!(
                    "unknown subagent_type '{subagent_type}'; available: {}",
                    self.agents
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        // Gate which agents may be launched via `Task(<glob>)` rules (default:
        // Auto allows, other modes ask, dont-ask denies).
        cx.gate(
            PermissionRequest::Other {
                tool: "Task".into(),
                arg: subagent_type.to_string(),
            },
            Approval::Command {
                command: format!("run subagent: {subagent_type}"),
                outside_project: false,
            },
        )
        .await?;

        let label = args
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| subagent_type.to_string());
        let req = DispatchRequest {
            label,
            prompt,
            model_ref: agent.model_ref.clone(),
            role: Some(agent.role_prompt.clone()),
            tool_allow: agent.tool_allow.clone(),
            tool_deny: agent.tool_deny.clone(),
        };
        let result = self
            .dispatcher
            .dispatch(vec![req])
            .await
            .into_iter()
            .next()
            .ok_or_else(|| ToolError::Execution("the sub-agent produced no result".into()))?;
        Ok(ToolResult {
            content: vec![ToolContent::text(result.summary)],
            is_error: !result.ok,
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
    pub sandbox_writable_roots: Option<Vec<PathBuf>>,
    /// The calling turn's safety budget. Dispatched sub-agents are admitted
    /// against it just like the main layer, so they cannot bypass the caps.
    pub budget: Option<Arc<crate::orchestrator::TurnBudget>>,
    /// Project context (`stepper.md`, `@import`s) prepended to each sub-agent's
    /// system prompt — without it sub-agents run blind to the repo.
    pub base_context: String,
    /// Format-on-edit formatters, shared into each dispatched sub-agent worker.
    pub formatters: Arc<Vec<Formatter>>,
    /// LSP diagnostics provider, shared into each dispatched sub-agent worker.
    pub lsp: Option<Arc<dyn crate::agent::LspDiagnostics>>,
}

impl OrchestratorDispatcher {
    const SUBAGENT_ROLE: &'static str =
        "You are a dispatched sub-agent. Complete the subtask and end with a concise summary of what you did.";

    /// Sub-agent system prompt: the project context (mirroring the main layers
    /// via `compose_system`) plus the sub-agent role, so dispatched work is not
    /// blind to the repo.
    fn subagent_system(&self) -> String {
        crate::setup::compose_system(&self.base_context, Self::SUBAGENT_ROLE)
    }
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
                    // Gate sub-agent steps/spend against the shared turn budget.
                    let provider =
                        crate::orchestrator::budget_wrap(provider, &self.budget, model_info);
                    let worker_index = next_index;
                    next_index += 1;
                    // A named agent (the `task` tool) carries its own role prompt
                    // and tool view; a generic dispatch leaves them empty and gets
                    // the standard sub-agent role + full base tools.
                    let system = match &req.role {
                        Some(role) => crate::setup::compose_system(&self.base_context, role),
                        None => self.subagent_system(),
                    };
                    let tools = if req.tool_allow.is_empty() && req.tool_deny.is_empty() {
                        self.base_tools.clone()
                    } else {
                        self.base_tools.filtered(&req.tool_allow, &req.tool_deny)
                    };
                    tasks.push(FanoutTask {
                        label: req.label,
                        worker_index,
                        provider,
                        tools,
                        cx: ToolCx {
                            cwd: self.cwd.clone(),
                            project_root: self.project_root.clone(),
                            home: self.home.clone(),
                            mode: self.mode,
                            // Dispatched sub-agents use the static snapshot mode.
                            live_mode: None,
                            rules: self.rules.clone(),
                            approver: self.approver.clone(),
                            cancel: self.cancel.clone(),
                            sandbox_writable_roots: self.sandbox_writable_roots.clone(),
                        },
                        model_info,
                        step_cap: self.step_cap,
                        hooks: self.hooks.clone(),
                        compaction_provider: self.compaction_provider.clone(),
                        system,
                        messages: vec![Message::user(req.prompt)],
                        formatters: self.formatters.clone(),
                        lsp: self.lsp.clone(),
                        budget: self.budget.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ModelInfo;
    use crate::orchestrator::TurnBudget;
    use crate::{CoreError, SessionLimits};
    use stepper_permission::Decision;
    use stepper_protocol::AppEvent;
    use stepper_provider::{ChatEvent, ChatRequest, ChatStream, ProviderError, StopReason};
    use stepper_tools::Approval;

    struct TextProvider;
    #[async_trait]
    impl LlmProvider for TextProvider {
        fn provider(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "m"
        }
        async fn chat_stream(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
        ) -> Result<ChatStream, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatEvent::Done(
                StopReason::EndTurn,
            ))])))
        }
    }

    struct OneResolver;
    impl ProviderResolver for OneResolver {
        fn resolve(&self, _model_ref: &str) -> Result<Box<dyn LlmProvider>, CoreError> {
            Ok(Box::new(TextProvider))
        }
        fn model_info(&self, _model_ref: &str) -> ModelInfo {
            ModelInfo {
                context_window: 1000,
                max_output_tokens: 0,
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
                cache_read_per_mtok: 0.0,
                cache_write_per_mtok: 0.0,
                estimated: false,
            }
        }
    }

    struct AllowAll;
    #[async_trait]
    impl Approver for AllowAll {
        async fn request(&self, _approval: Approval) -> Decision {
            Decision::Allow
        }
    }

    fn dispatcher(
        budget: Option<Arc<TurnBudget>>,
        base_context: &str,
        tx: EventTx,
        root: PathBuf,
    ) -> OrchestratorDispatcher {
        OrchestratorDispatcher {
            resolver: Arc::new(OneResolver),
            base_tools: ToolRegistry::builtins(),
            hooks: Arc::new(HookHost::empty(root.clone())),
            cwd: root.clone(),
            project_root: root,
            home: None,
            rules: Arc::new(RuleSet::default()),
            mode: PermissionMode::AcceptEdits,
            default_model: "fake/m".into(),
            event_tx: tx,
            approver: Arc::new(AllowAll),
            cancel: CancellationToken::new(),
            compaction_provider: None,
            concurrency: 2,
            step_cap: 4,
            sandbox_writable_roots: None,
            budget,
            base_context: base_context.to_string(),
            formatters: Arc::new(Vec::new()),
            lsp: None,
        }
    }

    /// A dispatched sub-agent is admitted against the turn budget like any other
    /// layer: once the session budget is spent, dispatch cannot run more work
    /// (regression — sub-agents previously used the raw, unbudgeted provider).
    #[tokio::test(flavor = "multi_thread")]
    async fn dispatched_subagents_are_gated_by_the_turn_budget() {
        let dir = tempfile::tempdir().unwrap();
        let limits = SessionLimits::new(None, Some(0.001), None);
        let budget = Arc::new(TurnBudget::new(&limits));
        budget.record_spend(2_000); // already past the 1_000 microusd ($0.001) cap
        let (tx, _rx) = tokio::sync::mpsc::channel::<AppEvent>(64);

        let dispatcher = dispatcher(Some(budget), "", tx, dir.path().to_path_buf());
        let results = dispatcher
            .dispatch(vec![DispatchRequest {
                label: "blocked".into(),
                prompt: "do work".into(),
                model_ref: None,
                ..Default::default()
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            !results[0].ok,
            "a spent budget must block the dispatched sub-agent: {:?}",
            results[0]
        );
    }

    #[test]
    fn subagent_system_prepends_project_context() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel::<AppEvent>(8);
        let d = dispatcher(None, "PROJECT_CONTEXT_MARKER", tx, dir.path().to_path_buf());
        let sys = d.subagent_system();
        assert!(sys.contains("PROJECT_CONTEXT_MARKER"), "project context is included");
        assert!(sys.contains("dispatched sub-agent"), "the role is included");
    }
}
