//! Placeholder renderer for not-yet-implemented scope tiles.
//!
//! Draws a bordered block with the label centered ("Ports — Phase 4"
//! style). The border is dimmed so users can see at a glance which
//! tiles are stubbed vs live. When focused, a "Phase N — not yet
//! implemented" hint appears below the label.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

pub(crate) fn render(label: &str, eta_phase: u8, frame: &mut Frame<'_>, area: Rect, focused: bool) {
    let (border_color, border_modifier) = if focused {
        (Color::Cyan, Modifier::empty())
    } else {
        (Color::DarkGray, Modifier::DIM)
    };

    let block = Block::default().borders(Borders::ALL).border_style(
        Style::default()
            .fg(border_color)
            .add_modifier(border_modifier),
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            label.to_string(),
            Style::default().fg(Color::Gray),
        ))
        .alignment(Alignment::Center),
    ];
    if focused {
        lines.push(Line::from(""));
        lines.push(
            Line::from(Span::styled(
                format!("Phase {eta_phase} — not yet implemented"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))
            .alignment(Alignment::Center),
        );
    }

    let paragraph = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(paragraph, inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_to_rows(
        label: &str,
        eta_phase: u8,
        area: Rect,
        focused: bool,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(label, eta_phase, f, area, focused))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn renders_label_inside_bordered_block() {
        let rows = render_to_rows("Ports — Phase 4", 4, Rect::new(0, 0, 40, 8), false, 40, 8);
        let joined = rows.join("\n");
        assert!(
            joined.contains("Ports"),
            "label must appear in the tile: {joined}"
        );
        // Top and bottom rows carry border characters.
        assert!(rows[0].starts_with('┌') || rows[0].contains('─'));
        assert!(rows[7].starts_with('└') || rows[7].contains('─'));
    }

    #[test]
    fn focused_renders_not_yet_implemented_hint() {
        let rows = render_to_rows("Ports — Phase 4", 4, Rect::new(0, 0, 50, 10), true, 50, 10);
        let joined = rows.join("\n");
        assert!(
            joined.contains("not yet implemented"),
            "focused placeholder must hint at the eta: {joined}"
        );
        assert!(joined.contains("Phase 4"));
    }

    #[test]
    fn unfocused_does_not_render_hint() {
        let rows = render_to_rows("Ports — Phase 4", 4, Rect::new(0, 0, 50, 10), false, 50, 10);
        let joined = rows.join("\n");
        assert!(
            !joined.contains("not yet implemented"),
            "unfocused placeholder should not show the hint"
        );
    }
}
