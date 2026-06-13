//! Index-less tool-call frames (oMLX/Ollama OpenAI-compat servers omit
//! `tool_calls[].index`): the accumulator attributes an index-less fragment to
//! the most recently started call instead of defaulting to call 0 — which used
//! to corrupt parallel calls — and a fresh index-less `id` opens a new call.

use serde_json::json;
use stepper_provider::{ChatEvent, StopReason, StreamAccumulator, WireDelta};

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
fn single_index_less_call_assembles_correctly() {
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: None,
            id: Some("call_1".into()),
            name: Some("get_weather".into()),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "{\"city\":".into(),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "\"seoul\"}".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let done = completed(&evs);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].1, "call_1");
    assert_eq!(done[0].2, "get_weather");
    assert_eq!(done[0].3, json!({"city": "seoul"}));
}

#[test]
fn two_index_less_calls_with_distinct_ids_do_not_corrupt_each_other() {
    // The Ollama/oMLX pattern: each parallel call streams sequentially with no
    // index anywhere. With the old `#[serde(default)] index = 0` both calls
    // collapsed into slot 0 and their arguments concatenated.
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: None,
            id: Some("call_a".into()),
            name: Some("read_file".into()),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "{\"path\":\"a.rs\"}".into(),
        },
        WireDelta::ToolCallStart {
            index: None,
            id: Some("call_b".into()),
            name: Some("list_dir".into()),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "{\"path\":\"/src\"}".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let mut done = completed(&evs);
    done.sort_by_key(|(i, ..)| *i);
    assert_eq!(done.len(), 2, "two distinct calls, not one corrupted call");
    assert_eq!(
        done[0],
        (
            0,
            "call_a".into(),
            "read_file".into(),
            json!({"path": "a.rs"})
        )
    );
    assert_eq!(
        done[1],
        (
            1,
            "call_b".into(),
            "list_dir".into(),
            json!({"path": "/src"})
        )
    );
}

#[test]
fn index_less_args_attach_to_the_most_recently_started_indexed_call() {
    // Starts carry indexes but a later argument fragment omits its index: it
    // belongs to the most recently started call (1), never silently to call 0.
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
        WireDelta::ToolCallStart {
            index: Some(1),
            id: Some("b".into()),
            name: Some("second".into()),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "{\"y\":2}".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let mut done = completed(&evs);
    done.sort_by_key(|(i, ..)| *i);
    assert_eq!(done.len(), 2);
    assert_eq!(done[0].3, json!({"x": 1}), "call 0 keeps only its own args");
    assert_eq!(
        done[1].3,
        json!({"y": 2}),
        "the index-less fragment landed on the most recently started call"
    );
}

#[test]
fn repeated_index_less_start_with_the_same_id_continues_the_same_call() {
    // Some servers resend id+name on every fragment chunk; same id must merge
    // into the in-flight call instead of opening a duplicate.
    let evs = collect(vec![
        WireDelta::ToolCallStart {
            index: None,
            id: Some("call_1".into()),
            name: Some("edit_file".into()),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "{\"pa".into(),
        },
        WireDelta::ToolCallStart {
            index: None,
            id: Some("call_1".into()),
            name: Some("edit_file".into()),
        },
        WireDelta::ToolCallArgs {
            index: None,
            fragment: "th\":\"a.rs\"}".into(),
        },
        WireDelta::Stop(StopReason::ToolUse),
    ]);
    let done = completed(&evs);
    assert_eq!(done.len(), 1, "same id continues the same call");
    assert_eq!(done[0].3, json!({"path": "a.rs"}));
    let started = evs
        .iter()
        .filter(|e| matches!(e, ChatEvent::ToolCallStarted { .. }))
        .count();
    assert_eq!(started, 1, "the call is announced exactly once");
}

#[test]
fn index_less_args_with_no_call_at_all_target_slot_zero_and_stay_unstarted() {
    let mut acc = StreamAccumulator::new();
    let from_args = acc.push(WireDelta::ToolCallArgs {
        index: None,
        fragment: "{\"k\":1}".into(),
    });
    assert!(
        from_args.is_empty(),
        "no started call exists, so nothing is announced"
    );
    let at_stop = acc.push(WireDelta::Stop(StopReason::ToolUse));
    assert!(
        !at_stop
            .iter()
            .any(|e| matches!(e, ChatEvent::ToolCallCompleted { .. })),
        "an id/name-less slot-0 call is dropped at Stop like any unstarted call"
    );
}
