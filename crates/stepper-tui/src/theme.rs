use ratatui::style::Color;

/// Color-role table (opencode-style). Defined once and passed to widgets so a
/// future `--theme` flag is trivial. Does NOT set a background — we respect the
/// terminal's own bg (opencode `system`/`none` behavior) in an inline viewport.
#[derive(Clone)]
pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    pub border_active: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    pub gauge_ok: Color,
    pub gauge_warn: Color,
    pub gauge_crit: Color,
    layer_colors: [Color; 5],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            muted: Color::DarkGray,
            border_active: Color::Cyan,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            diff_added: Color::Green,
            diff_removed: Color::Red,
            gauge_ok: Color::Green,
            gauge_warn: Color::Yellow,
            gauge_crit: Color::Red,
            layer_colors: [
                Color::Cyan,
                Color::Magenta,
                Color::Blue,
                Color::Green,
                Color::Yellow,
            ],
        }
    }
}

impl Theme {
    /// Per-layer accent color (echoes opencode's per-agent color).
    pub fn layer_color(&self, index: usize) -> Color {
        self.layer_colors[index % self.layer_colors.len()]
    }

    /// Context gauge color: lots of room = green, getting low = yellow, near the
    /// 95% auto-compaction line = red. `pct_left` is percent of context still free.
    pub fn gauge_color(&self, pct_left: u8) -> Color {
        if pct_left <= 5 {
            self.gauge_crit
        } else if pct_left <= 25 {
            self.gauge_warn
        } else {
            self.gauge_ok
        }
    }
}
