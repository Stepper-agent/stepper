use crate::event::{ChatEvent, StopReason};
use crate::usage::Usage;
use crate::wire::WireDelta;

#[derive(Debug, Default)]
struct ToolBuilder {
    id: Option<String>,
    name: Option<String>,
    args: String,
    started: bool,
    completed: bool,
}

/// Dialect-agnostic reassembly of a streaming turn. Feed it `WireDelta`s in
/// arrival order; it emits `ChatEvent`s and tracks per-index tool-call builders
/// so that fragmented arguments (OpenAI `function.arguments` substrings,
/// Anthropic `input_json_delta.partial_json`) accumulate into one parsed value.
///
/// This type carries the whole correctness burden of streaming normalization and
/// is therefore exercised exhaustively by fixture tests with no network.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    tools: Vec<ToolBuilder>,
    usage: Usage,
    stop: Option<StopReason>,
    most_recently_started: Option<usize>,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    fn builder(&mut self, index: usize) -> &mut ToolBuilder {
        if index >= self.tools.len() {
            self.tools.resize_with(index + 1, ToolBuilder::default);
        }
        &mut self.tools[index]
    }

    /// Resolve a `ToolCallStart` whose wire frame carried no index (oMLX/Ollama
    /// OpenAI-compat pattern): a fresh `id` opens a new call, anything else
    /// attaches to the most recently started one (or call 0 if none exists).
    fn resolve_start_index(&self, index: Option<usize>, id: Option<&String>) -> usize {
        if let Some(i) = index {
            return i;
        }
        if self.tools.is_empty() {
            return 0;
        }
        let recent = self
            .most_recently_started
            .filter(|i| *i < self.tools.len())
            .unwrap_or(self.tools.len() - 1);
        match (id, &self.tools[recent].id) {
            (Some(incoming), Some(existing)) if incoming != existing => self.tools.len(),
            _ => recent,
        }
    }

    /// Emit the `ToolCallStarted` event for `index` once both id and name are
    /// known and it has not already been announced.
    fn try_start(&mut self, index: usize, out: &mut Vec<ChatEvent>) {
        let b = &mut self.tools[index];
        if !b.started && let (Some(id), Some(name)) = (b.id.clone(), b.name.clone()) {
            b.started = true;
            out.push(ChatEvent::ToolCallStarted { index, id, name });
        }
    }

    fn complete(&mut self, index: usize, out: &mut Vec<ChatEvent>) {
        let b = &mut self.tools[index];
        if b.completed || !b.started {
            return;
        }
        b.completed = true;
        let id = b.id.clone().unwrap_or_default();
        let name = b.name.clone().unwrap_or_default();
        let input = parse_args(&b.args);
        out.push(ChatEvent::ToolCallCompleted {
            index,
            id,
            name,
            input,
        });
    }

    /// Process one wire delta, returning the `ChatEvent`s it produced.
    pub fn push(&mut self, delta: WireDelta) -> Vec<ChatEvent> {
        let mut out = Vec::new();
        match delta {
            WireDelta::Text(t) => {
                if !t.is_empty() {
                    out.push(ChatEvent::TextDelta(t));
                }
            }
            WireDelta::Thinking(t) => {
                if !t.is_empty() {
                    out.push(ChatEvent::ThinkingDelta(t));
                }
            }
            WireDelta::ThinkingSignature(s) => {
                if !s.is_empty() {
                    out.push(ChatEvent::ThinkingSignature(s));
                }
            }
            WireDelta::ToolCallStart { index, id, name } => {
                let index = self.resolve_start_index(index, id.as_ref());
                self.most_recently_started = Some(index);
                {
                    let b = self.builder(index);
                    // A fresh start on an already-completed index means the wire
                    // reused the slot for a new call — reset so its args don't
                    // append to the previous call's and it can be emitted again.
                    if b.completed {
                        *b = ToolBuilder::default();
                    }
                    if id.is_some() {
                        b.id = id;
                    }
                    if name.is_some() {
                        b.name = name;
                    }
                }
                self.try_start(index, &mut out);
            }
            WireDelta::ToolCallArgs { index, fragment } => {
                let index = index.or(self.most_recently_started).unwrap_or(0);
                self.builder(index).args.push_str(&fragment);
                self.try_start(index, &mut out);
                if self.tools[index].started {
                    out.push(ChatEvent::ToolCallArgsDelta { index, fragment });
                }
            }
            WireDelta::ToolCallEnd { index } => {
                if index < self.tools.len() {
                    self.try_start(index, &mut out);
                    self.complete(index, &mut out);
                }
            }
            WireDelta::Usage(u) => {
                self.usage.merge(&u);
                out.push(ChatEvent::Usage(self.usage));
            }
            WireDelta::Stop(reason) => {
                let indices: Vec<usize> = (0..self.tools.len()).collect();
                for i in indices {
                    self.complete(i, &mut out);
                }
                self.stop = Some(reason.clone());
                out.push(ChatEvent::Done(reason));
            }
        }
        out
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    pub fn stop_reason(&self) -> Option<&StopReason> {
        self.stop.as_ref()
    }
}

fn parse_args(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn collect(deltas: Vec<WireDelta>) -> Vec<ChatEvent> {
        let mut acc = StreamAccumulator::new();
        let mut events = Vec::new();
        for d in deltas {
            events.extend(acc.push(d));
        }
        events
    }

    #[test]
    fn text_deltas_pass_through_and_empty_is_dropped() {
        let evs = collect(vec![
            WireDelta::Text("Hel".into()),
            WireDelta::Text(String::new()),
            WireDelta::Text("lo".into()),
            WireDelta::Stop(StopReason::EndTurn),
        ]);
        assert_eq!(
            evs,
            vec![
                ChatEvent::TextDelta("Hel".into()),
                ChatEvent::TextDelta("lo".into()),
                ChatEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn openai_shaped_tool_call_fragments_accumulate_and_complete_at_stop() {
        // OpenAI: first frame carries index+id+name, later frames only args.
        let evs = collect(vec![
            WireDelta::ToolCallStart {
                index: Some(0),
                id: Some("call_1".into()),
                name: Some("edit_file".into()),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "{\"pa".into(),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "th\":\"a.rs\"}".into(),
            },
            WireDelta::Stop(StopReason::ToolUse),
        ]);
        assert_eq!(
            evs[0],
            ChatEvent::ToolCallStarted {
                index: 0,
                id: "call_1".into(),
                name: "edit_file".into()
            }
        );
        assert!(matches!(
            evs[1],
            ChatEvent::ToolCallArgsDelta { index: 0, .. }
        ));
        let completed = evs
            .iter()
            .find(|e| matches!(e, ChatEvent::ToolCallCompleted { .. }))
            .unwrap();
        match completed {
            ChatEvent::ToolCallCompleted { id, name, input, .. } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "edit_file");
                assert_eq!(input, &json!({"path": "a.rs"}));
            }
            _ => unreachable!(),
        }
        assert_eq!(evs.last(), Some(&ChatEvent::Done(StopReason::ToolUse)));
    }

    #[test]
    fn anthropic_shaped_tool_call_completes_at_block_stop() {
        // Anthropic: content_block_start carries id+name, input_json_delta
        // carries fragments, content_block_stop completes the call.
        let evs = collect(vec![
            WireDelta::ToolCallStart {
                index: Some(0),
                id: Some("toolu_1".into()),
                name: Some("get_weather".into()),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "{\"city\":".into(),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "\"seoul\"}".into(),
            },
            WireDelta::ToolCallEnd { index: 0 },
            WireDelta::Stop(StopReason::ToolUse),
        ]);
        let completed_before_stop = evs
            .iter()
            .position(|e| matches!(e, ChatEvent::ToolCallCompleted { .. }))
            .unwrap();
        let stop_pos = evs
            .iter()
            .position(|e| matches!(e, ChatEvent::Done(_)))
            .unwrap();
        assert!(
            completed_before_stop < stop_pos,
            "tool completes on block_stop, before Done"
        );
        match &evs[completed_before_stop] {
            ChatEvent::ToolCallCompleted { input, .. } => {
                assert_eq!(input, &json!({"city": "seoul"}));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn parallel_tool_calls_keyed_by_index_dont_cross_contaminate() {
        let evs = collect(vec![
            WireDelta::ToolCallStart {
                index: Some(0),
                id: Some("a".into()),
                name: Some("first".into()),
            },
            WireDelta::ToolCallStart {
                index: Some(1),
                id: Some("b".into()),
                name: Some("second".into()),
            },
            WireDelta::ToolCallArgs {
                index: Some(1),
                fragment: "{\"x\":1}".into(),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "{\"y\":2}".into(),
            },
            WireDelta::Stop(StopReason::ToolUse),
        ]);
        let completed: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ToolCallCompleted {
                    index, name, input, ..
                } => Some((*index, name.clone(), input.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 2);
        assert!(completed.contains(&(0, "first".into(), json!({"y": 2}))));
        assert!(completed.contains(&(1, "second".into(), json!({"x": 1}))));
    }

    #[test]
    fn split_usage_merges_to_final_figures() {
        let mut acc = StreamAccumulator::new();
        // Anthropic message_start: input + cache, no output yet.
        acc.push(WireDelta::Usage(Usage {
            input: 100,
            output: 0,
            cache_read: 20,
            cache_write: 0,
        }));
        // message_delta: final output.
        let last = acc.push(WireDelta::Usage(Usage {
            input: 0,
            output: 50,
            cache_read: 0,
            cache_write: 0,
        }));
        match last.last() {
            Some(ChatEvent::Usage(u)) => {
                assert_eq!(u.input, 100);
                assert_eq!(u.output, 50);
                assert_eq!(u.cache_read, 20);
            }
            _ => panic!("expected usage event"),
        }
    }

    #[test]
    fn reused_tool_index_after_completion_starts_a_fresh_call() {
        let evs = collect(vec![
            WireDelta::ToolCallStart {
                index: Some(0),
                id: Some("a".into()),
                name: Some("first".into()),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "{\"x\":1}".into(),
            },
            WireDelta::ToolCallEnd { index: 0 },
            WireDelta::ToolCallStart {
                index: Some(0),
                id: Some("b".into()),
                name: Some("second".into()),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "{\"y\":2}".into(),
            },
            WireDelta::ToolCallEnd { index: 0 },
            WireDelta::Stop(StopReason::ToolUse),
        ]);
        let completed: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                ChatEvent::ToolCallCompleted {
                    id, name, input, ..
                } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 2, "reused index yields two distinct calls");
        assert_eq!(completed[0], ("a".into(), "first".into(), json!({"x": 1})));
        assert_eq!(completed[1], ("b".into(), "second".into(), json!({"y": 2})));
    }

    #[test]
    fn malformed_args_fall_back_to_string_not_panic() {
        let evs = collect(vec![
            WireDelta::ToolCallStart {
                index: Some(0),
                id: Some("c".into()),
                name: Some("t".into()),
            },
            WireDelta::ToolCallArgs {
                index: Some(0),
                fragment: "not json".into(),
            },
            WireDelta::Stop(StopReason::ToolUse),
        ]);
        match evs.iter().find_map(|e| match e {
            ChatEvent::ToolCallCompleted { input, .. } => Some(input),
            _ => None,
        }) {
            Some(serde_json::Value::String(s)) => assert_eq!(s, "not json"),
            other => panic!("expected string fallback, got {other:?}"),
        }
    }
}
