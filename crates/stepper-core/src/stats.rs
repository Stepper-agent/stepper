use crate::session::{SessionRecord, TurnRecord};
use serde::Serialize;
use std::collections::BTreeMap;
use stepper_provider::{ContentBlock, Usage};

const SECS_PER_DAY: u64 = 86_400;

/// Per-model rollup within a [`SessionStats`].
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStat {
    pub turns: u64,
    pub usage: Usage,
    pub cost_usd: f64,
}

/// Cross-session aggregate over persisted sessions. Tokens/cost come from each
/// turn's recorded `usage`/`cost_usd`; turns saved before stats existed contribute
/// zeros (and, lacking a timestamp, are excluded once a `--days` window is set).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub total_sessions: u64,
    pub total_turns: u64,
    pub total_cost_usd: f64,
    pub total_usage: Usage,
    /// Per-turn primary model → rollup. Layered turns attribute to their primary
    /// model (stepper sums layer usage into one per-turn figure).
    pub per_model: BTreeMap<String, ModelStat>,
    /// Tool name → call count, walked from each turn's transcript.
    pub per_tool: BTreeMap<String, u64>,
    /// Earliest / latest turn timestamp seen (unix secs), when any turn had one.
    pub earliest: Option<u64>,
    pub latest: Option<u64>,
    /// The `--days` window applied, if any.
    pub days: Option<u64>,
    pub cost_per_day: f64,
    pub tokens_per_session: f64,
    pub median_tokens_per_session: u64,
}

/// Total tokens across all four usage fields.
pub fn total_tokens(u: &Usage) -> u64 {
    u.input
        .saturating_add(u.output)
        .saturating_add(u.cache_read)
        .saturating_add(u.cache_write)
}

/// Aggregate persisted sessions into a [`SessionStats`]. `days` limits to turns
/// whose `ended_at` is within the last N days of `now_secs` (turns without a
/// timestamp are excluded when a window is set, included otherwise). `now_secs`
/// is passed in so the function stays pure/testable (no clock access).
pub fn aggregate_stats(records: &[SessionRecord], days: Option<u64>, now_secs: u64) -> SessionStats {
    let cutoff = days.map(|d| now_secs.saturating_sub(d.saturating_mul(SECS_PER_DAY)));
    let mut stats = SessionStats { days, ..Default::default() };
    let mut per_session_tokens: Vec<u64> = Vec::new();

    for record in records {
        let turns: Vec<&TurnRecord> = record
            .turns
            .iter()
            .filter(|t| match cutoff {
                Some(c) => t.ended_at.is_some_and(|ts| ts >= c),
                None => true,
            })
            .collect();
        if turns.is_empty() {
            continue;
        }
        stats.total_sessions += 1;
        let mut session_tokens = 0u64;
        for t in turns {
            stats.total_turns += 1;
            stats.total_usage.add(&t.usage);
            stats.total_cost_usd += t.cost_usd;
            session_tokens = session_tokens.saturating_add(total_tokens(&t.usage));
            if let Some(ts) = t.ended_at {
                stats.earliest = Some(stats.earliest.map_or(ts, |e| e.min(ts)));
                stats.latest = Some(stats.latest.map_or(ts, |l| l.max(ts)));
            }
            let model = if t.model_ref.is_empty() {
                "unknown".to_string()
            } else {
                t.model_ref.clone()
            };
            let m = stats.per_model.entry(model).or_default();
            m.turns += 1;
            m.usage.add(&t.usage);
            m.cost_usd += t.cost_usd;
            for msg in &t.messages {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { name, .. } = block {
                        *stats.per_tool.entry(name.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
        per_session_tokens.push(session_tokens);
    }

    let span_days = match (stats.earliest, stats.latest) {
        (Some(e), Some(l)) if l > e => ((l - e) as f64 / SECS_PER_DAY as f64).max(1.0),
        _ => 1.0,
    };
    stats.cost_per_day = stats.total_cost_usd / span_days;
    stats.tokens_per_session = if stats.total_sessions > 0 {
        total_tokens(&stats.total_usage) as f64 / stats.total_sessions as f64
    } else {
        0.0
    };
    stats.median_tokens_per_session = median(&mut per_session_tokens);
    stats
}

fn median(values: &mut [u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let n = values.len();
    if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) / 2
    } else {
        values[n / 2]
    }
}

/// `1234567 → "1.2M"`, `2345 → "2.3K"`, small values unchanged.
fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

impl SessionStats {
    /// A human-readable summary for the terminal (the default `stepper stats`
    /// output). `models`/`tools` toggle the optional breakdown sections.
    pub fn render_text(&self, models: bool, tools: Option<usize>) -> String {
        let mut out = String::new();
        if self.total_sessions == 0 {
            out.push_str("No sessions with recorded usage yet.\n");
            if self.days.is_some() {
                out.push_str("(A `--days` window excludes turns saved before stats tracking.)\n");
            }
            return out;
        }
        out.push_str("stepper usage stats\n");
        if let Some(d) = self.days {
            out.push_str(&format!("  window:           last {d} day(s)\n"));
        }
        out.push_str(&format!("  sessions:         {}\n", self.total_sessions));
        out.push_str(&format!("  turns:            {}\n", self.total_turns));
        out.push_str(&format!("  total cost:       ${:.4}\n", self.total_cost_usd));
        out.push_str(&format!("  cost / day:       ${:.4}\n", self.cost_per_day));
        out.push_str(&format!(
            "  tokens:           {} in · {} out · {} cache-read · {} cache-write\n",
            fmt_count(self.total_usage.input),
            fmt_count(self.total_usage.output),
            fmt_count(self.total_usage.cache_read),
            fmt_count(self.total_usage.cache_write),
        ));
        out.push_str(&format!("  tokens / session: {:.0}\n", self.tokens_per_session));
        out.push_str(&format!("  median / session: {}\n", fmt_count(self.median_tokens_per_session)));

        if models && !self.per_model.is_empty() {
            out.push_str("\nby model (turns · tokens · cost)\n");
            let mut rows: Vec<(&String, &ModelStat)> = self.per_model.iter().collect();
            rows.sort_by_key(|r| std::cmp::Reverse(total_tokens(&r.1.usage)));
            for (model, m) in rows {
                out.push_str(&format!(
                    "  {model}: {} · {} · ${:.4}\n",
                    m.turns,
                    fmt_count(total_tokens(&m.usage)),
                    m.cost_usd,
                ));
            }
        }
        if let Some(limit) = tools
            && !self.per_tool.is_empty()
        {
            out.push_str("\nby tool (calls)\n");
            let mut rows: Vec<(&String, &u64)> = self.per_tool.iter().collect();
            rows.sort_by_key(|r| std::cmp::Reverse(*r.1));
            for (tool, count) in rows.into_iter().take(limit.max(1)) {
                out.push_str(&format!("  {tool}: {count}\n"));
            }
        }
        out
    }

    /// Per-model CSV (one row per model + a TOTAL row). Hand-formatted — the model
    /// refs and tool names here never contain commas or quotes, so no escaping is
    /// needed and a csv dependency would be overkill.
    pub fn to_csv(&self) -> String {
        let mut out = String::from("model,turns,input,output,cache_read,cache_write,cost_usd\n");
        for (model, m) in &self.per_model {
            out.push_str(&format!(
                "{model},{},{},{},{},{},{:.6}\n",
                m.turns, m.usage.input, m.usage.output, m.usage.cache_read, m.usage.cache_write, m.cost_usd,
            ));
        }
        out.push_str(&format!(
            "TOTAL,{},{},{},{},{},{:.6}\n",
            self.total_turns,
            self.total_usage.input,
            self.total_usage.output,
            self.total_usage.cache_read,
            self.total_usage.cache_write,
            self.total_cost_usd,
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRecord;
    use stepper_provider::{ContentBlock, Message, Role};

    fn turn(model: &str, input: u64, output: u64, cost: f64, ended_at: Option<u64>) -> TurnRecord {
        TurnRecord {
            user: "do a thing".into(),
            model_ref: model.into(),
            usage: Usage { input, output, ..Default::default() },
            cost_usd: cost,
            ended_at,
            ..Default::default()
        }
    }

    fn session(turns: Vec<TurnRecord>) -> SessionRecord {
        SessionRecord { id: "s".into(), name: None, turns }
    }

    #[test]
    fn empty_input_aggregates_to_zero() {
        let s = aggregate_stats(&[], None, 1_000_000);
        assert_eq!(s.total_sessions, 0);
        assert_eq!(s.total_turns, 0);
        assert_eq!(s.total_cost_usd, 0.0);
        assert_eq!(s.median_tokens_per_session, 0);
        assert!(s.render_text(true, Some(5)).contains("No sessions"));
    }

    #[test]
    fn sums_usage_cost_and_groups_by_model() {
        let records = vec![
            session(vec![
                turn("anthropic/claude", 100, 50, 0.10, Some(10)),
                turn("openai/gpt", 200, 80, 0.20, Some(20)),
            ]),
            session(vec![turn("anthropic/claude", 300, 100, 0.30, Some(30))]),
        ];
        let s = aggregate_stats(&records, None, 1_000_000);
        assert_eq!(s.total_sessions, 2);
        assert_eq!(s.total_turns, 3);
        assert_eq!(s.total_usage.input, 600);
        assert_eq!(s.total_usage.output, 230);
        assert!((s.total_cost_usd - 0.60).abs() < 1e-9);
        // Per-model grouping.
        let claude = &s.per_model["anthropic/claude"];
        assert_eq!(claude.turns, 2);
        assert_eq!(claude.usage.input, 400);
        assert!((claude.cost_usd - 0.40).abs() < 1e-9);
        assert_eq!(s.per_model["openai/gpt"].turns, 1);
    }

    #[test]
    fn days_window_excludes_old_and_timestampless_turns() {
        let now = 100 * SECS_PER_DAY;
        let records = vec![session(vec![
            turn("m", 10, 10, 0.0, Some(now - SECS_PER_DAY)), // 1 day ago — kept
            turn("m", 20, 20, 0.0, Some(now - 30 * SECS_PER_DAY)), // 30 days ago — dropped
            turn("m", 30, 30, 0.0, None),                     // no timestamp — dropped under a window
        ])];
        let windowed = aggregate_stats(&records, Some(7), now);
        assert_eq!(windowed.total_turns, 1, "only the recent timestamped turn");
        assert_eq!(windowed.total_usage.input, 10);
        // No window keeps everything (timestampless included).
        let all = aggregate_stats(&records, None, now);
        assert_eq!(all.total_turns, 3);
    }

    #[test]
    fn median_handles_even_and_odd_counts() {
        // Odd: 3 sessions, 1 turn each with distinct token totals → middle value.
        let odd = vec![
            session(vec![turn("m", 10, 0, 0.0, None)]),
            session(vec![turn("m", 30, 0, 0.0, None)]),
            session(vec![turn("m", 20, 0, 0.0, None)]),
        ];
        assert_eq!(aggregate_stats(&odd, None, 0).median_tokens_per_session, 20);
        // Even: 2 sessions → average of the two.
        let even = vec![
            session(vec![turn("m", 10, 0, 0.0, None)]),
            session(vec![turn("m", 30, 0, 0.0, None)]),
        ];
        assert_eq!(aggregate_stats(&even, None, 0).median_tokens_per_session, 20);
    }

    #[test]
    fn counts_tool_calls_from_the_transcript() {
        let mut t = turn("m", 0, 0, 0.0, None);
        t.messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse { id: "1".into(), name: "bash".into(), input: serde_json::json!({}) },
                ContentBlock::ToolUse { id: "2".into(), name: "bash".into(), input: serde_json::json!({}) },
                ContentBlock::ToolUse { id: "3".into(), name: "read_file".into(), input: serde_json::json!({}) },
            ],
        }];
        let s = aggregate_stats(&[session(vec![t])], None, 0);
        assert_eq!(s.per_tool["bash"], 2);
        assert_eq!(s.per_tool["read_file"], 1);
        let text = s.render_text(false, Some(10));
        assert!(text.contains("bash: 2"), "tool section: {text}");
    }

    #[test]
    fn csv_has_a_row_per_model_and_a_total() {
        let records = vec![session(vec![
            turn("a/x", 100, 50, 0.10, None),
            turn("b/y", 200, 80, 0.20, None),
        ])];
        let csv = aggregate_stats(&records, None, 0).to_csv();
        assert!(csv.starts_with("model,turns,input,output,cache_read,cache_write,cost_usd\n"));
        assert!(csv.contains("a/x,1,100,50,0,0,"), "csv: {csv}");
        assert!(csv.contains("TOTAL,2,300,130,0,0,"), "csv total: {csv}");
    }
}
