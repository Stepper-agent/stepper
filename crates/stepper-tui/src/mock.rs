//! Walking-skeleton fake core: scripts a realistic 2-layer turn (plan -> implement
//! with a diff approval) so the TUI is fully exercisable before the real
//! `stepper-core` exists. Wired by the CLI in place of `RealCore`.

use std::time::Duration;
use stepper_protocol::{
    Action, ActionRx, AppEvent, ApprovalDecision, ApprovalKind, ApprovalRequest, DiffView, EventRx,
    LayerStatus, ModelView, NoticeLevel, ToolCallView, UsageView,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const CONTEXT_LIMIT: u64 = 128_000;

pub fn spawn_fake_core(mut action_rx: ActionRx) -> EventRx {
    let (tx, rx) = mpsc::channel::<AppEvent>(64);
    tokio::spawn(async move {
        while let Some(action) = action_rx.recv().await {
            let result = match action {
                Action::Quit => break,
                Action::SubmitInput(prompt) => scripted_turn(&tx, &prompt).await,
                Action::RunShell(cmd) => scripted_bash(&tx, &cmd).await,
                _ => continue,
            };
            if result.is_err() {
                break;
            }
        }
    });
    rx
}

async fn scripted_turn(tx: &mpsc::Sender<AppEvent>, prompt: &str) -> Result<(), ()> {
    let mut ctx = 0u64;

    emit(tx, AppEvent::TurnStarted { turn_id: 1 }).await?;

    // ── layer 1/2: plan (ollama-cloud qwen) ───────────────────────────────
    emit(tx, AppEvent::ModelChanged(model("ollama-cloud", "qwen3-coder"))).await?;
    emit(
        tx,
        AppEvent::LayerStarted { index: 0, total: 2, name: "plan".into() },
    )
    .await?;
    for chunk in [
        "# Plan\n\n".to_string(),
        format!("Goal: {prompt}\n\n"),
        "1. Read `src/main.rs`\n".to_string(),
        "2. Apply the edit\n".to_string(),
        "3. Verify with `cargo check`\n".to_string(),
    ] {
        emit(tx, AppEvent::AssistantTokenDelta(chunk)).await?;
        ctx += 220;
        emit(tx, AppEvent::UsageUpdated(usage(ctx))).await?;
        sleep(90).await;
    }
    emit(tx, AppEvent::LayerFinished { index: 0, status: LayerStatus::Done }).await?;

    // ── layer 2/2: implement (omlx deepseek) ──────────────────────────────
    emit(tx, AppEvent::ModelChanged(model("omlx", "deepseek-coder-v2"))).await?;
    emit(
        tx,
        AppEvent::LayerStarted { index: 1, total: 2, name: "implement".into() },
    )
    .await?;
    let tool_id = "edit-1".to_string();
    emit(
        tx,
        AppEvent::ToolCallStarted(ToolCallView {
            id: tool_id.clone(),
            name: "edit_file".into(),
            summary: "src/main.rs".into(),
        }),
    )
    .await?;

    // Approval round-trip via the oneshot embedded in the request.
    let (reply_tx, reply_rx) = oneshot::channel();
    emit(
        tx,
        AppEvent::ApprovalRequested(ApprovalRequest {
            id: Uuid::new_v4(),
            kind: ApprovalKind::FileEdit(DiffView {
                path: "src/main.rs".into(),
                old: "fn main() {\n    println!(\"hi\");\n}\n".into(),
                new: "fn main() {\n    println!(\"hello, stepper\");\n}\n".into(),
            }),
            reply: reply_tx,
        }),
    )
    .await?;

    match reply_rx.await.map_err(|_| ())? {
        ApprovalDecision::Deny => {
            emit(tx, AppEvent::ToolCallFinished { id: tool_id, ok: false }).await?;
            emit(
                tx,
                AppEvent::Notice { level: NoticeLevel::Warn, text: "edit rejected".into() },
            )
            .await?;
            emit(tx, AppEvent::LayerFinished { index: 1, status: LayerStatus::Failed }).await?;
        }
        ApprovalDecision::AllowOnce | ApprovalDecision::AlwaysAllow { .. } => {
            emit(tx, AppEvent::ToolCallFinished { id: tool_id, ok: true }).await?;
            for chunk in [
                "Applied the edit to `main.rs`.\n\n".to_string(),
                "Ran `cargo check` — clean.\n".to_string(),
            ] {
                emit(tx, AppEvent::AssistantTokenDelta(chunk)).await?;
                ctx += 300;
                emit(tx, AppEvent::UsageUpdated(usage(ctx))).await?;
                sleep(90).await;
            }
            emit(tx, AppEvent::LayerFinished { index: 1, status: LayerStatus::Done }).await?;
        }
    }

    emit(tx, AppEvent::TurnComplete { turn_id: 1 }).await
}

async fn scripted_bash(tx: &mpsc::Sender<AppEvent>, cmd: &str) -> Result<(), ()> {
    emit(tx, AppEvent::TurnStarted { turn_id: 1 }).await?;
    let id = "bash-1".to_string();
    emit(
        tx,
        AppEvent::ToolCallStarted(ToolCallView {
            id: id.clone(),
            name: "bash".into(),
            summary: cmd.to_string(),
        }),
    )
    .await?;
    emit(
        tx,
        AppEvent::AssistantTokenDelta(format!(
            "```sh\n$ {cmd}\n```\n\n_(real sandboxed shell execution lands in phase 3)_\n"
        )),
    )
    .await?;
    emit(tx, AppEvent::ToolCallFinished { id, ok: true }).await?;
    emit(tx, AppEvent::TurnComplete { turn_id: 1 }).await
}

async fn emit(tx: &mpsc::Sender<AppEvent>, event: AppEvent) -> Result<(), ()> {
    tx.send(event).await.map_err(|_| ())
}

async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

fn model(provider: &str, model: &str) -> ModelView {
    ModelView { provider: provider.into(), model: model.into() }
}

fn usage(context_used: u64) -> UsageView {
    UsageView {
        tokens_in: context_used,
        tokens_out: context_used / 4,
        context_used,
        context_limit: CONTEXT_LIMIT,
        ..Default::default()
    }
}
