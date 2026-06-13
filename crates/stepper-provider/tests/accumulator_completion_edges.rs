//! Branch-targeted reassembly edges in `StreamAccumulator` plus a full-pipeline
//! composition through `ChatResponse::from_events`:
//! - an unstarted tool (args-only, no id/name) is silently dropped at `Stop`
//!   because `complete()` early-returns on `!started` (pins intended behavior);
//! - a stray `ToolCallEnd` for an out-of-range index hits the
//!   `if index < self.tools.len()` guard with no panic and no spurious event;
//! - whitespace-only args with NO `ToolCallEnd` (OpenAI shape) complete as `{}`
//!   via the `Stop`-driven path rather than the End-driven path;
//! - a mixed stream (usage + parallel tool calls + text + done) folds through
//!   the accumulator and `from_events` into one coherent response.

use serde_json::json;
use stepper_provider::{
    ChatEvent, ChatResponse, ContentBlock, StopReason, StreamAccumulator, Usage, WireDelta,
};

fn collect(deltas: Vec<WireDelta>) -> Vec<ChatEvent> {
    let mut acc = StreamAccumulator::new();
    let mut events = Vec::new();
    for d in deltas {
        events.extend(acc.push(d));
    }
    events
}

#[test]
fn args_only_tool_that_never_receives_id_or_name_is_dropped_at_stop() {
    let evs = collect(vec![
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"k\":1}".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, ChatEvent::ToolCallStarted { .. })),
        "an unstarted tool is never announced"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, ChatEvent::ToolCallCompleted { .. })),
        "Stop drops the unstarted tool: complete() early-returns on !started"
    );
    assert_eq!(
        evs,
        vec![ChatEvent::Done(StopReason::ToolUse)],
        "only the Done event survives"
    );
}

#[test]
fn args_only_tool_is_still_dropped_even_when_it_received_an_explicit_end() {
    let evs = collect(vec![
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"k\":1}".into(),
        },
        WireDelta::ToolCallEnd { index: 0 },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, ChatEvent::ToolCallCompleted { .. })),
        "End on an unstarted tool also early-returns in complete(); the call is dropped"
    );
    assert_eq!(evs, vec![ChatEvent::Done(StopReason::ToolUse)]);
}

#[test]
fn stray_tool_call_end_for_out_of_range_index_is_ignored_without_panic() {
    let evs = collect(vec![
        WireDelta::ToolCallEnd { index: 5 },
        WireDelta::Stop(StopReason::EndTurn),
    ]);
    assert_eq!(
        evs,
        vec![ChatEvent::Done(StopReason::EndTurn)],
        "the guard 'if index < self.tools.len()' drops the stray End frame"
    );
}

#[test]
fn tool_call_end_for_index_past_the_started_tools_does_not_complete_a_neighbor() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("a".into()),
            name: Some("alpha".into()),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"x\":1}".into(),
        },
        WireDelta::ToolCallEnd { index: 3 },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let completed: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            ChatEvent::ToolCallCompleted { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        vec![0],
        "the in-range tool completes at Stop; the out-of-range End touched nothing"
    );
    let stray_end_emitted_nothing = evs.iter().filter(|e| {
        matches!(
            e,
            ChatEvent::ToolCallCompleted { index: 3, .. }
                | ChatEvent::ToolCallStarted { index: 3, .. }
        )
    });
    assert_eq!(stray_end_emitted_nothing.count(), 0);
}

#[test]
fn whitespace_args_without_an_end_frame_complete_as_empty_object_at_stop() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("call_ws".into()),
            name: Some("noop".into()),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "  ".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let completed_pos = evs
        .iter()
        .position(|e| matches!(e, ChatEvent::ToolCallCompleted { .. }))
        .expect("whitespace-only args still complete on Stop (no End frame)");
    let done_pos = evs
        .iter()
        .position(|e| matches!(e, ChatEvent::Done(_)))
        .unwrap();
    assert!(
        completed_pos < done_pos,
        "Stop drives the completion before emitting Done"
    );
    match &evs[completed_pos] {
        ChatEvent::ToolCallCompleted {
            id, name, input, ..
        } => {
            assert_eq!(id, "call_ws");
            assert_eq!(name, "noop");
            assert_eq!(input, &json!({}), "trimmed-empty args parse to {{}}");
        }
        _ => unreachable!(),
    }
}

#[test]
fn full_pipeline_usage_parallel_tools_and_text_fold_into_one_response() {
    let evs = collect(vec![
        WireDelta::Usage(Usage {
            input: 120,
            output: 0,
            cache_read: 30,
            cache_write: 0,
        }),
        WireDelta::Text("Looking into it. ".into()),
        WireDelta::ToolCallStart {
            index: Some(0),
            id: Some("call_a".into()),
            name: Some("read_file".into()),
        },
        WireDelta::ToolCallStart {
            index: Some(1),
            id: Some("call_b".into()),
            name: Some("list_dir".into()),
        },
        WireDelta::ToolCallArgs {
            index: Some(1),
            fragment: "{\"path\":\"/src\"}".into(),
        },
        WireDelta::ToolCallArgs {
            index: Some(0),
            fragment: "{\"path\":\"a.rs\"}".into(),
        },
        WireDelta::Text("Running two tools.".into()),
        WireDelta::Usage(Usage {
            input: 0,
            output: 64,
            cache_read: 0,
            cache_write: 0,
        }),
        WireDelta::Stop(StopReason::ToolUse),
    ]);

    let resp = ChatResponse::from_events(evs);

    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.input, 120);
    assert_eq!(resp.usage.output, 64);
    assert_eq!(resp.usage.cache_read, 30);
    assert_eq!(resp.usage.total(), 184);
    assert_eq!(resp.text(), "Looking into it. Running two tools.");

    let tool_uses: Vec<_> = resp
        .tool_uses()
        .map(|(id, name, input)| (id.to_string(), name.to_string(), input.clone()))
        .collect();
    assert_eq!(tool_uses.len(), 2);
    assert!(tool_uses.contains(&(
        "call_a".to_string(),
        "read_file".to_string(),
        json!({"path": "a.rs"})
    )));
    assert!(tool_uses.contains(&(
        "call_b".to_string(),
        "list_dir".to_string(),
        json!({"path": "/src"})
    )));

    let text_blocks = resp
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::Text(_)))
        .count();
    assert_eq!(
        text_blocks, 1,
        "the two text deltas coalesce into a single Text block"
    );
    let tool_blocks = resp
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .count();
    assert_eq!(tool_blocks, 2);
}
