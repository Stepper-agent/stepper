use crate::agent::{AgentLoop, LayerOutcome, LspDiagnostics};
use crate::error::CoreError;
use crate::hooks::HookHost;
use crate::model::ModelInfo;
use std::sync::Arc;
use stepper_provider::{LlmProvider, Message};
use stepper_protocol::{AppEvent, EventTx, LayerStatus, ModelView};
use stepper_tools::{Formatter, ToolCx, ToolRegistry};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// One independent sub-agent to run in a fan-out batch. Everything is owned so
/// the task can move into a spawned future.
pub struct FanoutTask {
    pub label: String,
    /// This worker's position in the batch — drives its row in the live worker
    /// panel and tags its `WorkerActivity` events.
    pub worker_index: usize,
    pub provider: Box<dyn LlmProvider>,
    pub tools: ToolRegistry,
    pub cx: ToolCx,
    pub model_info: ModelInfo,
    pub step_cap: usize,
    pub hooks: Arc<HookHost>,
    pub compaction_provider: Option<Arc<dyn LlmProvider>>,
    pub system: String,
    pub messages: Vec<Message>,
    /// Format-on-edit formatters for this worker (shared from the orchestrator).
    pub formatters: Arc<Vec<Formatter>>,
    /// LSP diagnostics provider for this worker (shared from the orchestrator).
    pub lsp: Option<Arc<dyn LspDiagnostics>>,
}

/// Hard ceiling on the number of workers a single fan-out (parallel layer or
/// `dispatch`) may spawn. `parallel_max`/`concurrency` throttle how many run at
/// once; this bounds how many can EXIST, so a misbehaving/injected model can't
/// force an O(N) allocation burst (N FanoutTasks each holding the full handoff +
/// a provider). Exceeding it is rejected/failed, never silently truncated.
pub const MAX_FANOUT_WORKERS: usize = 32;

/// Guarantees a terminal `WorkerFinished` for a worker even if `agent.drive`
/// panics: the normal path sends it (backpressured) and disarms the guard, so on
/// panic the guard's `Drop` (which runs during unwind) emits `Failed` via the
/// sync `try_send`. Without this a panicked worker's panel row spins forever.
struct WorkerFinishGuard {
    index: usize,
    event_tx: EventTx,
    status: LayerStatus,
    fired: bool,
}

impl Drop for WorkerFinishGuard {
    fn drop(&mut self) {
        if !self.fired {
            let _ = self.event_tx.try_send(AppEvent::WorkerFinished {
                index: self.index,
                status: self.status,
            });
        }
    }
}

/// Run sub-agents concurrently with a concurrency cap (`JoinSet` + `Semaphore`).
/// Results come back in completion order; a task that errors yields `Err`. This
/// is the programmatic fan-out primitive — a model-callable `dispatch` tool that
/// triggers it is a follow-up.
pub async fn run_parallel(
    tasks: Vec<FanoutTask>,
    concurrency: usize,
    event_tx: EventTx,
) -> Vec<(String, Result<LayerOutcome, CoreError>)> {
    let semaphore = Arc::new(Semaphore::new(concurrency.max(1)));
    let total = tasks.len();
    let mut set = JoinSet::new();

    for task in tasks {
        // Announce the worker (its panel row) before it queues on a permit, so
        // every worker is visible even while some wait for a concurrency slot.
        let _ = event_tx
            .send(AppEvent::WorkerStarted {
                index: task.worker_index,
                total,
                label: task.label.clone(),
                model: ModelView {
                    provider: task.provider.provider().to_string(),
                    model: task.provider.model().to_string(),
                },
            })
            .await;
        let semaphore = semaphore.clone();
        let event_tx = event_tx.clone();
        set.spawn(async move {
            let _permit = semaphore.acquire().await;
            let index = task.worker_index;
            let mut finish = WorkerFinishGuard {
                index,
                event_tx: event_tx.clone(),
                status: LayerStatus::Failed,
                fired: false,
            };
            let agent = AgentLoop {
                layer_name: task.label.clone(),
                provider: task.provider.as_ref(),
                tools: &task.tools,
                cx: task.cx,
                event_tx: event_tx.clone(),
                model_info: task.model_info,
                step_cap: task.step_cap,
                hooks: task.hooks,
                compaction_provider: task.compaction_provider,
                temperature: None,
                top_p: None,
                reasoning_effort: None,
                thinking_budget: None,
                worker: Some(index),
                formatters: task.formatters,
                lsp: task.lsp,
            };
            let outcome = agent.drive(task.system, task.messages).await;
            let status = if outcome.is_ok() {
                LayerStatus::Done
            } else {
                LayerStatus::Failed
            };
            let _ = event_tx
                .send(AppEvent::WorkerFinished { index, status })
                .await;
            finish.fired = true; // normal path already sent it; disarm the guard
            (task.label, outcome)
        });
    }

    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(pair) => results.push(pair),
            Err(e) => results.push(("<panicked>".into(), Err(CoreError::Io(e.to_string())))),
        }
    }
    results
}
