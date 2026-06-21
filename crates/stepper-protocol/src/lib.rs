//! `stepper-protocol` — the TUI <-> core wire contract.
//!
//! This crate holds the `Action` (TUI->core) and `AppEvent` (core->TUI) enums
//! plus the plain view DTOs they exchange. It is the ONLY crate `stepper-tui`
//! is allowed to depend on, so it deliberately pulls NO async runtime (tokio is
//! `sync`-feature only), NO http client, and NO clap. Keeping it lean is what
//! mechanically prevents the UI from reaching into the agent core.

pub mod action;
pub mod approval;
pub mod event;
pub mod mode;
pub mod question;
pub mod view;

pub use action::{Action, RewindScope};
pub use approval::{ApprovalDecision, ApprovalKind, ApprovalRequest};
pub use event::AppEvent;
pub use question::QuestionRequest;
pub use mode::Mode;
pub use view::{
    ApprovalRuleView, CheckpointView, ContextBreakdownView, DiffView, LayerStatus, LayerView,
    ModelChoiceView, ModelView, NoticeLevel, PermissionRuleView, PermissionsSnapshotView,
    ProviderChoiceView, SessionView, SettingsRowView, SettingsSnapshotView, SettingsTabView,
    TodoItemView, TodoStatus, ToolCallView, UsageView, WorkerView,
};

/// Channel aliases wired by the CLI between the TUI and core.
pub type ActionTx = tokio::sync::mpsc::Sender<Action>;
pub type ActionRx = tokio::sync::mpsc::Receiver<Action>;
pub type EventTx = tokio::sync::mpsc::Sender<AppEvent>;
pub type EventRx = tokio::sync::mpsc::Receiver<AppEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycles_auto_plan_acceptedits_default() {
        assert_eq!(Mode::Auto.next(), Mode::Plan);
        assert_eq!(Mode::Plan.next(), Mode::AcceptEdits);
        assert_eq!(Mode::AcceptEdits.next(), Mode::Default);
        assert_eq!(Mode::Default.next(), Mode::Auto);
        assert_eq!(Mode::default(), Mode::Auto);
    }

    #[test]
    fn mode_cycle_never_enters_dont_ask_or_bypass() {
        let mut mode = Mode::default();
        for _ in 0..8 {
            mode = mode.next();
            assert!(!matches!(mode, Mode::DontAsk | Mode::Bypass));
        }
        // Cycling out of the explicit opt-in modes lands on the safe Default.
        assert_eq!(Mode::DontAsk.next(), Mode::Default);
        assert_eq!(Mode::Bypass.next(), Mode::Default);
    }

    #[test]
    fn context_pct_left_is_clamped() {
        let u = UsageView {
            context_used: 30,
            context_limit: 100,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 70);

        // over-limit clamps to 0, not underflow
        let full = UsageView {
            context_used: 150,
            context_limit: 100,
            ..Default::default()
        };
        assert_eq!(full.context_pct_left(), 0);

        // unknown limit -> treat as fully free
        let unknown = UsageView::default();
        assert_eq!(unknown.context_pct_left(), 100);
    }

    #[test]
    fn view_dtos_roundtrip_json() {
        let usage = UsageView {
            tokens_in: 10,
            tokens_out: 20,
            context_used: 5,
            context_limit: 200,
            cost_usd: 0.0,
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: UsageView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tokens_total(), 30);
    }

    #[test]
    fn context_pct_left_full_window_is_free() {
        let u = UsageView {
            context_used: 0,
            context_limit: 1000,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 100);
    }

    #[test]
    fn context_pct_left_exactly_full_is_zero() {
        let u = UsageView {
            context_used: 100,
            context_limit: 100,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 0);
    }

    #[test]
    fn context_pct_left_single_token_used_rounds_down() {
        let u = UsageView {
            context_used: 1,
            context_limit: 1000,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 99);
    }

    #[test]
    fn context_pct_left_half_used_is_fifty() {
        let u = UsageView {
            context_used: 64_000,
            context_limit: 128_000,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 50);
    }

    #[test]
    fn context_pct_left_uses_integer_floor_not_round() {
        let u = UsageView {
            context_used: 1,
            context_limit: 3,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 66);
    }

    #[test]
    fn context_pct_left_large_window_does_not_overflow() {
        let u = UsageView {
            context_used: 500_000,
            context_limit: 1_000_000,
            ..Default::default()
        };
        assert_eq!(u.context_pct_left(), 50);
    }

    #[test]
    fn tokens_total_sums_in_and_out_only() {
        let u = UsageView {
            tokens_in: 7,
            tokens_out: 11,
            cache_read: 100,
            cache_write: 200,
            ..Default::default()
        };
        assert_eq!(u.tokens_total(), 18);
    }

    #[test]
    fn mode_next_completes_full_cycle() {
        let mut mode = Mode::default();
        let cycle: Vec<Mode> = (0..4)
            .map(|_| {
                let current = mode;
                mode = mode.next();
                current
            })
            .collect();
        assert_eq!(
            cycle,
            vec![Mode::Auto, Mode::Plan, Mode::AcceptEdits, Mode::Default]
        );
        assert_eq!(mode, Mode::Auto);
    }

    #[test]
    fn mode_labels_are_stable_strings() {
        assert_eq!(Mode::Auto.label(), "auto");
        assert_eq!(Mode::Plan.label(), "plan");
        assert_eq!(Mode::AcceptEdits.label(), "accept-edits");
        assert_eq!(Mode::Default.label(), "default");
        assert_eq!(Mode::DontAsk.label(), "dont-ask");
        assert_eq!(Mode::Bypass.label(), "bypass");
    }

    #[test]
    fn mode_serializes_kebab_case() {
        assert_eq!(serde_json::to_string(&Mode::Auto).unwrap(), "\"auto\"");
        assert_eq!(serde_json::to_string(&Mode::Plan).unwrap(), "\"plan\"");
        assert_eq!(
            serde_json::to_string(&Mode::AcceptEdits).unwrap(),
            "\"accept-edits\""
        );
        assert_eq!(
            serde_json::to_string(&Mode::Default).unwrap(),
            "\"default\""
        );
        assert_eq!(
            serde_json::to_string(&Mode::DontAsk).unwrap(),
            "\"dont-ask\""
        );
        assert_eq!(serde_json::to_string(&Mode::Bypass).unwrap(), "\"bypass\"");
    }

    #[test]
    fn mode_roundtrips_through_json() {
        for mode in [
            Mode::Auto,
            Mode::Plan,
            Mode::AcceptEdits,
            Mode::Default,
            Mode::DontAsk,
            Mode::Bypass,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: Mode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn mode_rejects_unknown_label() {
        assert!(serde_json::from_str::<Mode>("\"accept_edits\"").is_err());
        assert!(serde_json::from_str::<Mode>("\"manual\"").is_err());
        assert!(serde_json::from_str::<Mode>("\"dont_ask\"").is_err());
        assert!(serde_json::from_str::<Mode>("\"bypass-permissions\"").is_err());
    }

    #[test]
    fn todo_status_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&TodoStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&TodoStatus::InProgress).unwrap(),
            "\"in-progress\""
        );
        assert_eq!(
            serde_json::to_string(&TodoStatus::Completed).unwrap(),
            "\"completed\""
        );
    }

    #[test]
    fn layer_status_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&LayerStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&LayerStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&LayerStatus::Done).unwrap(),
            "\"done\""
        );
        assert_eq!(
            serde_json::to_string(&LayerStatus::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn notice_level_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&NoticeLevel::Info).unwrap(),
            "\"info\""
        );
        assert_eq!(
            serde_json::to_string(&NoticeLevel::Warn).unwrap(),
            "\"warn\""
        );
        assert_eq!(
            serde_json::to_string(&NoticeLevel::Error).unwrap(),
            "\"error\""
        );
    }

    #[test]
    fn todo_item_view_roundtrips_through_json() {
        let item = TodoItemView {
            id: "t-1".to_string(),
            content: "wire the footer gauge".to_string(),
            status: TodoStatus::InProgress,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: TodoItemView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "t-1");
        assert_eq!(back.content, "wire the footer gauge");
        assert_eq!(back.status, TodoStatus::InProgress);
    }

    #[test]
    fn layer_view_roundtrips_through_json() {
        let layer = LayerView {
            name: "plan".to_string(),
            index: 2,
            total: 5,
            status: LayerStatus::Running,
        };
        let json = serde_json::to_string(&layer).unwrap();
        let back: LayerView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "plan");
        assert_eq!(back.index, 2);
        assert_eq!(back.total, 5);
        assert_eq!(back.status, LayerStatus::Running);
    }

    #[test]
    fn worker_view_roundtrips_through_json() {
        let w = WorkerView {
            index: 1,
            total: 3,
            label: "db-layer".to_string(),
            provider: "omlx".to_string(),
            model: "qwen3".to_string(),
            tokens: 1234,
            last_tool: Some("edit_file: db.rs".to_string()),
            status: LayerStatus::Running,
        };
        let json = serde_json::to_string(&w).unwrap();
        let back: WorkerView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.index, 1);
        assert_eq!(back.total, 3);
        assert_eq!(back.label, "db-layer");
        assert_eq!(back.tokens, 1234);
        assert_eq!(back.last_tool.as_deref(), Some("edit_file: db.rs"));
        assert_eq!(back.status, LayerStatus::Running);
    }

    #[test]
    fn model_view_roundtrips_through_json() {
        let model = ModelView {
            provider: "omlx".to_string(),
            model: "qwen3".to_string(),
        };
        let json = serde_json::to_string(&model).unwrap();
        let back: ModelView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider, "omlx");
        assert_eq!(back.model, "qwen3");
    }

    #[test]
    fn diff_view_roundtrips_through_json() {
        let diff = DiffView {
            path: std::path::PathBuf::from("src/main.rs"),
            old: "fn old() {}".to_string(),
            new: "fn new() {}".to_string(),
        };
        let json = serde_json::to_string(&diff).unwrap();
        let back: DiffView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.path, std::path::PathBuf::from("src/main.rs"));
        assert_eq!(back.old, "fn old() {}");
        assert_eq!(back.new, "fn new() {}");
    }

    #[test]
    fn tool_call_view_roundtrips_through_json() {
        let call = ToolCallView {
            id: "call-9".to_string(),
            name: "shell".to_string(),
            summary: "ls -la".to_string(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCallView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call-9");
        assert_eq!(back.name, "shell");
        assert_eq!(back.summary, "ls -la");
    }

    #[test]
    fn usage_view_default_is_all_zero() {
        let u = UsageView::default();
        assert_eq!(u.tokens_in, 0);
        assert_eq!(u.tokens_out, 0);
        assert_eq!(u.cache_read, 0);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.context_used, 0);
        assert_eq!(u.context_limit, 0);
        assert_eq!(u.cost_usd, 0.0);
    }

    #[test]
    fn usage_view_preserves_cost_through_json() {
        let usage = UsageView {
            cost_usd: 1.2345,
            ..Default::default()
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: UsageView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cost_usd, 1.2345);
    }

    #[test]
    fn context_pct_left_extreme_limit_does_not_overflow() {
        let empty = UsageView {
            context_used: 0,
            context_limit: u64::MAX,
            ..Default::default()
        };
        assert_eq!(empty.context_pct_left(), 100);

        let one_used = UsageView {
            context_used: 1,
            context_limit: u64::MAX,
            ..Default::default()
        };
        assert!((0..=100).contains(&one_used.context_pct_left()));
        assert_eq!(one_used.context_pct_left(), 99);

        let half = UsageView {
            context_used: u64::MAX / 2,
            context_limit: u64::MAX,
            ..Default::default()
        };
        assert_eq!(half.context_pct_left(), 50);

        let full = UsageView {
            context_used: u64::MAX,
            context_limit: u64::MAX,
            ..Default::default()
        };
        assert_eq!(full.context_pct_left(), 0);
    }

    #[test]
    fn usage_view_preserves_all_token_fields_through_json() {
        let usage = UsageView {
            tokens_in: 11,
            tokens_out: 22,
            cache_read: 333,
            cache_write: 4444,
            context_used: 55_555,
            context_limit: 200_000,
            cost_usd: 0.0,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: UsageView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cache_read, 333);
        assert_eq!(back.cache_write, 4444);
        assert_eq!(back.context_used, 55_555);
        assert_eq!(back.context_limit, 200_000);
    }

    #[test]
    fn todo_status_rejects_unknown_label() {
        assert!(serde_json::from_str::<TodoStatus>("\"in_progress\"").is_err());
        assert!(serde_json::from_str::<TodoStatus>("\"InProgress\"").is_err());
        assert!(serde_json::from_str::<TodoStatus>("\"done\"").is_err());
    }

    #[test]
    fn layer_status_rejects_unknown_label() {
        assert!(serde_json::from_str::<LayerStatus>("\"Running\"").is_err());
        assert!(serde_json::from_str::<LayerStatus>("\"in-progress\"").is_err());
        assert!(serde_json::from_str::<LayerStatus>("\"completed\"").is_err());
    }

    #[test]
    fn notice_level_rejects_unknown_label() {
        assert!(serde_json::from_str::<NoticeLevel>("\"Info\"").is_err());
        assert!(serde_json::from_str::<NoticeLevel>("\"warning\"").is_err());
        assert!(serde_json::from_str::<NoticeLevel>("\"debug\"").is_err());
    }
}
