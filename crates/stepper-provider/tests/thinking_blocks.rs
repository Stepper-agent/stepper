//! Thinking-block plumbing: the accumulator forwards Anthropic's
//! `signature_delta` as `ChatEvent::ThinkingSignature`, `from_events` attaches
//! it to the thinking block it signs, and the new `ChatRequest`
//! thinking/reasoning_effort fields stay serde-additive (old payloads without
//! them still deserialize).

use stepper_provider::{
    ChatEvent, ChatRequest, ChatResponse, ContentBlock, StopReason, StreamAccumulator,
    ThinkingConfig, WireDelta,
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
fn signature_delta_passes_through_the_accumulator_and_empty_is_dropped() {
    let evs = collect(vec![
        WireDelta::Thinking("planning".into()),
        WireDelta::ThinkingSignature(String::new()),
        WireDelta::ThinkingSignature("sig-abc".into()),
        WireDelta::Stop(StopReason::EndTurn),
    ]);
    assert_eq!(
        evs,
        vec![
            ChatEvent::ThinkingDelta("planning".into()),
            ChatEvent::ThinkingSignature("sig-abc".into()),
            ChatEvent::Done(StopReason::EndTurn),
        ]
    );
}

#[test]
fn from_events_attaches_the_signature_to_the_thinking_block_it_signs() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ThinkingDelta("step ".into()),
        ChatEvent::ThinkingDelta("by step".into()),
        ChatEvent::ThinkingSignature("sig-1".into()),
        ChatEvent::TextDelta("answer".into()),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    assert_eq!(resp.content.len(), 2);
    match &resp.content[0] {
        ContentBlock::Thinking { text, signature } => {
            assert_eq!(text, "step by step");
            assert_eq!(signature.as_deref(), Some("sig-1"));
        }
        other => panic!("expected a signed thinking block, got {other:?}"),
    }
    assert_eq!(resp.text(), "answer");
}

#[test]
fn each_thinking_block_keeps_its_own_signature() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ThinkingDelta("first".into()),
        ChatEvent::ThinkingSignature("sig-first".into()),
        ChatEvent::TextDelta("interleaved".into()),
        ChatEvent::ThinkingDelta("second".into()),
        ChatEvent::ThinkingSignature("sig-second".into()),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    let signed: Vec<_> = resp
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking { text, signature } => {
                Some((text.clone(), signature.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        signed,
        vec![
            ("first".to_string(), Some("sig-first".to_string())),
            ("second".to_string(), Some("sig-second".to_string())),
        ]
    );
}

#[test]
fn thinking_without_a_signature_folds_with_signature_none() {
    let resp = ChatResponse::from_events(vec![
        ChatEvent::ThinkingDelta("unsigned".into()),
        ChatEvent::Done(StopReason::EndTurn),
    ]);
    match &resp.content[0] {
        ContentBlock::Thinking { text, signature } => {
            assert_eq!(text, "unsigned");
            assert_eq!(signature, &None);
        }
        other => panic!("expected a thinking block, got {other:?}"),
    }
}

#[test]
fn chat_request_without_the_new_fields_still_deserializes() {
    let legacy = r#"{
        "model": "m",
        "system": null,
        "messages": [],
        "max_tokens": null,
        "temperature": null,
        "top_p": null
    }"#;
    let req: ChatRequest = serde_json::from_str(legacy).expect("additive fields default");
    assert_eq!(req.thinking, None);
    assert_eq!(req.reasoning_effort, None);
}

#[test]
fn chat_request_thinking_fields_round_trip_through_serde() {
    let mut req = ChatRequest::new("m");
    req.thinking = Some(ThinkingConfig {
        budget_tokens: 4096,
    });
    req.reasoning_effort = Some("high".into());
    let json = serde_json::to_string(&req).expect("serializes");
    let back: ChatRequest = serde_json::from_str(&json).expect("round trips");
    assert_eq!(
        back.thinking,
        Some(ThinkingConfig {
            budget_tokens: 4096
        })
    );
    assert_eq!(back.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn thinking_block_with_signature_round_trips_through_serde() {
    let block = ContentBlock::Thinking {
        text: "reasoning".into(),
        signature: Some("sig".into()),
    };
    let json = serde_json::to_string(&block).expect("serializes");
    let back: ContentBlock = serde_json::from_str(&json).expect("round trips");
    match back {
        ContentBlock::Thinking { text, signature } => {
            assert_eq!(text, "reasoning");
            assert_eq!(signature.as_deref(), Some("sig"));
        }
        other => panic!("expected thinking, got {other:?}"),
    }
    let unsigned = serde_json::to_string(&ContentBlock::Thinking {
        text: "t".into(),
        signature: None,
    })
    .expect("serializes");
    assert!(
        !unsigned.contains("signature"),
        "None signature is skipped on the wire: {unsigned}"
    );
}
