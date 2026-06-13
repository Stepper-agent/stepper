//! `StopReason` mapping contract: only `ToolUse` wants tools, the `Other`
//! variant carries an arbitrary dialect label, and the type round-trips through
//! serde so it can be persisted in the normalized response.

use stepper_provider::StopReason;

#[test]
fn only_tool_use_wants_tools() {
    assert!(StopReason::ToolUse.wants_tools());
    assert!(!StopReason::EndTurn.wants_tools());
    assert!(!StopReason::MaxTokens.wants_tools());
    assert!(!StopReason::StopSequence.wants_tools());
    assert!(!StopReason::Refusal.wants_tools());
    assert!(!StopReason::Other("content_filter".into()).wants_tools());
}

#[test]
fn other_variant_preserves_the_raw_dialect_label() {
    let reason = StopReason::Other("content_filter".into());
    match reason {
        StopReason::Other(label) => assert_eq!(label, "content_filter"),
        _ => panic!("expected Other variant"),
    }
}

#[test]
fn known_variants_round_trip_through_serde() {
    for reason in [
        StopReason::EndTurn,
        StopReason::ToolUse,
        StopReason::MaxTokens,
        StopReason::StopSequence,
        StopReason::Refusal,
    ] {
        let json = serde_json::to_string(&reason).unwrap();
        let back: StopReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reason);
    }
}

#[test]
fn other_variant_round_trips_with_its_payload() {
    let reason = StopReason::Other("custom_stop".into());
    let json = serde_json::to_string(&reason).unwrap();
    let back: StopReason = serde_json::from_str(&json).unwrap();
    assert_eq!(back, reason);
}

#[test]
fn distinct_variants_are_not_equal() {
    assert_ne!(StopReason::EndTurn, StopReason::ToolUse);
    assert_ne!(
        StopReason::Other("a".into()),
        StopReason::Other("b".into())
    );
}
