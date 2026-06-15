use crate::compaction::Compactor;
use crate::error::CoreError;
use crate::hooks::{HookDecision, HookHost};
use crate::layer::SubTask;
use crate::model::ModelInfo;
use futures::StreamExt;
use std::sync::Arc;
use serde_json::Value;
use stepper_protocol::{
    AppEvent, EventTx, TodoItemView, TodoStatus, ToolCallView, UsageView,
};
use stepper_provider::{
    ChatEvent, ChatRequest, ChatResponse, ContentBlock, LlmProvider, Message, Role, StopReason,
    Usage,
};
use stepper_tools::{ToolCx, ToolRegistry};

pub struct LayerOutcome {
    pub summary: String,
    pub usage: Usage,
    /// Subtasks the layer declared via `assign_tasks` (one worker each for the
    /// next `parallel` layer). Empty unless the layer called the tool.
    pub tasks: Vec<SubTask>,
    /// Every message the loop produced (assistant replies + tool results), in
    /// order, excluding the seeded initial messages. Recorded outside the live
    /// window so compaction never erodes the session transcript.
    pub messages: Vec<Message>,
}

/// Drives one layer's ReAct loop against its provider until the model stops
/// asking for tools (or the step cap is hit).
pub struct AgentLoop<'a> {
    pub layer_name: String,
    pub provider: &'a dyn LlmProvider,
    pub tools: &'a ToolRegistry,
    pub cx: ToolCx,
    pub event_tx: EventTx,
    pub model_info: ModelInfo,
    pub step_cap: usize,
    pub hooks: Arc<HookHost>,
    /// Optional (typically cheap) model that summarizes folded-away history; when
    /// absent, compaction uses the built-in heuristic marker.
    pub compaction_provider: Option<Arc<dyn LlmProvider>>,
    /// Sampling overrides forwarded to the provider (None = provider default).
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Reasoning overrides: OpenAI-family `reasoning_effort` and Anthropic
    /// extended-thinking budget (None = off / provider default).
    pub reasoning_effort: Option<String>,
    pub thinking_budget: Option<u32>,
    /// When `Some(i)`, this loop is fan-out worker `i`: its token/tool/usage
    /// progress is emitted as per-worker `WorkerActivity` (so the TUI can show it
    /// in the worker panel) instead of the global stream, which would otherwise
    /// interleave N workers into one buffer.
    pub worker: Option<usize>,
}

impl AgentLoop<'_> {
    pub async fn drive(
        &self,
        system: String,
        mut messages: Vec<Message>,
    ) -> Result<LayerOutcome, CoreError> {
        let mut total = Usage::default();
        let tool_specs = self.tools.specs();
        // Plan against the window minus the output cap so the reply always fits.
        let compactor = Compactor::with_output_reserve(
            self.model_info.context_window,
            self.model_info.max_output_tokens,
        );
        // `last_context` is the provider-reported size of the last request
        // (input + output) and `accounted` is how many leading messages it
        // covers; everything appended since is estimated at chars/4. The plan
        // therefore sees the CURRENT request — the overflowing step compacts
        // itself instead of running one step late — and a seeded first step
        // (accounted = 0) is estimated in full.
        let mut last_context = 0u64;
        let mut accounted = 0usize;
        let mut captured_tasks: Vec<SubTask> = Vec::new();
        // One bounded "you described an action but didn't take it — keep going"
        // nudge per turn: the code-level backstop to the AGENT_DIRECTIVES prompt
        // for weak models that narrate then stop without calling the tool.
        let mut nudged = false;
        // Bounded continuations when the output is truncated at the token cap, so a
        // cut-off mid-sentence reply isn't mistaken for a finished turn.
        let mut continuations = 0u32;
        const MAX_CONTINUATIONS: u32 = 3;
        let mut produced: Vec<Message> = Vec::new();

        for _step in 0..self.step_cap.max(1) {
            if self.cx.cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }

            let projected = last_context
                + crate::compaction::estimate_tokens(&messages[accounted.min(messages.len())..]);
            if let Some(cut) = compactor.plan(&messages, projected) {
                // A worker still compacts its own context, but silently: the
                // compaction notice is a shared global surface (N workers would
                // clobber it), so only the main thread announces it.
                if self.worker.is_none() {
                    let _ = self.event_tx.send(AppEvent::CompactionStarted).await;
                }
                let dropped: Vec<Message> = messages.drain(0..cut).collect();
                let freed_tokens = crate::compaction::estimate_tokens(&dropped);
                let summary = match &self.compaction_provider {
                    Some(p) => crate::compaction::summarize_with_model(p.as_ref(), &dropped, None)
                        .await
                        .unwrap_or_else(|| crate::compaction::heuristic_summary(&dropped)),
                    None => crate::compaction::heuristic_summary(&dropped),
                };
                messages.insert(0, crate::compaction::marker(&summary));
                // The prefix the provider last measured is gone; estimate the
                // whole compacted list until the next usage frame arrives.
                last_context = 0;
                accounted = 0;
                if self.worker.is_none() {
                    let _ = self
                        .event_tx
                        .send(AppEvent::CompactionDone { freed_tokens })
                        .await;
                }
            }

            let request = ChatRequest {
                model: self.provider.model().to_string(),
                system: Some(system.clone()),
                messages: messages.clone(),
                tools: tool_specs.clone(),
                tool_choice: Default::default(),
                // The registry's per-model output cap (0 = provider default), so
                // Anthropic isn't silently capped at its 4096 wire default.
                max_tokens: (self.model_info.max_output_tokens > 0)
                    .then_some(self.model_info.max_output_tokens as u32),
                temperature: self.temperature,
                top_p: self.top_p,
                stop: Vec::new(),
                thinking: self
                    .thinking_budget
                    .map(|budget_tokens| stepper_provider::ThinkingConfig { budget_tokens }),
                reasoning_effort: self.reasoning_effort.clone(),
                // The per-layer system + tools prefix is stable across every ReAct
                // step, so cache it (Anthropic; no-op for the other dialects).
                cache: true,
            };

            let (response, step_usage) = self.stream_once(request, &mut total).await?;

            let tool_calls: Vec<(String, String, Value)> = response
                .tool_uses()
                .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
                .collect();

            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });
            produced.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            // The usage frame covers the request plus this assistant reply, so it
            // accounts for the list up to and including the message just pushed.
            // A provider that reports no usage keeps the chars/4 estimate live.
            let observed = step_usage.input + step_usage.output;
            if observed > 0 {
                last_context = observed;
                accounted = messages.len();
            }

            if tool_calls.is_empty() {
                // Truncated at the output cap → not a completion. Ask it to resume
                // from where it was cut off (bounded so a model that always maxes
                // out can't loop forever; the step cap is the final backstop).
                if matches!(response.stop_reason, StopReason::MaxTokens)
                    && continuations < MAX_CONTINUATIONS
                {
                    continuations += 1;
                    let cont = Message::user(
                        "Your previous message was cut off at the output limit. \
                         Continue exactly where you left off — do not repeat what you already wrote.",
                    );
                    messages.push(cont.clone());
                    produced.push(cont);
                    continue;
                }
                // Narrate-then-stop backstop: if the model ended its turn while
                // its text still reads like an unfulfilled intent (or is empty),
                // nudge it once to actually act rather than returning a non-answer.
                if !nudged && looks_unfinished(&response.text()) {
                    nudged = true;
                    let nudge = Message::user(
                        "You described what to do next but did not do it, or returned nothing. \
                         Continue now: use the tools to actually carry out the step. \
                         Only stop once the task is truly complete.",
                    );
                    messages.push(nudge.clone());
                    produced.push(nudge);
                    continue;
                }
                return Ok(LayerOutcome {
                    summary: response.text(),
                    usage: total,
                    tasks: captured_tasks,
                    messages: produced,
                });
            }

            let mut results = Vec::new();
            for (id, name, input) in tool_calls {
                // The prior layer declares the next parallel layer's workers via
                // `assign_tasks`; capture the latest non-empty list (mirroring the
                // tool, which rejects an empty list — a rejected retry must not
                // wipe a previously declared plan).
                if name == "assign_tasks" {
                    let parsed = crate::tasks::parse_subtasks(&input);
                    if !parsed.is_empty() {
                        captured_tasks = parsed;
                    }
                }
                let block = self.run_tool(&id, &name, input).await;
                results.push(block);
            }
            messages.push(Message {
                role: Role::Tool,
                content: results.clone(),
            });
            produced.push(Message {
                role: Role::Tool,
                content: results,
            });
        }

        Err(CoreError::StepCapHit {
            layer: self.layer_name.clone(),
            cap: self.step_cap.max(1),
        })
    }

    /// Stream one assistant turn with request-level retry: transient failures
    /// (429/5xx/transport/truncated stream) back off exponentially with jitter
    /// for up to `MAX_REQUEST_RETRIES` retries, surfaced as `api retry n/3`
    /// notices; auth/4xx/cancellation fail immediately.
    async fn stream_once(
        &self,
        request: ChatRequest,
        total: &mut Usage,
    ) -> Result<(ChatResponse, Usage), CoreError> {
        let mut attempt = 0u32;
        loop {
            match self.stream_attempt(request.clone(), total).await {
                Err(CoreError::Provider(e)) if e.is_retryable() && attempt < MAX_REQUEST_RETRIES => {
                    attempt += 1;
                    // Workers stay quiet on the shared notice surface.
                    if self.worker.is_none() {
                        let _ = self
                            .event_tx
                            .send(AppEvent::Notice {
                                level: stepper_protocol::NoticeLevel::Warn,
                                text: format!("api retry {attempt}/{MAX_REQUEST_RETRIES}: {e}"),
                            })
                            .await;
                    }
                    tokio::select! {
                        _ = self.cx.cancel.cancelled() => return Err(CoreError::Cancelled),
                        _ = tokio::time::sleep(retry_backoff(attempt)) => {}
                    }
                }
                outcome => return outcome,
            }
        }
    }

    /// One streaming attempt: forward token/usage events to the TUI and fold
    /// the events into a `ChatResponse`. A stream that closes without a terminal
    /// `Done` event is a truncated turn, never a fake successful `EndTurn`.
    async fn stream_attempt(
        &self,
        request: ChatRequest,
        total: &mut Usage,
    ) -> Result<(ChatResponse, Usage), CoreError> {
        let mut stream = match self.provider.chat_stream(request, self.cx.cancel.clone()).await {
            Ok(s) => s,
            // A mid-flight cancellation (Esc) surfaces from the transport as
            // `ProviderError::Cancelled`; map it to `CoreError::Cancelled` so the
            // orchestrator never retries it and the loop suppresses the error notice
            // (rather than treating an interrupt as a layer failure).
            Err(stepper_provider::ProviderError::Cancelled) => return Err(CoreError::Cancelled),
            Err(e) => return Err(e.into()),
        };

        let mut events = Vec::new();
        let mut step_usage = Usage::default();
        let mut saw_terminal_done = false;

        while let Some(item) = stream.next().await {
            let event = match item {
                Ok(e) => e,
                Err(stepper_provider::ProviderError::Cancelled) => {
                    return Err(CoreError::Cancelled)
                }
                Err(e) => return Err(e.into()),
            };
            match &event {
                // In worker mode the live token stream is suppressed (N workers
                // would garble one buffer); the worker panel shows per-worker
                // progress via WorkerActivity instead.
                ChatEvent::TextDelta(t) if self.worker.is_none() => {
                    let _ = self.event_tx.send(AppEvent::AssistantTokenDelta(t.clone())).await;
                }
                ChatEvent::ThinkingDelta(t) if self.worker.is_none() => {
                    let _ = self.event_tx.send(AppEvent::ReasoningTokenDelta(t.clone())).await;
                }
                ChatEvent::Usage(u) => {
                    step_usage = *u;
                    self.emit_usage(total, &step_usage).await;
                }
                // Suppressed in worker mode: the shared notice line can't carry
                // N workers' refusals; a refused worker surfaces via its status.
                ChatEvent::Done(StopReason::Refusal) if self.worker.is_none() => {
                    let _ = self
                        .event_tx
                        .send(AppEvent::Notice {
                            level: stepper_protocol::NoticeLevel::Warn,
                            text: "model refused to respond".into(),
                        })
                        .await;
                }
                _ => {}
            }
            if matches!(event, ChatEvent::Done(_)) {
                saw_terminal_done = true;
            }
            events.push(event);
        }

        // A clean upstream close mid-turn (the most common transient failure)
        // must surface as an error — the provider-side `chat()` default already
        // guards this; mirror it on the streaming path.
        if !saw_terminal_done {
            return Err(stepper_provider::ProviderError::UnexpectedEnd.into());
        }

        // Accumulate across steps by SUM (each step is a separate request);
        // `merge`/max only folds partial frames within a single stream.
        total.add(&step_usage);
        Ok((ChatResponse::from_events(events), step_usage))
    }

    async fn emit_usage(&self, total: &Usage, step_usage: &Usage) {
        // `total` holds prior steps' summed usage; add the in-flight step for the
        // live cumulative figure shown in the footer.
        let mut cumulative = *total;
        cumulative.add(step_usage);
        // A worker reports its own running token count to its panel row, not the
        // shared footer gauge (which tracks the main/sequential thread).
        if let Some(index) = self.worker {
            let _ = self
                .event_tx
                .send(AppEvent::WorkerActivity {
                    index,
                    tokens: Some(cumulative.input + cumulative.output),
                    tool: None,
                })
                .await;
            return;
        }
        let context_used = step_usage.input + step_usage.output;
        let view = UsageView {
            tokens_in: cumulative.input,
            tokens_out: cumulative.output,
            cache_read: cumulative.cache_read,
            cache_write: cumulative.cache_write,
            context_used,
            context_limit: self.model_info.context_window,
            cost_usd: self.model_info.cost(&cumulative),
        };
        let _ = self.event_tx.send(AppEvent::UsageUpdated(view)).await;
    }

    /// Run one tool call, emitting start/finish events, and return the
    /// `tool_result` block to thread back to the model.
    async fn run_tool(&self, id: &str, name: &str, input: Value) -> ContentBlock {
        self.emit_tool_started(id, name, &input).await;

        // PreToolUse hook may block the call before it runs.
        if let HookDecision::Block(reason) = self
            .hooks
            .run(
                "PreToolUse",
                Some(name),
                &serde_json::json!({ "tool": name, "args": input }),
            )
            .await
        {
            self.emit_tool_finished(id, false).await;
            return ContentBlock::ToolResult {
                tool_call_id: id.to_string(),
                content: vec![stepper_provider::ToolContent::text(format!(
                    "blocked by hook: {reason}"
                ))],
                is_error: true,
            };
        }

        let outcome = match self.tools.get(name) {
            Some(tool) => tool.call(input, &self.cx).await,
            None => Err(stepper_provider::ToolError::NotFound(name.to_string())),
        };

        let (content, is_error) = match outcome {
            Ok(result) => {
                // The global todo panel is the main thread's; workers don't drive it.
                if name == "todo_write" && self.worker.is_none() {
                    self.emit_todos(&result).await;
                }
                (result.content, result.is_error)
            }
            Err(e) => (
                vec![stepper_provider::ToolContent::text(e.to_string())],
                true,
            ),
        };

        self.emit_tool_finished(id, !is_error).await;

        let _ = self
            .hooks
            .run(
                "PostToolUse",
                Some(name),
                &serde_json::json!({ "tool": name, "ok": !is_error }),
            )
            .await;

        ContentBlock::ToolResult {
            tool_call_id: id.to_string(),
            content,
            is_error,
        }
    }

    /// Tool-call start: a worker reports its latest tool to its panel row; the
    /// main thread emits the global `ToolCallStarted` chip.
    async fn emit_tool_started(&self, id: &str, name: &str, input: &Value) {
        match self.worker {
            Some(index) => {
                let _ = self
                    .event_tx
                    .send(AppEvent::WorkerActivity {
                        index,
                        tokens: None,
                        tool: Some(summarize(name, input)),
                    })
                    .await;
            }
            None => {
                let _ = self
                    .event_tx
                    .send(AppEvent::ToolCallStarted(ToolCallView {
                        id: id.to_string(),
                        name: name.to_string(),
                        summary: summarize(name, input),
                    }))
                    .await;
            }
        }
    }

    /// Tool-call finish: suppressed in worker mode (the worker's terminal status
    /// arrives once via `WorkerFinished`).
    async fn emit_tool_finished(&self, id: &str, ok: bool) {
        if self.worker.is_none() {
            let _ = self
                .event_tx
                .send(AppEvent::ToolCallFinished {
                    id: id.to_string(),
                    ok,
                })
                .await;
        }
    }

    async fn emit_todos(&self, result: &stepper_provider::ToolResult) {
        for block in &result.content {
            if let stepper_provider::ToolContent::Json { json } = block
                && let Some(items) = json.get("todos").and_then(Value::as_array)
            {
                let todos = items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| {
                        Some(TodoItemView {
                            id: i.to_string(),
                            content: t.get("content")?.as_str()?.to_string(),
                            status: parse_status(t.get("status").and_then(Value::as_str)),
                        })
                    })
                    .collect();
                let _ = self.event_tx.send(AppEvent::TodoUpdated(todos)).await;
            }
        }
    }
}

/// Max request-level retries per ReAct step (after the initial attempt).
const MAX_REQUEST_RETRIES: u32 = 3;

/// Exponential backoff with jitter: 500ms doubling per retry, plus up to +50%
/// derived from the clock (no rng dependency).
/// Whether an assistant turn that ended with no tool call still reads like an
/// unfulfilled intent (or is empty) — the trigger for the one-shot continue
/// nudge. The cue set is deliberately specific (imminent-action phrases, not
/// generic sign-offs like "let me know"), so a false positive — costing one
/// extra request — is rare; the single-nudge-per-turn bound caps it regardless.
fn looks_unfinished(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_lowercase();
    if lower.ends_with(':') {
        return true;
    }
    const CUES: &[&str] = &[
        "let me create", "let me write", "let me add", "let me implement",
        "let me update", "let me start", "let me build", "let me make", "let me fix",
        "i'll create", "i'll write", "i'll add", "i'll implement", "i'll update",
        "i'll start", "i'll make", "i'll fix", "now i'll", "next i'll", "let's create",
        "이제 ", "만들겠", "작성하겠", "구현하겠", "수정하겠", "진행하겠", "추가하겠", "고치겠",
    ];
    CUES.iter().any(|c| lower.contains(c))
}

fn retry_backoff(attempt: u32) -> std::time::Duration {
    let base_ms = 500u64 << attempt.saturating_sub(1).min(4);
    let jitter_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % (base_ms / 2 + 1))
        .unwrap_or(0);
    std::time::Duration::from_millis(base_ms + jitter_ms)
}

fn parse_status(s: Option<&str>) -> TodoStatus {
    match s {
        Some("in_progress") => TodoStatus::InProgress,
        Some("completed") => TodoStatus::Completed,
        _ => TodoStatus::Pending,
    }
}

/// A one-line summary of a tool call for the `ToolCallStarted` chip.
fn summarize(name: &str, input: &Value) -> String {
    let arg = input
        .get("path")
        .or_else(|| input.get("command"))
        .or_else(|| input.get("pattern"))
        .or_else(|| input.get("url"))
        .and_then(Value::as_str);
    match arg {
        Some(a) => format!("{name}: {a}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::looks_unfinished;

    #[test]
    fn looks_unfinished_flags_intent_and_empty_not_completions() {
        // Empty / colon-ended / imminent-action cues → nudge.
        assert!(looks_unfinished(""));
        assert!(looks_unfinished("   "));
        assert!(looks_unfinished("Here is the plan:"));
        assert!(looks_unfinished("Now let me create the file with the layout."));
        assert!(looks_unfinished("좋습니다. 이제 페이지를 만들겠습니다."));
        assert!(looks_unfinished("I'll write the component now."));
        // Genuine completions / generic sign-offs → no nudge.
        assert!(!looks_unfinished("Done — I added the route and the test passes."));
        assert!(!looks_unfinished("The page is built. Let me know if you want changes."));
        assert!(!looks_unfinished("작업을 완료했습니다."));
    }
}
