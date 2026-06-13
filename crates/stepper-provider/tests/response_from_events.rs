//! `ChatResponse::from_events` folding contract: adjacent Text and adjacent
//! Thinking deltas coalesce into single blocks, tool-use blocks keep their
//! emission order relative to text, usage is merged, and the final stop reason
//! is taken from the `Done` event.

use serde_json::json;
use stepper_provider::{ChatEvent, ChatResponse, ContentBlock, StopReason, Usage};

#[test]
fn adjacent_text_deltas_coalesce_into_one_text_block() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::TextDelta("Hel".into()),
        ChatEvent::TextDelta("lo, ".into()),
        ChatEvent::TextDelta("world".into()),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    assert_eq!(resp.content.len(), 1);
    assert_eq!(resp.text(), "Hello, world");
    assert!(matches!(resp.content[0], ContentBlock::Text(_)));
}

#[test]
fn adjacent_thinking_deltas_coalesce_into_one_thinking_block() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ThinkingDelta("step ".into()),
        ChatEvent::ThinkingDelta("by step".into()),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    let thinking: Vec<_> = resp
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, vec!["step by step".to_string()]);
}

#[test]
fn text_split_by_a_tool_use_yields_two_distinct_text_blocks() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::TextDelta("before ".into()),
        ChatEvent::ToolCallCompleted {
            index: 0,
            id: "t1".into(),
            name: "run".into(),
            input: json!({"a": 1}),
        },
        ChatEvent::TextDelta("after".into()),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    assert!(matches!(resp.content[0], ContentBlock::Text(ref t) if t == "before "));
    assert!(matches!(resp.content[1], ContentBlock::ToolUse { .. }));
    assert!(matches!(resp.content[2], ContentBlock::Text(ref t) if t == "after"));
    assert_eq!(resp.text(), "before after");
}

#[test]
fn tool_use_blocks_preserve_emission_order() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ToolCallCompleted {
            index: 0,
            id: "first".into(),
            name: "alpha".into(),
            input: json!({}),
        },
        ChatEvent::ToolCallCompleted {
            index: 1,
            id: "second".into(),
            name: "beta".into(),
            input: json!({}),
        },
        ChatEvent::Done(StopReason::ToolUse),
    ]);
    let uses: Vec<_> = resp.tool_uses().map(|(id, name, _)| (id.to_string(), name.to_string())).collect();
    assert_eq!(
        uses,
        vec![
            ("first".to_string(), "alpha".to_string()),
            ("second".to_string(), "beta".to_string()),
        ]
    );
}

#[test]
fn started_and_args_delta_events_are_ignored_in_the_fold() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ToolCallStarted {
            index: 0,
            id: "t".into(),
            name: "n".into(),
        },
        ChatEvent::ToolCallArgsDelta {
            index: 0,
            fragment: "{".into(),
        },
        ChatEvent::ToolCallCompleted {
            index: 0,
            id: "t".into(),
            name: "n".into(),
            input: json!({"k": 1}),
        },
        ChatEvent::Done(StopReason::ToolUse),
    ]);
    assert_eq!(resp.content.len(), 1);
    let (id, name, input) = resp.tool_uses().next().unwrap();
    assert_eq!(id, "t");
    assert_eq!(name, "n");
    assert_eq!(input, &json!({"k": 1}));
}

#[test]
fn usage_events_are_merged_field_wise_into_the_response() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::Usage(Usage {
            input: 200,
            output: 0,
            cache_read: 50,
            cache_write: 0,
        }),
        ChatEvent::Usage(Usage {
            input: 0,
            output: 80,
            cache_read: 0,
            cache_write: 0,
        }),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    assert_eq!(resp.usage.input, 200);
    assert_eq!(resp.usage.output, 80);
    assert_eq!(resp.usage.cache_read, 50);
}

#[test]
fn done_event_sets_the_stop_reason_and_a_streamless_fold_defaults_to_end_turn() {
    let with_done = ChatResponse::from_events(vec![ChatEvent::Done(StopReason::MaxTokens)]);
    assert_eq!(with_done.stop_reason, StopReason::MaxTokens);

    let without_done = ChatResponse::from_events(vec![ChatEvent::TextDelta("hi".into())]);
    assert_eq!(without_done.stop_reason, StopReason::EndTurn);
}

#[test]
fn last_done_event_wins_when_multiple_are_present() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::Done(StopReason::EndTurn),
        ChatEvent::Done(StopReason::Refusal),
    ]);
    assert_eq!(resp.stop_reason, StopReason::Refusal);
}

#[test]
fn text_accessor_ignores_thinking_and_tool_use_blocks() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ThinkingDelta("hidden".into()),
        ChatEvent::TextDelta("visible".into()),
        ChatEvent::ToolCallCompleted {
            index: 0,
            id: "t".into(),
            name: "n".into(),
            input: json!({}),
        },
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    assert_eq!(resp.text(), "visible");
}
