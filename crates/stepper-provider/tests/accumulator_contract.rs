//! Public-API contracts for `StreamAccumulator` reassembly that the in-crate
//! unit tests in `src/accumulator.rs` do not already cover: many parallel calls,
//! empty/absent arguments, malformed fragments accumulated across frames,
//! interleaved Text/Thinking deltas, a Done-only stream, and multi-frame usage
//! merging surfaced on every emitted `Usage` event.

use serde_json::json;
use stepper_provider::{ChatEvent, StopReason, StreamAccumulator, Usage, WireDelta};

fn collect(deltas: Vec<WireDelta>) -> Vec<ChatEvent> {
    let mut acc = StreamAccumulator::new();
    let mut events = Vec::new();
    for d in deltas {
        events.extend(acc.push(d));
    }
    events
}

fn completed(events: &[ChatEvent]) -> Vec<(usize, String, String, serde_json::Value)> {
    events
        .iter()
        .filter_map(|e| match e {
            ChatEvent::ToolCallCompleted {
                index,
                id,
                name,
                input,
            } => Some((*index, id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn three_parallel_tool_calls_each_keep_their_own_arguments() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("a".into()),
            name: Some("alpha".into()),
        },
        WireDelta::ToolCallStart {
            index: Some(1),
            id: Some("b".into()),
            name: Some("beta".into()),
        },
        WireDelta::ToolCallStart {
            index: Some(2),
            id: Some("c".into()),
            name: Some("gamma".into()),
        },
        WireDelta::ToolCallArgs {
            index: Some(2),
            fragment: "{\"z\":3}".into(),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"x\":1}".into(),
        },
        WireDelta::ToolCallArgs {
            index: Some(1),
            fragment: "{\"y\":2}".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let mut done = completed(&evs);
    done.sort_by_key(|(i, ..)| *i);
    assert_eq!(
        done,
        vec![
            (0, "a".into(), "alpha".into(), json!({"x": 1})),
            (1, "b".into(), "beta".into(), json!({"y": 2})),
            (2, "c".into(), "gamma".into(), json!({"z": 3})),
        ]
    );
}

#[test]
fn tool_call_with_no_argument_frames_completes_as_empty_object() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("call_empty".into()),
            name: Some("now".into()),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let done = completed(&evs);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].3, json!({}));
}

#[test]
fn tool_call_with_blank_whitespace_arguments_completes_as_empty_object() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("call_ws".into()),
            name: Some("noop".into()),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "   ".into(),
        },
        WireDelta::ToolCallEnd { index: 0 },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let done = completed(&evs);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].3, json!({}));
}

#[test]
fn malformed_arguments_assembled_from_fragments_fall_back_to_raw_string() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("call_bad".into()),
            name: Some("broken".into()),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"path\": ".into(),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "<<not json>>".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let done = completed(&evs);
    assert_eq!(done.len(), 1);
    assert_eq!(
        done[0].3,
        serde_json::Value::String("{\"path\": <<not json>>".into())
    );
}

#[test]
fn interleaved_text_and_thinking_deltas_preserve_arrival_order() {
    let evs = collect(vec![
        WireDelta::Thinking("plan".into()),
        WireDelta::Text("ans".into()),
        WireDelta::Thinking("more".into()),
        WireDelta::Text("wer".into()),
        WireDelta::Stop(StopReason::EndTurn),
    ]);
    assert_eq!(
        evs,
        vec![
            ChatEvent::ThinkingDelta("plan".into()),
            ChatEvent::TextDelta("ans".into()),
            ChatEvent::ThinkingDelta("more".into()),
            ChatEvent::TextDelta("wer".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ]
    );
}

#[test]
fn empty_thinking_delta_is_dropped_but_nonempty_is_kept() {
    let evs = collect(vec![
        WireDelta::Thinking(String::new()),
        WireDelta::Thinking("hmm".into()),
        WireDelta::Stop(StopReason::EndTurn),
    ]);
    assert_eq!(
        evs,
        vec![
            ChatEvent::ThinkingDelta("hmm".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ]
    );
}

#[test]
fn done_only_stream_emits_just_the_done_event() {
    let evs = collect(vec![WireDelta::Stop(StopReason::EndTurn)]);
    assert_eq!(evs, vec![ChatEvent::Done(StopReason::EndTurn)]);
}

#[test]
fn fully_empty_stream_emits_nothing() {
    let evs = collect(vec![]);
    assert!(evs.is_empty());
}

#[test]
fn multiple_usage_frames_merge_and_each_emitted_event_carries_running_total() {
    let mut acc = StreamAccumulator::new();
    let first = acc.push(WireDelta::Usage(Usage {
        input: 100,
        output: 10,
        cache_read: 5,
        cache_write: 2,
    }));
    let second = acc.push(WireDelta::Usage(Usage {
        input: 100,
        output: 40,
        cache_read: 5,
        cache_write: 2,
    }));
    let third = acc.push(WireDelta::Usage(Usage {
        input: 100,
        output: 70,
        cache_read: 5,
        cache_write: 2,
    }));
    assert_eq!(first, vec![ChatEvent::Usage(Usage { input: 100, output: 10, cache_read: 5, cache_write: 2 })]);
    assert_eq!(second, vec![ChatEvent::Usage(Usage { input: 100, output: 40, cache_read: 5, cache_write: 2 })]);
    assert_eq!(third, vec![ChatEvent::Usage(Usage { input: 100, output: 70, cache_read: 5, cache_write: 2 })]);
    assert_eq!(acc.usage(), Usage { input: 100, output: 70, cache_read: 5, cache_write: 2 });
}

#[test]
fn args_before_start_metadata_are_buffered_then_announced_when_id_and_name_arrive() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: None,
            name: None,
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"k\":1}".into(),
        },
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("late".into()),
            name: Some("deferred".into()),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let started: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e, ChatEvent::ToolCallStarted { .. }))
        .collect();
    assert_eq!(started.len(), 1, "exactly one start announced once id+name known");
    let done = completed(&evs);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].1, "late");
    assert_eq!(done[0].2, "deferred");
    assert_eq!(done[0].3, json!({"k": 1}));
}

#[test]
fn args_for_unstarted_tool_emit_no_args_delta_until_started() {
    let mut acc = StreamAccumulator::new();
    let from_args = acc.push(WireDelta::ToolCallArgs {
        index: Some(0),
        fragment: "{\"k\":1}".into(),
    });
    assert!(
        from_args.is_empty(),
        "no ToolCallArgsDelta before the call is started"
    );
}

#[test]
fn stop_reason_and_usage_are_queryable_after_consuming_the_stream() {
    let mut acc = StreamAccumulator::new();
    acc.push(WireDelta::Usage(Usage {
        input: 7,
        output: 0,
        cache_read: 0,
        cache_write: 0,
    }));
    acc.push(WireDelta::Stop(StopReason::MaxTokens));
    assert_eq!(acc.stop_reason(), Some(&StopReason::MaxTokens));
    assert_eq!(acc.usage().input, 7);
}
