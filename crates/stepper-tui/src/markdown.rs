use ratatui::style::Style;
use ratatui::text::Text;
use tui_markdown::{Options, StyleSheet};

/// stepper's markdown house style: the tui-markdown defaults verbatim, minus the
/// `.underlined()` modifier baked into H1 headings and links. Everything else
/// (colors, bold, italic) is preserved so output looks the same just without the
/// underline the user found noisy.
#[derive(Clone, Copy, Debug, Default)]
struct NoUnderlineStyleSheet;

impl StyleSheet for NoUnderlineStyleSheet {
    fn heading(&self, level: u8) -> Style {
        match level {
            1 => Style::new().on_cyan().bold(),
            2 => Style::new().cyan().bold(),
            3 => Style::new().cyan().bold().italic(),
            _ => Style::new().light_cyan().italic(),
        }
    }

    fn code(&self) -> Style {
        Style::new().white().on_black()
    }

    fn link(&self) -> Style {
        Style::new().blue()
    }

    fn blockquote(&self) -> Style {
        Style::new().green()
    }

    fn heading_meta(&self) -> Style {
        Style::new().dim()
    }

    fn metadata_block(&self) -> Style {
        Style::new().light_yellow()
    }
}

/// Render markdown to ratatui `Text` with stepper's house style. The single seam
/// for every markdown sink (live pane + committed scrollback) so they never drift.
pub(crate) fn render_markdown(input: &str) -> Text<'_> {
    tui_markdown::from_str_with_options(input, &Options::new(NoUnderlineStyleSheet))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn h1_and_links_render_without_underline() {
        let text = render_markdown("# Title\n\n[link](https://x)\n");
        let has_underline = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!has_underline, "no span should carry the UNDERLINED modifier");
    }

    #[test]
    fn h1_keeps_bold_and_color() {
        // Sanity: we strip only underline, not the rest of the heading style.
        let style = NoUnderlineStyleSheet.heading(1);
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(!style.add_modifier.contains(Modifier::UNDERLINED));
    }
}
