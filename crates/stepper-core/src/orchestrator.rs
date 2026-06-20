use crate::agent::{AgentLoop, LspDiagnostics};
use crate::error::CoreError;
use crate::fanout::{run_parallel, FanoutTask};
use crate::hooks::HookHost;
use crate::layer::{FailurePolicy, Handoff, StepDef, SubTask};
use crate::model::ModelInfo;
use crate::ports::ProviderResolver;
use async_trait::async_trait;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use stepper_permission::{PermissionMode, RuleSet};
use stepper_provider::{ChatRequest, ChatStream, ContentBlock, LlmProvider, Message, ProviderError};
use stepper_protocol::{AppEvent, EventTx, LayerStatus, ModelView};
use stepper_tools::{Approver, Formatter, ToolCx, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// Session-wide safety caps (`--max-turns`, `--max-budget-usd`, `--turn-timeout`,
/// or their `setting.json` `limits` equivalents). The step cap and the wall-clock
/// timeout are per user turn (reset each `run_turn`); the budget covers the
/// accumulated cost of the whole session, tracked in `spent_microusd` across
/// turns. All `None` means no limit. The wall-clock timeout is enforced by the
/// turn driver (`run_watched`), not by `TurnBudget` (which gates per-step caps).
#[derive(Clone, Default)]
pub struct SessionLimits {
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub turn_timeout: Option<std::time::Duration>,
    pub spent_microusd: Arc<AtomicU64>,
}

impl SessionLimits {
    pub fn new(
        max_turns: Option<u32>,
        max_budget_usd: Option<f64>,
        turn_timeout: Option<std::time::Duration>,
    ) -> Self {
        // Normalize a zero/negative on any axis to "no limit" (single chokepoint
        // for every intake — CLI flag, setting.json, onboarding). Otherwise a
        // `--turn-timeout 0` / `turnTimeoutSecs: 0` would sleep for `Duration::ZERO`
        // and abort every turn instantly, and `maxBudgetUsd: 0` / `maxTurns: 0`
        // would trip on the first step.
        SessionLimits {
            max_turns: max_turns.filter(|&n| n > 0),
            max_budget_usd: max_budget_usd.filter(|&b| b > 0.0),
            turn_timeout: turn_timeout.filter(|d| !d.is_zero()),
            spent_microusd: Arc::default(),
        }
    }

    fn enabled(&self) -> bool {
        self.max_turns.is_some() || self.max_budget_usd.is_some()
    }
}

const TRIPPED_NONE: u8 = 0;
const TRIPPED_STEPS: u8 = 1;
const TRIPPED_BUDGET: u8 = 2;

/// Per-turn enforcement state for `SessionLimits`. Each provider request is one
/// ReAct step; `admit_step` gates it and `cap_error` reports which cap tripped
/// so `run_turn` can convert the resulting stop into a clear error.
pub struct TurnBudget {
    max_steps: Option<u32>,
    max_budget_microusd: Option<u64>,
    steps: AtomicU32,
    spent_microusd: Arc<AtomicU64>,
    tripped: AtomicU8,
}

impl TurnBudget {
    pub(crate) fn new(limits: &SessionLimits) -> Self {
        TurnBudget {
            max_steps: limits.max_turns,
            max_budget_microusd: limits.max_budget_usd.map(|b| (b * 1_000_000.0) as u64),
            steps: AtomicU32::new(0),
            spent_microusd: limits.spent_microusd.clone(),
            tripped: AtomicU8::new(TRIPPED_NONE),
        }
    }

    fn admit_step(&self) -> bool {
        if let Some(max) = self.max_steps
            && self.steps.fetch_add(1, Ordering::SeqCst) >= max
        {
            self.tripped.store(TRIPPED_STEPS, Ordering::SeqCst);
            return false;
        }
        if let Some(max) = self.max_budget_microusd
            && self.spent_microusd.load(Ordering::SeqCst) >= max
        {
            self.tripped.store(TRIPPED_BUDGET, Ordering::SeqCst);
            return false;
        }
        true
    }

    pub(crate) fn record_spend(&self, delta_microusd: u64) {
        self.spent_microusd
            .fetch_add(delta_microusd, Ordering::SeqCst);
    }

    fn cap_error(&self) -> Option<CoreError> {
        match self.tripped.load(Ordering::SeqCst) {
            TRIPPED_STEPS => Some(CoreError::MaxTurnsExceeded {
                cap: self.max_steps.unwrap_or(0),
            }),
            TRIPPED_BUDGET => Some(CoreError::BudgetExceeded {
                cap: self.max_budget_microusd.unwrap_or(0) as f64 / 1_000_000.0,
                spent: self.spent_microusd.load(Ordering::SeqCst) as f64 / 1_000_000.0,
            }),
            _ => None,
        }
    }
}

/// Wraps a layer's provider so every `chat_stream` call (= one ReAct step) is
/// admitted against the turn budget and its streamed usage is folded into the
/// session spend. A tripped cap surfaces as `ProviderError::Cancelled` so the
/// agent loop stops without retrying; `run_turn` converts it via `cap_error`.
struct BudgetedProvider {
    inner: Box<dyn LlmProvider>,
    budget: Arc<TurnBudget>,
    info: ModelInfo,
}

#[async_trait]
impl LlmProvider for BudgetedProvider {
    fn provider(&self) -> &str {
        self.inner.provider()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatStream, ProviderError> {
        if !self.budget.admit_step() {
            return Err(ProviderError::Cancelled);
        }
        let stream = self.inner.chat_stream(request, cancel).await?;
        let budget = self.budget.clone();
        let info = self.info;
        // Usage frames within one stream are cumulative; spend the delta against
        // the highest frame seen so far so partial frames never double-count.
        let mut last_cost_microusd = 0u64;
        Ok(stream
            .map(move |item| {
                if let Ok(stepper_provider::ChatEvent::Usage(usage)) = &item {
                    let cost = (info.cost(usage) * 1_000_000.0) as u64;
                    if cost > last_cost_microusd {
                        budget.record_spend(cost - last_cost_microusd);
                        last_cost_microusd = cost;
                    }
                }
                item
            })
            .boxed())
    }
}

pub(crate) fn budget_wrap(
    provider: Box<dyn LlmProvider>,
    budget: &Option<Arc<TurnBudget>>,
    info: ModelInfo,
) -> Box<dyn LlmProvider> {
    match budget {
        Some(budget) => Box::new(BudgetedProvider {
            inner: provider,
            budget: budget.clone(),
            info,
        }),
        None => provider,
    }
}

/// Runs the `step` pipeline: each layer gets a fresh context window and its own
/// provider, and its free-text outcome is threaded into the next layer.
pub struct Orchestrator {
    pub resolver: Arc<dyn ProviderResolver>,
    pub base_tools: ToolRegistry,
    pub steps: Vec<StepDef>,
    pub base_context: String,
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// Live session rules, mutated in place by AlwaysAllow grants and the
    /// `/allow|/deny|/ask` commands. `run_turn` snapshots it per turn; the
    /// approver and the slash commands hold a clone of the same cell.
    pub rules: Arc<RwLock<RuleSet>>,
    /// Live permission mode, mutated by Shift+Tab (`SetMode`) and the
    /// `exit_plan_mode` tool. The sequential layer's ToolCx holds a clone so an
    /// in-turn flip is seen immediately (B6 6b).
    pub mode: Arc<RwLock<PermissionMode>>,
    pub hooks: Arc<HookHost>,
    /// MCP servers with `alwaysLoad: true` — visible to every layer regardless of
    /// per-layer `mcp.allow`.
    pub always_load_mcp: Vec<String>,
    /// Optional model ref (`compaction.provider`) used to summarize folded
    /// history; `None` falls back to the heuristic marker.
    pub compaction_model: Option<String>,
    /// Expose the model-callable `dispatch` tool (parallel sub-agents). Off by
    /// default so the tool set the model sees is unchanged unless opted in.
    pub dispatch_enabled: bool,
    /// Max concurrent dispatched sub-agents (`dispatch.concurrency`, default 8).
    pub dispatch_concurrency: usize,
    /// Per-subtask ReAct step cap (`dispatch.stepCap`); `None` inherits the
    /// calling layer's `step_cap` so a delegated subtask isn't starved.
    pub dispatch_step_cap: Option<usize>,
    /// Safety caps (`--max-turns` / `--max-budget-usd`); `SessionLimits::default()`
    /// disables both.
    pub limits: SessionLimits,
    /// `--fallback-model`: when a step's primary model fails non-retryably or
    /// exhausts its retries, the layer is re-run once on this model.
    pub fallback_model: Option<String>,
    /// Real prior messages from a resumed session, prepended to every
    /// sequential layer's opening messages (the message-level analogue of the
    /// old resume digest in `base_context`). Empty for a fresh session.
    pub resume_seed: Vec<Message>,
    /// Writable roots for the opt-in OS bash sandbox (`settings.sandbox.enabled`),
    /// or `None` when disabled. Threaded into every layer/worker `ToolCx` so the
    /// `bash` tool can confine writes to the project + `additionalDirectories`.
    pub sandbox_writable_roots: Option<Vec<PathBuf>>,
    /// Active format-on-edit formatters (`settings.formatter`), shared into every
    /// layer/worker `AgentLoop`. Empty = disabled (the default).
    pub formatters: Arc<Vec<Formatter>>,
    /// Optional LSP diagnostics provider (`settings.lsp`), shared into every
    /// layer/worker. `None` = disabled (the default).
    pub lsp: Option<Arc<dyn LspDiagnostics>>,
    /// Named sub-agents (`.stepper/agents/`), exposed via the `task` tool. Empty =
    /// no `task` tool registered (the default).
    pub agents: Arc<Vec<crate::layer::AgentDef>>,
}

/// What one user turn produced: each layer's free-text outcome (the handoff
/// carriers, kept as the human-readable digest), the full normalized message
/// transcript persisted for full-fidelity resume, plus the turn's summed token
/// usage and its USD cost (priced per layer with that layer's model rates) for
/// the session-level `/cost` accounting.
#[derive(Debug)]
pub struct TurnOutput {
    pub summaries: Vec<(String, String)>,
    pub messages: Vec<Message>,
    pub usage: stepper_provider::Usage,
    pub cost_usd: f64,
}

impl Orchestrator {
    /// A consistent snapshot of the live rules for a turn/layer (so a mid-turn
    /// AlwaysAllow/`/allow` fold is picked up by the next snapshot, not mid-walk).
    pub(crate) fn rules_snapshot(&self) -> Arc<RuleSet> {
        Arc::new(self.rules.read().unwrap().clone())
    }

    /// The current live permission mode.
    pub(crate) fn mode_snapshot(&self) -> PermissionMode {
        *self.mode.read().unwrap()
    }

    /// One user turn: SessionStart hook → the layer pipeline → Stop hook. The Stop
    /// hook is guaranteed to fire on EVERY exit path (success, error, cancel) to
    /// mirror SessionStart — a teardown/cleanup/lock-release hook must not be
    /// skipped when a turn errors or is interrupted.
    pub async fn run_turn(
        &self,
        user_turn: String,
        images: Vec<(String, String)>,
        event_tx: &EventTx,
        approver: Arc<dyn Approver>,
        cancel: CancellationToken,
    ) -> Result<TurnOutput, CoreError> {
        let outcome = self
            .run_turn_inner(user_turn, images, event_tx, approver, cancel)
            .await;
        // Mirror SessionStart on every exit. Use a FRESH token (the turn's may
        // already be cancelled, which would make the hook itself bail immediately)
        // so a cleanup Stop hook still runs after an interrupted turn — bounded by
        // its own 30s timeout.
        let _ = self
            .hooks
            .run("Stop", None, &serde_json::json!({}), &CancellationToken::new())
            .await;
        outcome
    }

    async fn run_turn_inner(
        &self,
        user_turn: String,
        images: Vec<(String, String)>,
        event_tx: &EventTx,
        approver: Arc<dyn Approver>,
        cancel: CancellationToken,
    ) -> Result<TurnOutput, CoreError> {
        let total = self.steps.len();
        // The turn's persisted transcript: each sequential layer's opening
        // message plus everything it produced (resume seed excluded, so a
        // resumed session never re-saves its own history).
        let mut turn_messages: Vec<Message> = Vec::new();
        // Session-level accounting (`/cost`): the turn's summed usage, priced
        // per layer with that layer's model rates.
        let mut turn_usage = stepper_provider::Usage::default();
        let mut turn_cost_usd = 0.0_f64;

        let _ = self
            .hooks
            .run(
                "SessionStart",
                None,
                &serde_json::json!({ "turn": user_turn }),
                &cancel,
            )
            .await;

        let mut handoff = Handoff::new(user_turn, images);

        // Caps are enforced at the provider boundary: every chat_stream call is
        // one ReAct step, admitted against this turn's budget.
        let budget = self
            .limits
            .enabled()
            .then(|| Arc::new(TurnBudget::new(&self.limits)));

        // Resolve the compaction summarizer once for the whole turn (shared by
        // every layer); failure to resolve just falls back to the heuristic.
        let compaction_provider: Option<Arc<dyn stepper_provider::LlmProvider>> = self
            .compaction_model
            .as_ref()
            .and_then(|m| self.resolver.resolve(m).ok())
            .map(Arc::from);

        // The tool a layer uses to declare the *next* (parallel) layer's workers.
        // Registered only on the layer preceding a parallel one (below).
        let assign_tasks_tool: Arc<dyn stepper_tools::Tool> =
            Arc::new(crate::tasks::AssignTasksTool::default());

        // The most recent layer's `assign_tasks` list, consumed by the next layer
        // if it is `parallel` (one worker per task).
        let mut pending_tasks: Vec<SubTask> = Vec::new();

        for (index, step) in self.steps.iter().enumerate() {
            let provider = self.resolver.resolve(&step.model_ref)?;

            let _ = event_tx
                .send(AppEvent::ModelChanged(ModelView {
                    provider: provider.provider().to_string(),
                    model: provider.model().to_string(),
                }))
                .await;
            let _ = event_tx
                .send(AppEvent::LayerStarted {
                    index,
                    total,
                    name: step.name.clone(),
                })
                .await;

            // ── parallel layer: fan out the prior layer's task list ──
            // Every assigned task becomes a worker; `parallel_max` caps how many
            // run *at once* (the semaphore in run_parallel), never how many exist.
            if step.parallel && !pending_tasks.is_empty() {
                let subtasks: Vec<SubTask> = std::mem::take(&mut pending_tasks);
                let (summary, status, workers_usage) = self
                    .run_parallel_layer(
                        step,
                        &subtasks,
                        &handoff,
                        event_tx,
                        &approver,
                        &cancel,
                        &compaction_provider,
                        &budget,
                    )
                    .await;
                // A tripped cap stops every worker the same way an interrupt
                // would — surface it as the cap error, not a worker failure.
                if let Some(budget) = &budget
                    && let Some(e) = budget.cap_error()
                {
                    return Err(e);
                }
                // An interrupt makes every worker fail with Cancelled; surface that
                // as a clean cancellation rather than a ParallelLayerFailed error
                // (and stop the pipeline instead of continuing to the next layer).
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                turn_usage.add(&workers_usage);
                turn_cost_usd += self.resolver.model_info(&step.model_ref).cost(&workers_usage);
                if !summary.is_empty() {
                    let _ = event_tx
                        .send(AppEvent::AssistantTokenDelta(summary.clone()))
                        .await;
                    // Workers' own transcripts aren't persisted; their converged
                    // summary stands in for the layer in the session transcript.
                    turn_messages.push(Message::assistant(summary.clone()));
                }
                let _ = event_tx
                    .send(AppEvent::LayerFinished { index, status })
                    .await;
                if status == LayerStatus::Failed && matches!(step.on_failure, FailurePolicy::Stop) {
                    return Err(CoreError::ParallelLayerFailed {
                        layer: step.name.clone(),
                    });
                }
                handoff.prior.push((step.name.clone(), summary));
                continue;
            }

            let model_info = self.resolver.model_info(&step.model_ref);
            // The rates the successful run is priced at — swapped if the
            // fallback model ends up serving the layer.
            let mut active_info = model_info;
            let provider = budget_wrap(provider, &budget, model_info);
            let layer_rules = layer_ruleset(&self.rules_snapshot(), &step.permission);
            let mut tools = self
                .base_tools
                .filtered(&step.tool_allow, &step.tool_deny)
                .filter_mcp(&step.mcp_allow, &self.always_load_mcp);
            // Dispatched sub-agents inherit THIS layer's rules + scoped tool view
            // (taken before `dispatch` is registered, so no recursion) — they can
            // never escalate past the calling layer's permission/tool restrictions.
            // The `dispatch` tool (parallel generic sub-agents) and the `task` tool
            // (named sub-agents) share one dispatcher, built from THIS layer's
            // rules + scoped tool view (taken before either is registered, so no
            // recursion) — sub-agents can never escalate past the calling layer.
            if self.dispatch_enabled || !self.agents.is_empty() {
                let dispatcher = Arc::new(crate::dispatch::OrchestratorDispatcher {
                    resolver: self.resolver.clone(),
                    base_tools: tools.clone(),
                    hooks: self.hooks.clone(),
                    cwd: self.cwd.clone(),
                    project_root: self.project_root.clone(),
                    home: self.home.clone(),
                    rules: layer_rules.clone(),
                    mode: self.mode_snapshot(),
                    default_model: step.model_ref.clone(),
                    event_tx: event_tx.clone(),
                    approver: approver.clone(),
                    cancel: cancel.clone(),
                    compaction_provider: compaction_provider.clone(),
                    concurrency: self.dispatch_concurrency.max(1),
                    // Config `dispatch.stepCap` overrides; otherwise inherit the
                    // calling layer's step budget so a delegated subtask isn't
                    // starved relative to the main loop.
                    step_cap: self.dispatch_step_cap.unwrap_or(step.step_cap),
                    sandbox_writable_roots: self.sandbox_writable_roots.clone(),
                    // Gate sub-agents against this turn's budget (they cannot
                    // bypass --max-turns/--max-budget-usd) and hand them the
                    // project context so they aren't blind to the repo.
                    budget: budget.clone(),
                    base_context: self.base_context.clone(),
                    formatters: self.formatters.clone(),
                    lsp: self.lsp.clone(),
                });
                if self.dispatch_enabled {
                    tools.register(Arc::new(crate::dispatch::DispatchTool::new(dispatcher.clone())));
                }
                if !self.agents.is_empty() {
                    tools.register(Arc::new(crate::dispatch::TaskTool::new(
                        self.agents.clone(),
                        dispatcher,
                    )));
                }
            }
            // If the next layer is a parallel fan-out, this layer plans it.
            if self.steps.get(index + 1).is_some_and(|s| s.parallel) {
                tools.register(assign_tasks_tool.clone());
            }
            // This layer's skills, loadable on demand via the `skill` tool.
            if !step.skills.is_empty() {
                tools.register(Arc::new(crate::skills::SkillTool::new(step.skills.clone())));
            }
            // In plan mode the model can call `exit_plan_mode` to present its plan
            // and, on approval, flip the live mode to AcceptEdits this same turn.
            if self.mode_snapshot() == PermissionMode::Plan {
                tools.register(Arc::new(crate::exit_plan::ExitPlanTool::new(self.mode.clone())));
            }
            let system = self.system_for(step);
            let layer_initial = handoff.initial_messages();
            // A resumed session replays the real prior conversation ahead of
            // this layer's opening message (mirroring the digest's visibility
            // to every layer, but at full fidelity).
            let mut initial = self.resume_seed.clone();
            initial.extend(layer_initial.iter().cloned());

            let mut succeeded = None;
            let mut last_err = None;
            for attempt in 0..=step.retries {
                let cx = ToolCx {
                    cwd: self.cwd.clone(),
                    project_root: self.project_root.clone(),
                    home: self.home.clone(),
                    mode: self.mode_snapshot(),
                    // The sequential layer holds the live mode cell so an in-turn
                    // exit_plan_mode flip is seen by its own later tool calls.
                    live_mode: Some(self.mode.clone()),
                    rules: layer_rules.clone(),
                    approver: approver.clone(),
                    cancel: cancel.clone(),
                    sandbox_writable_roots: self.sandbox_writable_roots.clone(),
                };
                let agent = AgentLoop {
                    layer_name: step.name.clone(),
                    provider: provider.as_ref(),
                    tools: &tools,
                    cx,
                    event_tx: event_tx.clone(),
                    model_info,
                    step_cap: step.step_cap,
                    hooks: self.hooks.clone(),
                    compaction_provider: compaction_provider.clone(),
                    temperature: step.temperature,
                    top_p: step.top_p,
                    reasoning_effort: step.reasoning_effort.clone(),
                    thinking_budget: step.thinking_budget,
                    worker: None,
                    formatters: self.formatters.clone(),
                    lsp: self.lsp.clone(),
                };
                match agent.drive(system.clone(), initial.clone()).await {
                    Ok(outcome) => {
                        succeeded = Some(outcome);
                        break;
                    }
                    // A cancellation is a deliberate stop — never retried. When
                    // it was the budget wrapper that stopped the stream, surface
                    // the cap error instead of a silent cancellation.
                    Err(CoreError::Cancelled) => {
                        if let Some(budget) = &budget
                            && let Some(e) = budget.cap_error()
                        {
                            return Err(e);
                        }
                        return Err(CoreError::Cancelled);
                    }
                    Err(e) => {
                        // Layer retries are reserved for retryable failures
                        // (transient provider errors, step-cap nondeterminism);
                        // auth/config/4xx fail straight through to the fallback.
                        let retryable = e.is_retryable();
                        last_err = Some(e);
                        if !retryable {
                            break;
                        }
                        if attempt < step.retries {
                            let _ = event_tx
                                .send(AppEvent::Notice {
                                    level: stepper_protocol::NoticeLevel::Warn,
                                    text: format!(
                                        "layer '{}' failed, retrying ({}/{})",
                                        step.name,
                                        attempt + 1,
                                        step.retries
                                    ),
                                })
                                .await;
                        }
                    }
                }
            }

            // The primary model failed non-retryably or exhausted its retries:
            // re-resolve once with `--fallback-model` and notice the switch.
            if succeeded.is_none()
                && let Some(fallback_ref) = self
                    .fallback_model
                    .as_ref()
                    .filter(|f| f.as_str() != step.model_ref)
            {
                match self.resolver.resolve(fallback_ref) {
                    Ok(provider) => {
                        let reason = last_err
                            .as_ref()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "unknown error".into());
                        let _ = event_tx
                            .send(AppEvent::Notice {
                                level: stepper_protocol::NoticeLevel::Warn,
                                text: format!(
                                    "layer '{}' failed ({reason}); switching to fallback model '{fallback_ref}'",
                                    step.name
                                ),
                            })
                            .await;
                        let fallback_info = self.resolver.model_info(fallback_ref);
                        let provider = budget_wrap(provider, &budget, fallback_info);
                        let agent = AgentLoop {
                            layer_name: step.name.clone(),
                            provider: provider.as_ref(),
                            tools: &tools,
                            cx: ToolCx {
                                cwd: self.cwd.clone(),
                                project_root: self.project_root.clone(),
                                home: self.home.clone(),
                                mode: self.mode_snapshot(),
                                live_mode: Some(self.mode.clone()),
                                rules: layer_rules.clone(),
                                approver: approver.clone(),
                                cancel: cancel.clone(),
                                sandbox_writable_roots: self.sandbox_writable_roots.clone(),
                            },
                            event_tx: event_tx.clone(),
                            model_info: fallback_info,
                            step_cap: step.step_cap,
                            hooks: self.hooks.clone(),
                            compaction_provider: compaction_provider.clone(),
                            temperature: step.temperature,
                            top_p: step.top_p,
                            reasoning_effort: step.reasoning_effort.clone(),
                            thinking_budget: step.thinking_budget,
                            worker: None,
                            formatters: self.formatters.clone(),
                            lsp: self.lsp.clone(),
                        };
                        match agent.drive(system.clone(), initial.clone()).await {
                            Ok(outcome) => {
                                active_info = fallback_info;
                                succeeded = Some(outcome);
                            }
                            Err(CoreError::Cancelled) => {
                                if let Some(budget) = &budget
                                    && let Some(e) = budget.cap_error()
                                {
                                    return Err(e);
                                }
                                return Err(CoreError::Cancelled);
                            }
                            Err(e) => last_err = Some(e),
                        }
                    }
                    Err(e) => {
                        let _ = event_tx
                            .send(AppEvent::Notice {
                                level: stepper_protocol::NoticeLevel::Warn,
                                text: format!(
                                    "cannot resolve fallback model '{fallback_ref}': {e}"
                                ),
                            })
                            .await;
                    }
                }
            }

            match succeeded {
                Some(outcome) => {
                    let _ = event_tx
                        .send(AppEvent::LayerFinished {
                            index,
                            status: LayerStatus::Done,
                        })
                        .await;
                    turn_usage.add(&outcome.usage);
                    turn_cost_usd += active_info.cost(&outcome.usage);
                    turn_messages.extend(layer_initial);
                    turn_messages.extend(strip_thinking(outcome.messages));
                    // Carry any `assign_tasks` list forward to the next layer.
                    pending_tasks = outcome.tasks;
                    handoff.prior.push((step.name.clone(), outcome.summary));
                }
                None => {
                    let _ = event_tx
                        .send(AppEvent::LayerFinished {
                            index,
                            status: LayerStatus::Failed,
                        })
                        .await;
                    let e = last_err.expect("a failed layer recorded an error");
                    // A failed/skipped layer produces no fan-out list.
                    pending_tasks = Vec::new();
                    match step.on_failure {
                        FailurePolicy::Stop => return Err(e),
                        FailurePolicy::Skip => {
                            let _ = event_tx
                                .send(AppEvent::Notice {
                                    level: stepper_protocol::NoticeLevel::Warn,
                                    text: format!("layer '{}' skipped after failure: {e}", step.name),
                                })
                                .await;
                            handoff
                                .prior
                                .push((step.name.clone(), format!("[layer failed and was skipped: {e}]")));
                        }
                    }
                }
            }
        }

        // Stop runs in the `run_turn` wrapper so it fires on every exit path.
        Ok(TurnOutput {
            summaries: handoff.prior,
            messages: turn_messages,
            usage: turn_usage,
            cost_usd: turn_cost_usd,
        })
    }

    /// Run a `parallel` layer: one worker per subtask, concurrently (capped by
    /// `step.parallel_max`), each a fresh context window with this layer's config
    /// and the shared prior-layer handoff. Returns the converged summary (one
    /// section per worker, deterministic by label), the layer status (`Done` if
    /// any worker succeeded, else `Failed`), and the workers' summed usage.
    #[allow(clippy::too_many_arguments)]
    async fn run_parallel_layer(
        &self,
        step: &StepDef,
        subtasks: &[SubTask],
        handoff: &Handoff,
        event_tx: &EventTx,
        approver: &Arc<dyn Approver>,
        cancel: &CancellationToken,
        compaction_provider: &Option<Arc<dyn LlmProvider>>,
        budget: &Option<Arc<TurnBudget>>,
    ) -> (String, LayerStatus, stepper_provider::Usage) {
        // Reject an over-large fan-out (model misbehavior) rather than materialize
        // N FanoutTasks — fail the layer with a clear message, don't silently drop.
        if subtasks.len() > crate::fanout::MAX_FANOUT_WORKERS {
            return (
                format!(
                    "[parallel layer '{}' aborted: {} subtasks exceeds the {}-worker limit]",
                    step.name,
                    subtasks.len(),
                    crate::fanout::MAX_FANOUT_WORKERS
                ),
                LayerStatus::Failed,
                stepper_provider::Usage::default(),
            );
        }
        let layer_rules = layer_ruleset(&self.rules_snapshot(), &step.permission);
        // Workers are leaves: the step's tool view, with no `dispatch`/`assign_tasks`,
        // but they may load this layer's skills via the `skill` tool.
        let mut tools = self
            .base_tools
            .filtered(&step.tool_allow, &step.tool_deny)
            .filter_mcp(&step.mcp_allow, &self.always_load_mcp);
        if !step.skills.is_empty() {
            tools.register(Arc::new(crate::skills::SkillTool::new(step.skills.clone())));
        }
        let system = self.system_for(step);

        let mut tasks = Vec::new();
        let mut sections: Vec<(String, bool, String)> = Vec::new();
        // worker_index must be dense over the workers that actually spawn (a
        // resolve failure must not leave a gap / push an index past the total).
        let mut next_index = 0;
        for sub in subtasks {
            match self.resolver.resolve(&step.model_ref) {
                Ok(provider) => {
                    let worker_index = next_index;
                    next_index += 1;
                    let model_info = self.resolver.model_info(&step.model_ref);
                    tasks.push(FanoutTask {
                        label: sub.label.clone(),
                        worker_index,
                        provider: budget_wrap(provider, budget, model_info),
                        tools: tools.clone(),
                        cx: ToolCx {
                            cwd: self.cwd.clone(),
                            project_root: self.project_root.clone(),
                            home: self.home.clone(),
                            mode: self.mode_snapshot(),
                            // Workers don't run exit_plan; the static snapshot suffices.
                            live_mode: None,
                            rules: layer_rules.clone(),
                            approver: approver.clone(),
                            cancel: cancel.clone(),
                            sandbox_writable_roots: self.sandbox_writable_roots.clone(),
                        },
                        model_info: self.resolver.model_info(&step.model_ref),
                        step_cap: step.step_cap,
                        hooks: self.hooks.clone(),
                        compaction_provider: compaction_provider.clone(),
                        system: system.clone(),
                        messages: handoff.worker_messages(&sub.prompt),
                        formatters: self.formatters.clone(),
                        lsp: self.lsp.clone(),
                    });
                }
                Err(e) => sections.push((
                    sub.label.clone(),
                    false,
                    format!("could not resolve model '{}': {e}", step.model_ref),
                )),
            }
        }

        let results = run_parallel(tasks, step.parallel_max.max(1), event_tx.clone()).await;
        let mut workers_usage = stepper_provider::Usage::default();
        for (label, outcome) in results {
            match outcome {
                Ok(o) => {
                    workers_usage.add(&o.usage);
                    sections.push((label, true, o.summary));
                }
                Err(e) => sections.push((label, false, format!("failed: {e}"))),
            }
        }

        // run_parallel returns completion order; sort by label for a stable handoff.
        sections.sort_by(|a, b| a.0.cmp(&b.0));
        let any_ok = sections.iter().any(|(_, ok, _)| *ok);
        let mut merged = String::new();
        for (label, ok, body) in &sections {
            let mark = if *ok { "" } else { " (failed)" };
            merged.push_str(&format!("## {label}{mark}\n\n{body}\n\n"));
        }
        let status = if any_ok {
            LayerStatus::Done
        } else {
            LayerStatus::Failed
        };
        (merged.trim_end().to_string(), status, workers_usage)
    }

    fn system_for(&self, step: &StepDef) -> String {
        crate::setup::compose_system(&self.base_context, &step.system_prompt)
    }
}

/// Drop thinking blocks before persisting a transcript: they are bulky, only
/// signed ones could ever be replayed, and resume fidelity needs the visible
/// blocks (text, tool calls/results) only. Messages left empty are dropped.
fn strip_thinking(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|mut m| {
            m.content
                .retain(|b| !matches!(b, ContentBlock::Thinking { .. }));
            (!m.content.is_empty()).then_some(m)
        })
        .collect()
}

/// The base rules plus a layer's `(rule, decision)` permission overrides (or the
/// base rules unchanged when the layer has none).
fn layer_ruleset(base: &Arc<RuleSet>, overrides: &[(String, String)]) -> Arc<RuleSet> {
    if overrides.is_empty() {
        return base.clone();
    }
    let (mut allow, mut ask, mut deny) = (Vec::new(), Vec::new(), Vec::new());
    for (rule, decision) in overrides {
        match decision.trim().to_ascii_lowercase().as_str() {
            "allow" => allow.push(rule.clone()),
            "ask" => ask.push(rule.clone()),
            "deny" => deny.push(rule.clone()),
            _ => {}
        }
    }
    Arc::new(base.extended(&allow, &ask, &deny))
}
