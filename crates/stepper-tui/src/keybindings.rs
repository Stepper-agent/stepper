//! User-customizable key bindings (`~/.stepper/keybindings.json`).
//!
//! Design: **additive**. Each entry binds an extra key to an editor action; the
//! built-in defaults (Enter=submit, Ctrl+C=quit, Esc=interrupt, Shift+Enter=
//! newline, Ctrl+E=editor, …) always keep working. So a binding can only *add* a
//! key, never break a load-bearing one — zero regression risk for the delicate
//! input layer. `lower_event` consults these overrides first.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// An editor action a key can be bound to. Only non-destructive, single-chord
/// actions are bindable; submit/quit/interrupt stay hardwired.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BindableAction {
    Newline,
    CycleMode,
    ExternalEditor,
    HistorySearch,
    ScrollUp,
    ScrollDown,
}

impl BindableAction {
    /// The config key name for this action.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "newline" => Self::Newline,
            "cycle-mode" => Self::CycleMode,
            "external-editor" | "editor" => Self::ExternalEditor,
            "history-search" => Self::HistorySearch,
            "scroll-up" => Self::ScrollUp,
            "scroll-down" => Self::ScrollDown,
            _ => return None,
        })
    }
}

/// An exact key chord: a base key plus the modifiers that must be held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Chord {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl Chord {
    /// Parse a chord like `ctrl+e`, `shift+enter`, `alt+r`, `pageup`. Returns
    /// `None` for an unrecognized key token.
    fn parse(s: &str) -> Option<Chord> {
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        let mut code = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()).filter(|p| !p.is_empty()) {
            match part.as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" | "option" | "meta" => alt = true,
                "shift" => shift = true,
                token => code = Some(parse_code(token)?),
            }
        }
        code.map(|code| Chord { code, ctrl, alt, shift })
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        key.code == self.code
            && key.modifiers.contains(KeyModifiers::CONTROL) == self.ctrl
            && key.modifiers.contains(KeyModifiers::ALT) == self.alt
            && key.modifiers.contains(KeyModifiers::SHIFT) == self.shift
    }
}

/// Map a key-token to a `KeyCode` (named keys + single characters).
fn parse_code(token: &str) -> Option<KeyCode> {
    Some(match token {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pagedn" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        s if s.chars().count() == 1 => KeyCode::Char(s.chars().next().unwrap()),
        _ => return None,
    })
}

/// The user's extra key bindings, applied on top of the built-in defaults.
#[derive(Default)]
pub struct KeyBindings {
    extra: Vec<(Chord, BindableAction)>,
}

impl KeyBindings {
    /// Build from `(action-name, chord-string)` overrides (already read from the
    /// keybindings.json files by the CLI). Unparseable entries are skipped.
    pub fn from_overrides(overrides: &[(String, String)]) -> Self {
        let extra = overrides
            .iter()
            .filter_map(|(name, chord)| {
                Some((Chord::parse(chord)?, BindableAction::from_name(name)?))
            })
            .collect();
        KeyBindings { extra }
    }

    /// The action a user-bound chord maps this key to, if any. Defaults are not
    /// here — they stay in `lower_event`'s built-in handling.
    pub fn action_for(&self, key: &KeyEvent) -> Option<BindableAction> {
        self.extra.iter().find(|(chord, _)| chord.matches(key)).map(|(_, action)| *action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parses_modifiers_and_named_keys() {
        let c = Chord::parse("ctrl+e").unwrap();
        assert!(c.ctrl && !c.alt && !c.shift && c.code == KeyCode::Char('e'));
        assert!(Chord::parse("shift+enter").unwrap().matches(&key(KeyCode::Enter, KeyModifiers::SHIFT)));
        assert!(Chord::parse("pageup").unwrap().matches(&key(KeyCode::PageUp, KeyModifiers::NONE)));
        assert!(Chord::parse("nonsense-key").is_none());
    }

    #[test]
    fn action_for_matches_only_the_bound_chord() {
        let kb = KeyBindings::from_overrides(&[
            ("external-editor".into(), "ctrl+t".into()),
            ("scroll-up".into(), "alt+k".into()),
            ("bogus-action".into(), "ctrl+z".into()), // skipped
        ]);
        assert_eq!(
            kb.action_for(&key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(BindableAction::ExternalEditor)
        );
        assert_eq!(
            kb.action_for(&key(KeyCode::Char('k'), KeyModifiers::ALT)),
            Some(BindableAction::ScrollUp)
        );
        // An unbound key, and the skipped bogus action, match nothing.
        assert_eq!(kb.action_for(&key(KeyCode::Char('z'), KeyModifiers::CONTROL)), None);
        assert_eq!(kb.action_for(&key(KeyCode::Char('t'), KeyModifiers::NONE)), None);
    }
}
