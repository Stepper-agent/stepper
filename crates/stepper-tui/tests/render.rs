//! Integration coverage of the public TUI surface: the walking-skeleton fake
//! core (`spawn_fake_core`) driven over the real `Action`/`AppEvent` channels,
//! including the embedded-oneshot approval roundtrip that suspends the turn.

use std::time::Duration;

use stepper_protocol::{Action, AppEvent, ApprovalDecision, ApprovalKind};
use stepper_tui::spawn_fake_core;
use tokio::sync::mpsc;
use tokio::time::timeout;

async fn next_event(rx: &mut mpsc::Receiver<AppEvent>) -> AppEvent {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("fake core stalled")
        .expect("fake core closed the event channel")
}

async fn drain_to_approval(rx: &mut mpsc::Receiver<AppEvent>) -> stepper_protocol::ApprovalRequest {
    loop {
        match next_event(rx).await {
            AppEvent::ApprovalRequested(req) => return req,
            AppEvent::TurnComplete { .. } => panic!("turn completed before requesting approval"),
            _ => {}
        }
    }
}

async fn collect_until_complete(rx: &mut mpsc::Receiver<AppEvent>) -> Vec<AppEvent> {
    let mut events = Vec::new();
    loop {
        let ev = next_event(rx).await;
        let done = matches!(ev, AppEvent::TurnComplete { .. });
        events.push(ev);
        if done {
            return events;
        }
    }
}

#[tokio::test]
async fn submit_runs_two_layer_turn_and_completes_after_approval() {
    let (action_tx, action_rx) = mpsc::channel::<Action>(16);
    let mut event_rx = spawn_fake_core(action_rx);

    action_tx
        .send(Action::SubmitInput("add a greeting".into()))
        .await
        .unwrap();

    assert!(matches!(next_event(&mut event_rx).await, AppEvent::TurnStarted { turn_id: 1 }));

    let req = drain_to_approval(&mut event_rx).await;
    assert!(matches!(req.kind, ApprovalKind::FileEdit(_)));
    req.reply.send(ApprovalDecision::AllowOnce).unwrap();

    let rest = collect_until_complete(&mut event_rx).await;
    let applied = rest.iter().any(|e| matches!(
        e, AppEvent::ToolCallFinished { ok: true, .. }
    ));
    assert!(applied, "edit should be applied after approval");
    let layer_done = rest.iter().any(|e| matches!(
        e,
        AppEvent::LayerFinished { status: stepper_protocol::LayerStatus::Done, index: 1 }
    ));
    assert!(layer_done, "implement layer should finish Done");
    assert!(matches!(rest.last(), Some(AppEvent::TurnComplete { turn_id: 1 })));
}

#[tokio::test]
async fn denying_the_edit_fails_the_layer_and_still_completes() {
    let (action_tx, action_rx) = mpsc::channel::<Action>(16);
    let mut event_rx = spawn_fake_core(action_rx);

    action_tx
        .send(Action::SubmitInput("risky change".into()))
        .await
        .unwrap();

    let req = drain_to_approval(&mut event_rx).await;
    req.reply.send(ApprovalDecision::Deny).unwrap();

    let rest = collect_until_complete(&mut event_rx).await;
    assert!(rest.iter().any(|e| matches!(
        e, AppEvent::ToolCallFinished { ok: false, .. }
    )));
    assert!(rest.iter().any(|e| matches!(
        e,
        AppEvent::LayerFinished { status: stepper_protocol::LayerStatus::Failed, .. }
    )));
    assert!(matches!(rest.last(), Some(AppEvent::TurnComplete { .. })));
}

#[tokio::test]
async fn run_shell_emits_a_bash_tool_call_turn() {
    let (action_tx, action_rx) = mpsc::channel::<Action>(16);
    let mut event_rx = spawn_fake_core(action_rx);

    action_tx
        .send(Action::RunShell("echo hi".into()))
        .await
        .unwrap();

    assert!(matches!(next_event(&mut event_rx).await, AppEvent::TurnStarted { .. }));
    let events = collect_until_complete(&mut event_rx).await;
    assert!(events.iter().any(|e| matches!(
        e, AppEvent::ToolCallStarted(v) if v.name == "bash" && v.summary == "echo hi"
    )));
    assert!(events.iter().any(|e| matches!(
        e, AppEvent::ToolCallFinished { ok: true, .. }
    )));
}

#[tokio::test]
async fn quit_action_shuts_down_the_fake_core() {
    let (action_tx, action_rx) = mpsc::channel::<Action>(16);
    let mut event_rx = spawn_fake_core(action_rx);

    action_tx.send(Action::Quit).await.unwrap();

    let closed = timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("fake core did not shut down");
    assert!(closed.is_none(), "event channel should close after Quit");
}

#[tokio::test]
async fn unhandled_actions_are_ignored_without_emitting_events() {
    let (action_tx, action_rx) = mpsc::channel::<Action>(16);
    let mut event_rx = spawn_fake_core(action_rx);

    action_tx.send(Action::Interrupt).await.unwrap();

    let nothing = timeout(Duration::from_millis(200), event_rx.recv()).await;
    assert!(nothing.is_err(), "an ignored action must not produce events");
}
