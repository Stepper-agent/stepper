use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Auto,
    Plan,
    AcceptEdits,
    /// Read-only tools run without prompting; everything else asks.
    Default,
    /// Anything that would ask is auto-denied (CI-safe headless posture).
    DontAsk,
    /// Anything that would ask is allowed; explicit deny rules still deny.
    /// Reachable only via `--dangerously-skip-permissions`, never the cycle.
    Bypass,
}

impl Mode {
    /// The Shift+Tab cycle. `DontAsk`/`Bypass` are never cycled *into* (they are
    /// explicit opt-ins); cycling out of them lands on the safe `Default`.
    pub fn next(self) -> Self {
        match self {
            Mode::Auto => Mode::Plan,
            Mode::Plan => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Default,
            Mode::Default => Mode::Auto,
            Mode::DontAsk => Mode::Default,
            Mode::Bypass => Mode::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Plan => "plan",
            Mode::AcceptEdits => "accept-edits",
            Mode::Default => "default",
            Mode::DontAsk => "dont-ask",
            Mode::Bypass => "bypass",
        }
    }
}
