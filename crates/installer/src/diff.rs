//! Render a unified line diff between two config texts as styled TUI lines.

use ratatui::style::{Color, Style};
use ratatui::text::Line;
use similar::{ChangeTag, TextDiff};

/// A red/green/grey line diff of `old` → `new`, for the review screen.
pub fn diff_lines(old: &str, new: &str) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(old, new);
    let mut lines = Vec::new();
    for change in diff.iter_all_changes() {
        let (sign, color) = match change.tag() {
            ChangeTag::Delete => ('-', Color::Red),
            ChangeTag::Insert => ('+', Color::Green),
            ChangeTag::Equal => (' ', Color::DarkGray),
        };
        let text = change.value().trim_end_matches('\n');
        lines.push(Line::styled(
            format!("{sign} {text}"),
            Style::default().fg(color),
        ));
    }
    lines
}
