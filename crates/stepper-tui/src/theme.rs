use ratatui::style::Color;
use std::str::FromStr;

/// Built-in preset names, in the order the editor cycles them. `dark` is the
/// default (terminal-bg-respecting); the rest are common palettes.
pub const PRESET_NAMES: &[&str] = &[
    "dark",
    "light",
    "nord",
    "dracula",
    "gruvbox",
    "solarized",
    "tokyonight",
    "catppuccin",
    "catppuccin-macchiato",
    "onedark",
    "everforest",
    "kanagawa",
    "ayu",
];

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
            "gruvbox" => Theme {
                accent: rgb(0x83, 0xa5, 0x98),
                muted: rgb(0x92, 0x83, 0x74),
                border_active: rgb(0x83, 0xa5, 0x98),
                success: rgb(0xb8, 0xbb, 0x26),
                warning: rgb(0xfa, 0xbd, 0x2f),
                error: rgb(0xfb, 0x49, 0x34),
                diff_added: rgb(0x98, 0x97, 0x1a),
                diff_removed: rgb(0xcc, 0x24, 0x1d),
                gauge_ok: rgb(0xb8, 0xbb, 0x26),
                gauge_warn: rgb(0xfa, 0xbd, 0x2f),
                gauge_crit: rgb(0xfb, 0x49, 0x34),
                layer_colors: [
                    rgb(0x83, 0xa5, 0x98),
                    rgb(0xd3, 0x86, 0x9b),
                    rgb(0xfa, 0xbd, 0x2f),
                    rgb(0xb8, 0xbb, 0x26),
                    rgb(0xfe, 0x80, 0x19),
                ],
            },
            "solarized" => Theme {
                accent: rgb(0x26, 0x8b, 0xd2),
                muted: rgb(0x58, 0x6e, 0x75),
                border_active: rgb(0x26, 0x8b, 0xd2),
                success: rgb(0x85, 0x99, 0x00),
                warning: rgb(0xb5, 0x89, 0x00),
                error: rgb(0xdc, 0x32, 0x2f),
                diff_added: rgb(0x85, 0x99, 0x00),
                diff_removed: rgb(0xdc, 0x32, 0x2f),
                gauge_ok: rgb(0x85, 0x99, 0x00),
                gauge_warn: rgb(0xb5, 0x89, 0x00),
                gauge_crit: rgb(0xdc, 0x32, 0x2f),
                layer_colors: [
                    rgb(0x26, 0x8b, 0xd2),
                    rgb(0x6c, 0x71, 0xc4),
                    rgb(0x2a, 0xa1, 0x98),
                    rgb(0x85, 0x99, 0x00),
                    rgb(0xcb, 0x4b, 0x16),
                ],
            },
            "tokyonight" => Theme {
                accent: rgb(0x82, 0xaa, 0xff),
                muted: rgb(0x82, 0x8b, 0xb8),
                border_active: rgb(0x82, 0xaa, 0xff),
                success: rgb(0xc3, 0xe8, 0x8d),
                warning: rgb(0xff, 0xc7, 0x77),
                error: rgb(0xff, 0x75, 0x7f),
                diff_added: rgb(0xc3, 0xe8, 0x8d),
                diff_removed: rgb(0xff, 0x75, 0x7f),
                gauge_ok: rgb(0xc3, 0xe8, 0x8d),
                gauge_warn: rgb(0xff, 0xc7, 0x77),
                gauge_crit: rgb(0xff, 0x75, 0x7f),
                layer_colors: [
                    rgb(0x82, 0xaa, 0xff),
                    rgb(0xc0, 0x99, 0xff),
                    rgb(0x86, 0xe1, 0xfc),
                    rgb(0xc3, 0xe8, 0x8d),
                    rgb(0xff, 0x96, 0x6c),
                ],
            },
            "catppuccin" => Theme {
                accent: rgb(0x89, 0xb4, 0xfa),
                muted: rgb(0x93, 0x99, 0xb2),
                border_active: rgb(0x89, 0xb4, 0xfa),
                success: rgb(0xa6, 0xe3, 0xa1),
                warning: rgb(0xf9, 0xe2, 0xaf),
                error: rgb(0xf3, 0x8b, 0xa8),
                diff_added: rgb(0xa6, 0xe3, 0xa1),
                diff_removed: rgb(0xf3, 0x8b, 0xa8),
                gauge_ok: rgb(0xa6, 0xe3, 0xa1),
                gauge_warn: rgb(0xf9, 0xe2, 0xaf),
                gauge_crit: rgb(0xf3, 0x8b, 0xa8),
                layer_colors: [
                    rgb(0x89, 0xb4, 0xfa),
                    rgb(0xcb, 0xa6, 0xf7),
                    rgb(0x94, 0xe2, 0xd5),
                    rgb(0xa6, 0xe3, 0xa1),
                    rgb(0xf9, 0xe2, 0xaf),
                ],
            },
            "catppuccin-macchiato" => Theme {
                accent: rgb(0x8a, 0xad, 0xf4),
                muted: rgb(0x93, 0x9a, 0xb7),
                border_active: rgb(0x8a, 0xad, 0xf4),
                success: rgb(0xa6, 0xda, 0x95),
                warning: rgb(0xee, 0xd4, 0x9f),
                error: rgb(0xed, 0x87, 0x96),
                diff_added: rgb(0xa6, 0xda, 0x95),
                diff_removed: rgb(0xed, 0x87, 0x96),
                gauge_ok: rgb(0xa6, 0xda, 0x95),
                gauge_warn: rgb(0xee, 0xd4, 0x9f),
                gauge_crit: rgb(0xed, 0x87, 0x96),
                layer_colors: [
                    rgb(0x8a, 0xad, 0xf4),
                    rgb(0xc6, 0xa0, 0xf6),
                    rgb(0x8b, 0xd5, 0xca),
                    rgb(0xa6, 0xda, 0x95),
                    rgb(0xee, 0xd4, 0x9f),
                ],
            },
            "onedark" => Theme {
                accent: rgb(0x61, 0xaf, 0xef),
                muted: rgb(0x5c, 0x63, 0x70),
                border_active: rgb(0x61, 0xaf, 0xef),
                success: rgb(0x98, 0xc3, 0x79),
                warning: rgb(0xe5, 0xc0, 0x7b),
                error: rgb(0xe0, 0x6c, 0x75),
                diff_added: rgb(0x98, 0xc3, 0x79),
                diff_removed: rgb(0xe0, 0x6c, 0x75),
                gauge_ok: rgb(0x98, 0xc3, 0x79),
                gauge_warn: rgb(0xe5, 0xc0, 0x7b),
                gauge_crit: rgb(0xe0, 0x6c, 0x75),
                layer_colors: [
                    rgb(0x61, 0xaf, 0xef),
                    rgb(0xc6, 0x78, 0xdd),
                    rgb(0x56, 0xb6, 0xc2),
                    rgb(0x98, 0xc3, 0x79),
                    rgb(0xd1, 0x9a, 0x66),
                ],
            },
            "everforest" => Theme {
                accent: rgb(0xa7, 0xc0, 0x80),
                muted: rgb(0x7a, 0x84, 0x78),
                border_active: rgb(0xa7, 0xc0, 0x80),
                success: rgb(0xa7, 0xc0, 0x80),
                warning: rgb(0xdb, 0xbc, 0x7f),
                error: rgb(0xe6, 0x7e, 0x80),
                diff_added: rgb(0xa7, 0xc0, 0x80),
                diff_removed: rgb(0xe6, 0x7e, 0x80),
                gauge_ok: rgb(0xa7, 0xc0, 0x80),
                gauge_warn: rgb(0xdb, 0xbc, 0x7f),
                gauge_crit: rgb(0xe6, 0x7e, 0x80),
                layer_colors: [
                    rgb(0xa7, 0xc0, 0x80),
                    rgb(0x7f, 0xbb, 0xb3),
                    rgb(0x83, 0xc0, 0x92),
                    rgb(0xd6, 0x99, 0xb6),
                    rgb(0xe6, 0x98, 0x75),
                ],
            },
            "kanagawa" => Theme {
                accent: rgb(0x7e, 0x9c, 0xd8),
                muted: rgb(0x72, 0x71, 0x69),
                border_active: rgb(0x7e, 0x9c, 0xd8),
                success: rgb(0x98, 0xbb, 0x6c),
                warning: rgb(0xd7, 0xa6, 0x57),
                error: rgb(0xe8, 0x24, 0x24),
                diff_added: rgb(0x98, 0xbb, 0x6c),
                diff_removed: rgb(0xe8, 0x24, 0x24),
                gauge_ok: rgb(0x98, 0xbb, 0x6c),
                gauge_warn: rgb(0xd7, 0xa6, 0x57),
                gauge_crit: rgb(0xe8, 0x24, 0x24),
                layer_colors: [
                    rgb(0x7e, 0x9c, 0xd8),
                    rgb(0x95, 0x7f, 0xb8),
                    rgb(0x76, 0x94, 0x6a),
                    rgb(0x98, 0xbb, 0x6c),
                    rgb(0xc3, 0x8d, 0x9d),
                ],
            },
            "ayu" => Theme {
                accent: rgb(0x59, 0xc2, 0xff),
                muted: rgb(0x56, 0x5b, 0x66),
                border_active: rgb(0x59, 0xc2, 0xff),
                success: rgb(0x7f, 0xd9, 0x62),
                warning: rgb(0xe6, 0xb6, 0x73),
                error: rgb(0xd9, 0x57, 0x57),
                diff_added: rgb(0x7f, 0xd9, 0x62),
                diff_removed: rgb(0xf2, 0x6d, 0x78),
                gauge_ok: rgb(0x7f, 0xd9, 0x62),
                gauge_warn: rgb(0xe6, 0xb6, 0x73),
                gauge_crit: rgb(0xd9, 0x57, 0x57),
                layer_colors: [
                    rgb(0x59, 0xc2, 0xff),
                    rgb(0xd2, 0xa6, 0xff),
                    rgb(0x39, 0xba, 0xe6),
                    rgb(0x7f, 0xd9, 0x62),
                    rgb(0xff, 0xb4, 0x54),
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
    fn catalog_presets_are_distinct_and_take_overrides() {
        // The added catalog presets each resolve to their own palette (a missing
        // match arm would surface as `None` in the loop above; this guards the
        // colors are actually wired, not just present).
        let dark = Theme::default();
        for name in ["gruvbox", "solarized", "tokyonight", "catppuccin", "ayu"] {
            let t = Theme::preset(name).unwrap();
            assert_ne!(t.accent, dark.accent, "{name} has its own accent");
        }
        assert_eq!(Theme::preset("gruvbox").unwrap().accent, Color::Rgb(0x83, 0xa5, 0x98));
        // Overrides still win over a catalog preset.
        let t = Theme::resolve(Some("tokyonight"), &[("accent".to_string(), "#000000".to_string())]);
        assert_eq!(t.accent, Color::Rgb(0, 0, 0));
        assert_eq!(t.error, Theme::preset("tokyonight").unwrap().error);
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
