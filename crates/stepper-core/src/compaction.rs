use stepper_provider::{ChatRequest, ContentBlock, LlmProvider, Message, Role, ToolContent};
use tokio_util::sync::CancellationToken;

/// Floor for the planning budget so a pathological `context_window -
/// max_output_tokens` (override smaller than the output cap) never collapses
/// compaction to "fold everything, always".
const MIN_PLANNING_BUDGET: u64 = 16_384;

/// Keeps a layer's context bounded. The system prompt is passed separately and
/// never touched (the pinned prefix); when the running token estimate crosses
/// the soft threshold, the oldest non-recent messages are replaced with a single
/// summary marker, preserving KV-cache for the recent tail.
#[derive(Debug, Clone, Copy)]
pub struct Compactor {
    pub context_limit: u64,
    pub soft_ratio: f64,
    pub keep_recent: usize,
}

impl Compactor {
    pub fn new(context_limit: u64) -> Self {
        Compactor {
            context_limit,
            soft_ratio: 0.70,
            keep_recent: 6,
        }
    }

    /// Plan against `context_window - max_output_tokens` so a request near the
    /// soft threshold still leaves room for the model's reply (floored at
    /// `MIN_PLANNING_BUDGET`; a zero window keeps compaction disabled).
    pub fn with_output_reserve(context_window: u64, max_output_tokens: u64) -> Self {
        if context_window == 0 {
            return Compactor::new(0);
        }
        let floor = MIN_PLANNING_BUDGET.min(context_window);
        Compactor::new(context_window.saturating_sub(max_output_tokens).max(floor))
    }

    /// Decide how many leading messages to fold: `None` below the soft threshold
    /// or when there is no safe cut point (never splits an assistant tool_use
    /// from its following Tool results). Returns the cut index — drop `0..cut`.
    pub fn plan(&self, messages: &[Message], used_tokens: u64) -> Option<usize> {
        if self.context_limit == 0 {
            return None;
        }
        let threshold = (self.context_limit as f64 * self.soft_ratio) as u64;
        if used_tokens < threshold || messages.len() <= self.keep_recent + 1 {
            return None;
        }
        let drop_count = messages.len() - self.keep_recent;
        // Never split an assistant tool-call from its following tool results:
        // back off to a boundary where the next kept message is not a Tool reply.
        let mut cut = drop_count;
        while cut > 0 && matches!(messages.get(cut).map(|m| m.role), Some(Role::Tool)) {
            cut -= 1;
        }
        if cut == 0 { None } else { Some(cut) }
    }

    /// Compact in place with the built-in heuristic summary. Returns the number
    /// of messages folded away, or `None` if nothing was done. (The model-summary
    /// path lives in the agent loop via `summarize_with_model`.)
    pub fn maybe_compact(&self, messages: &mut Vec<Message>, used_tokens: u64) -> Option<usize> {
        let cut = self.plan(messages, used_tokens)?;
        let dropped: Vec<Message> = messages.drain(0..cut).collect();
        messages.insert(0, marker(&heuristic_summary(&dropped)));
        Some(cut)
    }
}

/// chars/4 token estimate over every block kind (text, thinking, tool inputs
/// and results), so tool-heavy histories — usually the bulk — are counted.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(message_chars).sum();
    (chars / 4) as u64
}

fn message_chars(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.len(),
            // An image's char-size proxy: the base64 payload length (compaction
            // only needs a rough magnitude, not real vision-token accounting).
            ContentBlock::Image { data, .. } => data.len(),
            ContentBlock::Thinking { text, .. } => text.len(),
            ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => tool_result_chars(content),
        })
        .sum()
}

/// Char count of a tool result's content blocks (text + JSON payloads). Shared
/// with the agent loop's per-turn tool-output budget.
pub(crate) fn tool_result_chars(content: &[ToolContent]) -> usize {
    content
        .iter()
        .map(|c| match c {
            ToolContent::Text { text } => text.len(),
            ToolContent::Json { json } => json.to_string().len(),
        })
        .sum()
}

/// The User message that replaces the folded-away history.
pub(crate) fn marker(summary: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text(format!(
            "[Earlier conversation compacted to save context. {summary}]"
        ))],
    }
}

/// Summarize folded-away history with a (typically cheap) model. `instructions`
/// (from `/compact [instructions]`) steers the summary's focus. Falls back to
/// `None` on any provider error or an empty response so the caller can use the
/// heuristic instead.
pub async fn summarize_with_model(
    provider: &dyn LlmProvider,
    dropped: &[Message],
    instructions: Option<&str>,
) -> Option<String> {
    let mut system = String::from(
        "Summarize the earlier conversation excerpt below concisely. Preserve key \
         decisions, file paths, and any open tasks. Output only the summary.",
    );
    if let Some(focus) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str(&format!(" Focus on: {focus}"));
    }
    let request = ChatRequest::new(provider.model())
        .with_system(system)
        .with_messages(vec![Message::user(render(dropped))]);
    let response = provider.chat(request, CancellationToken::new()).await.ok()?;
    let text = response.text().trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Render dropped messages into a transcript for the summarizer. Tool calls and
/// their results are the actual work of a coding session, so they are kept
/// (briefly) rather than dropped — `Message::text()` alone would summarize a
/// tool-heavy turn as a blank line. Reasoning/image blocks are omitted.
fn render(dropped: &[Message]) -> String {
    let mut out = String::new();
    for m in dropped {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
            Role::System => "System",
        };
        for block in &m.content {
            let line = match block {
                ContentBlock::Text(t) => t.trim().to_string(),
                ContentBlock::ToolUse { name, input, .. } => {
                    format!("[called {name} {}]", brief(&input.to_string()))
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    let text = content
                        .iter()
                        .map(|c| match c {
                            ToolContent::Text { text } => text.clone(),
                            ToolContent::Json { json } => json.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("[tool{} → {}]", if *is_error { " error" } else { "" }, brief(&text))
                }
                ContentBlock::Thinking { .. } | ContentBlock::Image { .. } => continue,
            };
            if line.is_empty() {
                continue;
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Trim a value to a short, char-boundary-safe preview for the summary transcript.
fn brief(s: &str) -> String {
    const MAX: usize = 200;
    let s = s.trim();
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub(crate) fn heuristic_summary(dropped: &[Message]) -> String {
    let users = dropped.iter().filter(|m| m.role == Role::User).count();
    let tools = dropped.iter().filter(|m| m.role == Role::Tool).count();
    let first = dropped
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| truncate(&m.text(), 200))
        .unwrap_or_default();
    format!("{users} user turn(s), {tools} tool round(s). Opening request: {first}")
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim().replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convo(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("user {i}"))
                } else {
                    Message::assistant(format!("assistant {i}"))
                }
            })
            .collect()
    }

    #[test]
    fn output_reserve_shrinks_the_planning_budget_with_a_floor() {
        assert_eq!(Compactor::with_output_reserve(200_000, 64_000).context_limit, 136_000);
        assert_eq!(
            Compactor::with_output_reserve(20_000, 18_000).context_limit,
            MIN_PLANNING_BUDGET,
            "a cap that eats the window floors at the minimum budget"
        );
        assert_eq!(
            Compactor::with_output_reserve(10_000, 64_000).context_limit,
            10_000,
            "the floor never exceeds the window itself"
        );
        assert_eq!(
            Compactor::with_output_reserve(0, 64_000).context_limit,
            0,
            "a zero window keeps compaction disabled"
        );
        assert_eq!(Compactor::with_output_reserve(200_000, 0).context_limit, 200_000);
    }

    #[test]
    fn estimate_counts_text_thinking_and_tool_payloads() {
        let messages = vec![
            Message::user("x".repeat(400)),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        text: "y".repeat(40),
                        signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({ "command": "ls" }),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "c1".into(),
                    content: vec![ToolContent::text("ok")],
                    is_error: false,
                }],
            },
        ];
        // 400 text + 40 thinking + (4 name + 16 serialized input) + 2 result = 462
        assert_eq!(estimate_tokens(&messages), 462 / 4);
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn no_compaction_below_threshold() {
        let c = Compactor::new(1000);
        let mut m = convo(20);
        assert_eq!(c.maybe_compact(&mut m, 100), None);
        assert_eq!(m.len(), 20);
    }

    #[test]
    fn compacts_and_keeps_recent_tail() {
        let c = Compactor::new(1000);
        let mut m = convo(20);
        let folded = c.maybe_compact(&mut m, 900).unwrap();
        assert!(folded > 0);
        // one summary marker + keep_recent tail
        assert_eq!(m.len(), 1 + c.keep_recent);
        assert!(m[0].text().contains("compacted"));
        assert!(m.last().unwrap().text().contains("assistant 19"));
    }

    #[test]
    fn zero_context_limit_never_compacts() {
        let c = Compactor::new(0);
        let mut m = convo(50);
        assert_eq!(c.maybe_compact(&mut m, u64::MAX), None);
        assert_eq!(m.len(), 50);
    }

    #[test]
    fn no_compaction_when_history_fits_keep_recent() {
        let c = Compactor::new(1000);
        let mut m = convo(c.keep_recent + 1);
        assert_eq!(c.maybe_compact(&mut m, 999), None);
        assert_eq!(m.len(), c.keep_recent + 1);
    }

    #[test]
    fn triggers_just_above_soft_threshold() {
        let c = Compactor::new(1000);
        let threshold = (1000.0 * c.soft_ratio) as u64;
        let mut below = convo(20);
        assert_eq!(c.maybe_compact(&mut below, threshold - 1), None);
        let mut at = convo(20);
        assert!(c.maybe_compact(&mut at, threshold).is_some());
    }

    fn tool_round(id: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.into(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": "ls" }),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: id.into(),
                    content: vec![stepper_provider::ToolContent::text("ok")],
                    is_error: false,
                }],
            },
        ]
    }

    #[test]
    fn never_splits_tool_use_from_its_result() {
        let c = Compactor::new(1000);
        let mut m = vec![Message::user("opening request")];
        for i in 0..10 {
            m.extend(tool_round(&format!("call_{i}")));
        }
        let before = m.len();
        c.maybe_compact(&mut m, 999).expect("should compact");

        let kept_tail = &m[1..];
        assert!(
            !matches!(kept_tail.first().map(|msg| msg.role), Some(Role::Tool)),
            "the first kept message after the summary must not be an orphaned Tool result"
        );
        for window in kept_tail.windows(2) {
            if window[1].role == Role::Tool {
                assert_eq!(
                    window[0].role,
                    Role::Assistant,
                    "every Tool result must be preceded by its Assistant tool_use"
                );
            }
        }
        assert!(m.len() < before);
    }

    fn tool_result_msg(id: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: id.into(),
                content: vec![stepper_provider::ToolContent::text("ok")],
                is_error: false,
            }],
        }
    }

    #[test]
    fn backs_off_to_none_when_no_safe_cut_point_exists() {
        let c = Compactor::new(1000);
        let mut m = vec![Message::user("opening request")];
        let unbroken_tools = c.keep_recent + 8;
        for i in 0..unbroken_tools {
            m.push(tool_result_msg(&format!("t{i}")));
        }
        let before_len = m.len();
        let drop_count = before_len - c.keep_recent;
        assert!(drop_count > 0, "precondition: there is something to drop");
        for msg in m.iter().take(drop_count + 1).skip(1) {
            assert_eq!(
                msg.role,
                Role::Tool,
                "precondition: every message in the drop window is a Tool result, so back-off must walk cut to 0"
            );
        }
        assert_eq!(
            c.maybe_compact(&mut m, 999),
            None,
            "an unbroken Tool chain has no safe cut point; maybe_compact must back off to None"
        );
        assert_eq!(
            m.len(),
            before_len,
            "nothing is folded when the cut walks all the way to 0"
        );
        assert!(
            !m[0].text().contains("compacted"),
            "no summary marker is inserted on the None path"
        );
    }

    #[test]
    fn backs_off_when_drop_boundary_lands_on_a_tool_result() {
        let c = Compactor::new(1000);
        let mut m = Vec::new();
        for i in 0..10 {
            m.extend(tool_round(&format!("a{i}")));
        }
        let final_len = m.len() + 1;
        let drop_count = final_len - c.keep_recent;
        m.insert(drop_count, tool_result_msg("boundary"));
        assert_eq!(m.len(), final_len);
        assert_eq!(
            m[drop_count].role,
            Role::Tool,
            "precondition: the drop boundary lands on a Tool result"
        );
        let folded = c.maybe_compact(&mut m, 999).expect("should compact");
        assert!(
            folded < drop_count,
            "back-off cut short of the boundary that would orphan the Tool result"
        );
        assert_eq!(
            m[1].role,
            Role::Assistant,
            "the message right after the summary is the Assistant tool_use, not its result"
        );
    }

    #[test]
    fn render_keeps_tool_calls_and_results_not_just_text() {
        let dropped = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({ "path": "a.rs" }),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "1".into(),
                    content: vec![ToolContent::Text { text: "wrote a.rs".into() }],
                    is_error: false,
                }],
            },
        ];
        let rendered = render(&dropped);
        assert!(rendered.contains("write_file"), "tool call is kept: {rendered}");
        assert!(rendered.contains("wrote a.rs"), "tool result is kept: {rendered}");
    }

    #[test]
    fn brief_truncates_a_long_value_on_a_char_boundary() {
        // A tool result longer than the cap, with a multibyte char straddling the
        // 200-byte mark, must be truncated safely (no panic) and marked with `…`.
        let long = "é".repeat(500); // 1000 bytes, 2-byte chars across byte 200
        let dropped = vec![Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "1".into(),
                content: vec![ToolContent::Text { text: long.clone() }],
                is_error: false,
            }],
        }];
        let rendered = render(&dropped);
        assert!(rendered.contains('…'), "long value is truncated with a marker");
        assert!(rendered.len() < long.len(), "the rendered transcript is shorter than the raw value");
    }
}
