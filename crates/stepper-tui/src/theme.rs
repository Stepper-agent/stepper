use ratatui::style::Color;
use std::str::FromStr;

/// Built-in preset names, in the order the editor cycles them. `dark` is the
/// default (terminal-bg-respecting); the rest are common palettes.
pub const PRESET_NAMES: &[&str] = &["dark", "light", "nord", "dracula"];

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
    /// A named preset, or `None` for an unknown name. `dark` is the default.
    pub fn preset(name: &str) -> Option<Theme> {
        let rgb = Color::Rgb;
        let dark_layers = Theme::default().layer_colors;
        Some(match name {
            "dark" => Theme::default(),
            "light" => Theme {
                accent: rgb(0x00, 0x5f, 0xd7),
                muted: rgb(0x9e, 0x9e, 0x9e),
                border_active: rgb(0x00, 0x5f, 0xd7),
                success: rgb(0x00, 0x87, 0x00),
                warning: rgb(0xaf, 0x87, 0x00),
                error: rgb(0xd7, 0x00, 0x00),
                diff_added: rgb(0x00, 0x87, 0x00),
                diff_removed: rgb(0xd7, 0x00, 0x00),
                gauge_ok: rgb(0x00, 0x87, 0x00),
                gauge_warn: rgb(0xaf, 0x87, 0x00),
                gauge_crit: rgb(0xd7, 0x00, 0x00),
                layer_colors: dark_layers,
            },
            "nord" => Theme {
                accent: rgb(0x88, 0xc0, 0xd0),
                muted: rgb(0x4c, 0x56, 0x6a),
                border_active: rgb(0x88, 0xc0, 0xd0),
                success: rgb(0xa3, 0xbe, 0x8c),
                warning: rgb(0xeb, 0xcb, 0x8b),
                error: rgb(0xbf, 0x61, 0x6a),
                diff_added: rgb(0xa3, 0xbe, 0x8c),
                diff_removed: rgb(0xbf, 0x61, 0x6a),
                gauge_ok: rgb(0xa3, 0xbe, 0x8c),
                gauge_warn: rgb(0xeb, 0xcb, 0x8b),
                gauge_crit: rgb(0xbf, 0x61, 0x6a),
                layer_colors: [
                    rgb(0x88, 0xc0, 0xd0),
                    rgb(0xb4, 0x8e, 0xad),
                    rgb(0x81, 0xa1, 0xc1),
                    rgb(0xa3, 0xbe, 0x8c),
                    rgb(0xeb, 0xcb, 0x8b),
                ],
            },
            "dracula" => Theme {
                accent: rgb(0xbd, 0x93, 0xf8),
                muted: rgb(0x62, 0x72, 0xa4),
                border_active: rgb(0xbd, 0x93, 0xf8),
                success: rgb(0x50, 0xfa, 0x7b),
                warning: rgb(0xf1, 0xfa, 0x8c),
                error: rgb(0xff, 0x55, 0x55),
                diff_added: rgb(0x50, 0xfa, 0x7b),
                diff_removed: rgb(0xff, 0x55, 0x55),
                gauge_ok: rgb(0x50, 0xfa, 0x7b),
                gauge_warn: rgb(0xf1, 0xfa, 0x8c),
                gauge_crit: rgb(0xff, 0x55, 0x55),
                layer_colors: [
                    rgb(0xbd, 0x93, 0xf8),
                    rgb(0xff, 0x79, 0xc6),
                    rgb(0x8b, 0xe9, 0xfd),
                    rgb(0x50, 0xfa, 0x7b),
                    rgb(0xf1, 0xfa, 0x8c),
                ],
            },
            _ => return None,
        })
    }

    /// The editable color roles in display order: `(name, current value)`. Layer
    /// colors are intentionally not editable (the cycle is derived).
    pub fn color_fields(&self) -> [(&'static str, Color); 11] {
        [
            ("accent", self.accent),
            ("muted", self.muted),
            ("border_active", self.border_active),
            ("success", self.success),
            ("warning", self.warning),
            ("error", self.error),
            ("diff_added", self.diff_added),
            ("diff_removed", self.diff_removed),
            ("gauge_ok", self.gauge_ok),
            ("gauge_warn", self.gauge_warn),
            ("gauge_crit", self.gauge_crit),
        ]
    }

    /// Set a color role by name (no-op for an unknown name).
    pub fn set_color(&mut self, name: &str, c: Color) {
        match name {
            "accent" => self.accent = c,
            "muted" => self.muted = c,
            "border_active" => self.border_active = c,
            "success" => self.success = c,
            "warning" => self.warning = c,
            "error" => self.error = c,
            "diff_added" => self.diff_added = c,
            "diff_removed" => self.diff_removed = c,
            "gauge_ok" => self.gauge_ok = c,
            "gauge_warn" => self.gauge_warn = c,
            "gauge_crit" => self.gauge_crit = c,
            _ => {}
        }
    }

    /// Parse a color string: `#RRGGBB`, a named color (e.g. `cyan`), or a 0–255
    /// palette index (ratatui's `Color::from_str` grammar).
    pub fn parse_color(s: &str) -> Option<Color> {
        Color::from_str(s.trim()).ok()
    }

    /// Serialize a color for persistence (`#RRGGBB` for rgb, the name otherwise).
    pub fn color_to_string(c: Color) -> String {
        c.to_string()
    }

    /// Build a theme from a preset (default `dark`) plus per-color overrides
    /// `(role_name, color_string)`; unparseable/unknown entries are ignored.
    pub fn resolve(preset: Option<&str>, overrides: &[(String, String)]) -> Theme {
        let mut theme = preset.and_then(Theme::preset).unwrap_or_default();
        for (name, value) in overrides {
            if let Some(c) = Theme::parse_color(value) {
                theme.set_color(name, c);
            }
        }
        theme
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_and_unknown_is_none() {
        for name in PRESET_NAMES {
            assert!(Theme::preset(name).is_some(), "preset {name} exists");
        }
        assert!(Theme::preset("bogus").is_none());
    }

    #[test]
    fn color_parse_and_serialize_round_trip() {
        let c = Theme::parse_color("#bd93f8").unwrap();
        assert_eq!(c, Color::Rgb(0xbd, 0x93, 0xf8));
        let s = Theme::color_to_string(c);
        assert_eq!(Theme::parse_color(&s), Some(c), "display → parse round-trips");
        assert!(Theme::parse_color("definitely-not-a-color").is_none());
    }

    #[test]
    fn resolve_applies_overrides_over_the_preset() {
        let t = Theme::resolve(Some("dracula"), &[("accent".to_string(), "#ff0000".to_string())]);
        assert_eq!(t.accent, Color::Rgb(0xff, 0, 0), "override wins");
        assert_eq!(t.error, Theme::preset("dracula").unwrap().error, "un-overridden keeps the preset");
        // An unparseable override is ignored (keeps the preset color).
        let t2 = Theme::resolve(Some("nord"), &[("accent".to_string(), "zzz".to_string())]);
        assert_eq!(t2.accent, Theme::preset("nord").unwrap().accent);
    }
}
