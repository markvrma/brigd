//! Text rendering for read-only tabs: markdown via tui-markdown and colored unified diffs.

use super::theme::pal;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};

/// Unified diff → colored lines, one per source line: + green, - red, @@ cyan.
pub(super) fn diff_lines(text: &str) -> Vec<Line<'_>> {
    text.lines()
        .map(|l| match l.as_bytes().first() {
            Some(b'+') => Line::styled(l, Style::new().fg(pal().green)),
            Some(b'-') => Line::styled(l, Style::new().fg(pal().red)),
            Some(b'@') if l.starts_with("@@") => {
                Line::styled(l, Style::new().fg(pal().sky).add_modifier(Modifier::DIM))
            }
            _ => Line::raw(l),
        })
        .collect()
}

/// tui-markdown styles from the active `pal()` theme; unlisted ones keep the crate defaults.
#[derive(Clone)]
struct MdStyle;

impl tui_markdown::StyleSheet for MdStyle {
    fn heading(&self, level: u8) -> Style {
        let c = [pal().mauve, pal().sky, pal().blue][(level.max(1) - 1).min(2) as usize];
        Style::new().fg(c).add_modifier(Modifier::BOLD)
    }
    fn heading_marker(&self, _: u8) -> &str {
        ""
    }
    fn code_block_fence(&self) -> &str {
        ""
    }
    fn code(&self) -> Style {
        Style::new().fg(pal().peach).bg(pal().surface0)
    }
    fn blockquote(&self) -> Style {
        Style::new()
            .fg(pal().subtext0)
            .add_modifier(Modifier::ITALIC)
    }
    fn table_border(&self) -> Style {
        Style::new().fg(pal().overlay0)
    }
    fn table_header(&self) -> Style {
        Style::new().fg(pal().text).add_modifier(Modifier::BOLD)
    }
    fn link(&self) -> Style {
        Style::new()
            .fg(pal().sky)
            .add_modifier(Modifier::UNDERLINED)
    }
    fn list_marker(&self) -> Style {
        Style::new().fg(pal().yellow)
    }
    fn metadata_block(&self) -> Style {
        Style::new().fg(pal().overlay0)
    }
    // Math parsing is always on; keep `$5 and $10` plain.
    fn math_inline(&self) -> Style {
        Style::new()
    }
    fn math_display(&self) -> Style {
        Style::new()
    }
}

/// Markdown → styled text wrapped for `width` columns; tables are fitted to it.
/// ponytail: re-rendered every frame (~0.3ms typical, 12ms for 150KB in release); cache by (path, width, text len) if big files lag.
pub(super) fn md_text(text: &str, width: u16) -> Text<'_> {
    let mut t = tui_markdown::from_str_with_options(
        text,
        &tui_markdown::Options::new(MdStyle).table_width(width),
    );
    for l in &mut t.lines {
        // A thematic break comes out as a bare unstyled `---`; code and front matter lines are styled.
        if l.style == Style::default()
            && l.spans.iter().all(|s| s.style == Style::default())
            && l.spans.iter().map(|s| &*s.content).collect::<String>() == "---"
        {
            *l = Line::styled("─".repeat(width as usize), Style::new().fg(pal().overlay0));
        }
    }
    t
}
