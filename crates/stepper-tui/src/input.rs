use crossterm::event::{Event, KeyCode, KeyModifiers};
use stepper_protocol::{Action, ApprovalDecision, ApprovalKind, ApprovalRequest};

use crate::state::{AppState, Overlay};

pub enum Lowered {
    Action(Action),
    ForwardToTextarea,
    Ignore,
}

/// Navigation intent for the generic list-picker overlay (rewind/resume).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerNav {
    Up,
    Down,
    Select,
    Cancel,
}

/// Lower a raw terminal event into a list-picker navigation key
/// (up/down/enter/esc); anything else is `None`.
pub fn lower_picker_nav(event: &Event) -> Option<PickerNav> {
    let Event::Key(key) = event else {
        return None;
    };
    match key.code {
        KeyCode::Up => Some(PickerNav::Up),
        KeyCode::Down => Some(PickerNav::Down),
        KeyCode::Enter => Some(PickerNav::Select),
        KeyCode::Esc => Some(PickerNav::Cancel),
        _ => None,
    }
}

/// Lower a raw crossterm event into an `Action` (or a signal to forward it to
/// the textarea), resolved against the current overlay/mode. Press-only; the
/// caller filters `KeyEventKind`.
pub fn lower_event(event: &Event, state: &AppState) -> Lowered {
    let Event::Key(key) = event else {
        return Lowered::Ignore;
    };
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // An open approval overlay captures keys first (y / a / n).
    if let Some(Overlay::Approval(req)) = &state.overlay {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => approve(req, ApprovalDecision::AllowOnce),
            KeyCode::Char('a') | KeyCode::Char('A') => {
                approve(req, ApprovalDecision::AlwaysAllow { scope: scope_of(req) })
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                approve(req, ApprovalDecision::Deny)
            }
            _ => Lowered::Ignore,
        };
    }

    match key.code {
        KeyCode::Char('c') if ctrl => Lowered::Action(Action::Quit),
        // Shift+Tab cycles Auto -> Plan -> AcceptEdits -> Default (Claude Code
        // convention); dont-ask/bypass are explicit opt-ins, never cycled into.
        KeyCode::BackTab => Lowered::Action(Action::CycleMode),
        KeyCode::Tab if shift => Lowered::Action(Action::CycleMode),
        KeyCode::Esc => Lowered::Action(Action::Interrupt),
        KeyCode::Enter if shift => Lowered::Action(Action::InsertNewline),
        KeyCode::Enter => submit_action(&state.input_text()),
        // Backspace on an empty input removes the most recently queued message.
        KeyCode::Backspace if state.input_text().is_empty() && !state.queue.is_empty() => {
            Lowered::Action(Action::RemoveLastQueued)
        }
        _ => Lowered::ForwardToTextarea,
    }
}

/// A `!`-prefixed line is a direct shell command and a `/name`-prefixed line is a
/// slash command (Claude-Code-style); everything else is a chat message.
fn submit_action(text: &str) -> Lowered {
    if let Some(rest) = text.strip_prefix('!') {
        let cmd = rest.trim();
        if cmd.is_empty() {
            Lowered::Ignore
        } else {
            Lowered::Action(Action::RunShell(cmd.to_string()))
        }
    } else if text.trim().is_empty() {
        Lowered::Ignore
    } else if let Some(command) = slash_command(text) {
        command
    } else {
        Lowered::Action(Action::SubmitInput(text.to_string()))
    }
}

/// `/name args` is a slash command — but only when `name` looks like a command
/// token (so a pasted absolute path like `/usr/bin` stays a chat message).
fn slash_command(text: &str) -> Option<Lowered> {
    let rest = text.strip_prefix('/')?.trim_start();
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let is_command_name = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b':');
    is_command_name.then(|| {
        Lowered::Action(Action::SlashCommand {
            name: name.to_string(),
            args: args.trim().to_string(),
        })
    })
}

fn approve(req: &ApprovalRequest, decision: ApprovalDecision) -> Lowered {
    Lowered::Action(Action::Approve {
        request_id: req.id,
        decision,
    })
}

fn scope_of(req: &ApprovalRequest) -> String {
    match &req.kind {
        ApprovalKind::Command { cmd, .. } => cmd.clone(),
        ApprovalKind::FileEdit(diff) => diff.path.display().to_string(),
        ApprovalKind::OutsideProject { path, .. } => path.display().to_string(),
        ApprovalKind::Mcp { server, tool } => format!("{server}/{tool}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use std::path::PathBuf;
    use stepper_protocol::{Mode, ModelView};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    use crate::state::Queued;

    fn state() -> AppState {
        AppState::new(crate::TuiInit {
            inline_height: 10,
            model: ModelView { provider: "p".into(), model: "m".into() },
            mode: Mode::Auto,
            cwd: PathBuf::from("/tmp"),
            commands: vec!["review".into(), "rewind".into()],
        })
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn type_text(s: &mut AppState, text: &str) {
        s.textarea.insert_str(text);
    }

    #[test]
    fn enter_on_text_submits_input() {
        let mut s = state();
        type_text(&mut s, "hello there");
        match lower_event(&key(KeyCode::Enter), &s) {
            Lowered::Action(Action::SubmitInput(t)) => assert_eq!(t, "hello there"),
            _ => panic!("expected SubmitInput"),
        }
    }

    #[test]
    fn shift_enter_inserts_newline_not_submit() {
        let mut s = state();
        type_text(&mut s, "line one");
        match lower_event(&key_mod(KeyCode::Enter, KeyModifiers::SHIFT), &s) {
            Lowered::Action(Action::InsertNewline) => {}
            _ => panic!("expected InsertNewline"),
        }
    }

    #[test]
    fn enter_on_blank_input_is_ignored() {
        let mut s = state();
        type_text(&mut s, "   ");
        assert!(matches!(lower_event(&key(KeyCode::Enter), &s), Lowered::Ignore));
    }

    #[test]
    fn bang_prefix_enter_runs_shell_with_trimmed_command() {
        let mut s = state();
        type_text(&mut s, "!  ls -la  ");
        match lower_event(&key(KeyCode::Enter), &s) {
            Lowered::Action(Action::RunShell(cmd)) => assert_eq!(cmd, "ls -la"),
            _ => panic!("expected RunShell"),
        }
    }

    #[test]
    fn bare_bang_with_no_command_is_ignored() {
        let mut s = state();
        type_text(&mut s, "!   ");
        assert!(matches!(lower_event(&key(KeyCode::Enter), &s), Lowered::Ignore));
    }

    #[test]
    fn slash_command_is_parsed_into_name_and_args() {
        let mut s = state();
        type_text(&mut s, "/model   gpt-4  rest");
        match lower_event(&key(KeyCode::Enter), &s) {
            Lowered::Action(Action::SlashCommand { name, args }) => {
                assert_eq!(name, "model");
                assert_eq!(args, "gpt-4  rest");
            }
            _ => panic!("expected SlashCommand"),
        }
    }

    #[test]
    fn absolute_path_is_a_chat_message_not_a_slash_command() {
        let mut s = state();
        type_text(&mut s, "/usr/bin/env");
        match lower_event(&key(KeyCode::Enter), &s) {
            Lowered::Action(Action::SubmitInput(t)) => assert_eq!(t, "/usr/bin/env"),
            _ => panic!("expected SubmitInput for a path-like slash input"),
        }
    }

    #[test]
    fn shift_tab_cycles_mode() {
        let s = state();
        assert!(matches!(
            lower_event(&key_mod(KeyCode::Tab, KeyModifiers::SHIFT), &s),
            Lowered::Action(Action::CycleMode)
        ));
        assert!(matches!(
            lower_event(&key(KeyCode::BackTab), &s),
            Lowered::Action(Action::CycleMode)
        ));
    }

    #[test]
    fn ctrl_c_quits_and_esc_interrupts() {
        let s = state();
        assert!(matches!(
            lower_event(&key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL), &s),
            Lowered::Action(Action::Quit)
        ));
        assert!(matches!(
            lower_event(&key(KeyCode::Esc), &s),
            Lowered::Action(Action::Interrupt)
        ));
    }

    #[test]
    fn backspace_on_empty_input_with_queue_removes_last_queued() {
        let mut s = state();
        s.queue.push_back(Queued::Chat("queued".into()));
        assert!(matches!(
            lower_event(&key(KeyCode::Backspace), &s),
            Lowered::Action(Action::RemoveLastQueued)
        ));
    }

    #[test]
    fn backspace_on_empty_input_without_queue_forwards_to_textarea() {
        let s = state();
        assert!(matches!(
            lower_event(&key(KeyCode::Backspace), &s),
            Lowered::ForwardToTextarea
        ));
    }

    #[test]
    fn backspace_with_text_forwards_to_textarea_even_when_queued() {
        let mut s = state();
        s.queue.push_back(Queued::Chat("queued".into()));
        type_text(&mut s, "x");
        assert!(matches!(
            lower_event(&key(KeyCode::Backspace), &s),
            Lowered::ForwardToTextarea
        ));
    }

    #[test]
    fn plain_character_forwards_to_textarea() {
        let s = state();
        assert!(matches!(
            lower_event(&key(KeyCode::Char('h')), &s),
            Lowered::ForwardToTextarea
        ));
    }

    #[test]
    fn non_key_event_is_ignored() {
        let s = state();
        assert!(matches!(
            lower_event(&Event::FocusGained, &s),
            Lowered::Ignore
        ));
    }

    fn open_approval(s: &mut AppState) -> (Uuid, oneshot::Receiver<ApprovalDecision>) {
        let id = Uuid::new_v4();
        let (reply, rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest {
            id,
            kind: ApprovalKind::Command { cmd: "rm -rf /".into(), outside_project: true },
            reply,
        }));
        (id, rx)
    }

    #[test]
    fn overlay_y_allows_once() {
        let mut s = state();
        let (id, _rx) = open_approval(&mut s);
        match lower_event(&key(KeyCode::Char('y')), &s) {
            Lowered::Action(Action::Approve { request_id, decision }) => {
                assert_eq!(request_id, id);
                assert!(matches!(decision, ApprovalDecision::AllowOnce));
            }
            _ => panic!("expected Approve AllowOnce"),
        }
    }

    #[test]
    fn overlay_a_always_allows_with_command_scope() {
        let mut s = state();
        let (_id, _rx) = open_approval(&mut s);
        match lower_event(&key(KeyCode::Char('a')), &s) {
            Lowered::Action(Action::Approve { decision: ApprovalDecision::AlwaysAllow { scope }, .. }) => {
                assert_eq!(scope, "rm -rf /");
            }
            _ => panic!("expected AlwaysAllow scoped to the command"),
        }
    }

    #[test]
    fn overlay_n_and_esc_both_deny() {
        let mut s = state();
        open_approval(&mut s);
        assert!(matches!(
            lower_event(&key(KeyCode::Char('n')), &s),
            Lowered::Action(Action::Approve { decision: ApprovalDecision::Deny, .. })
        ));
        assert!(matches!(
            lower_event(&key(KeyCode::Esc), &s),
            Lowered::Action(Action::Approve { decision: ApprovalDecision::Deny, .. })
        ));
    }

    #[test]
    fn overlay_swallows_unrelated_keys() {
        let mut s = state();
        open_approval(&mut s);
        assert!(matches!(
            lower_event(&key(KeyCode::Enter), &s),
            Lowered::Ignore
        ));
        assert!(matches!(
            lower_event(&key(KeyCode::Char('z')), &s),
            Lowered::Ignore
        ));
    }

    fn open_approval_kind(s: &mut AppState, kind: ApprovalKind) -> oneshot::Receiver<ApprovalDecision> {
        let (reply, rx) = oneshot::channel();
        s.overlay = Some(Overlay::Approval(ApprovalRequest { id: Uuid::new_v4(), kind, reply }));
        rx
    }

    fn always_allow_scope(s: &AppState) -> String {
        match lower_event(&key(KeyCode::Char('a')), s) {
            Lowered::Action(Action::Approve { decision: ApprovalDecision::AlwaysAllow { scope }, .. }) => scope,
            _ => panic!("expected AlwaysAllow"),
        }
    }

    #[test]
    fn overlay_a_scopes_file_edit_to_the_diff_path() {
        use stepper_protocol::DiffView;
        let mut s = state();
        let _rx = open_approval_kind(
            &mut s,
            ApprovalKind::FileEdit(DiffView {
                path: PathBuf::from("src/widgets/list.rs"),
                old: "a\n".into(),
                new: "b\n".into(),
            }),
        );
        assert_eq!(always_allow_scope(&s), "src/widgets/list.rs");
    }

    #[test]
    fn overlay_a_scopes_outside_project_to_the_path() {
        let mut s = state();
        let _rx = open_approval_kind(
            &mut s,
            ApprovalKind::OutsideProject {
                path: PathBuf::from("/etc/hosts"),
                action: "read".into(),
            },
        );
        assert_eq!(always_allow_scope(&s), "/etc/hosts");
    }

    #[test]
    fn overlay_a_scopes_mcp_to_server_slash_tool() {
        let mut s = state();
        let _rx = open_approval_kind(
            &mut s,
            ApprovalKind::Mcp { server: "fs".into(), tool: "write".into() },
        );
        assert_eq!(always_allow_scope(&s), "fs/write");
    }

    #[test]
    fn picker_nav_lowers_arrows_enter_and_esc_only() {
        assert_eq!(lower_picker_nav(&key(KeyCode::Up)), Some(PickerNav::Up));
        assert_eq!(lower_picker_nav(&key(KeyCode::Down)), Some(PickerNav::Down));
        assert_eq!(lower_picker_nav(&key(KeyCode::Enter)), Some(PickerNav::Select));
        assert_eq!(lower_picker_nav(&key(KeyCode::Esc)), Some(PickerNav::Cancel));
        assert_eq!(lower_picker_nav(&key(KeyCode::Char('x'))), None);
        assert_eq!(lower_picker_nav(&Event::FocusGained), None);
    }

    #[test]
    fn enter_on_two_line_textarea_submits_value_joined_by_newline() {
        let mut s = state();
        type_text(&mut s, "first line");
        s.textarea.insert_str("\n");
        type_text(&mut s, "second line");
        match lower_event(&key(KeyCode::Enter), &s) {
            Lowered::Action(Action::SubmitInput(t)) => {
                assert_eq!(t, "first line\nsecond line");
                assert!(t.contains('\n'), "the two lines must be joined by a newline");
            }
            _ => panic!("expected SubmitInput carrying the newline-joined buffer"),
        }
    }
}
