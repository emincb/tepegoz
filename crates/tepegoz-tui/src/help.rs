//! Help overlay renderer.
//!
//! Slice 6.0 promoted `Ctrl-b ?` from a reserved-but-no-op keybind to
//! a toggled modal that lists the documented keyboard surface. The
//! overlay is the authoritative reference for the post-6.0 five-
//! binding surface (Tab / Shift-Tab tile focus, arrows & j/k row nav,
//! Enter primary action, Esc cancel, Ctrl-b d detach) plus the
//! mouse-first interaction. Per-tile intra-keybinds also live here
//! because every scope tile's inline help bar is narrower than its
//! full keybind map and necessarily abbreviates.
//!
//! Input while the overlay is visible is absorbed as a dismissal
//! gesture in `App::handle_stdin` — any keypress (or any click, via
//! `handle_mouse_click`) closes the overlay without reaching the
//! underlying tile. The only exception is `Ctrl-b d` (detach), which
//! is preserved as an escape hatch.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

/// Rendered size of the overlay. Kept modest so it doesn't swallow
/// the full tile grid (the user may still want to read the dimmed
/// state underneath). Shrinks if the terminal is smaller.
const HELP_OVERLAY_WIDTH: u16 = 60;
const HELP_OVERLAY_HEIGHT: u16 = 20;

pub(crate) fn render(frame: &mut Frame<'_>) {
    let area = frame.area();
    let width = area.width.min(HELP_OVERLAY_WIDTH);
    let height = area.height.min(HELP_OVERLAY_HEIGHT);
    if width < 20 || height < 8 {
        // Terminal is too small for anything useful — render a 1-line
        // hint at the top rather than crashing the layout calc below.
        // Matches the too-small tile fallback's tone.
        let hint = Paragraph::new(Line::from(Span::styled(
            "help: Ctrl-b ? again to close",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(hint, Rect::new(area.x, area.y, area.width, 1));
        return;
    }
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let rect = Rect::new(x, y, width, height);

    // Clear the backing cells so the overlay reads cleanly over
    // whatever tile renderers drew into the buffer first.
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help · Ctrl-b ? toggles ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let heading_style = Style::default().add_modifier(Modifier::BOLD);
    let dim_style = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(" tile focus", heading_style)),
        Line::from("   Tab · Shift-Tab     cycle tile focus (PTY → shell)"),
        Line::from("   mouse click         focus tile · select row"),
        Line::from(""),
        Line::from(Span::styled(" inside a focused scope tile", heading_style)),
        Line::from("   j · k · ↓ · ↑       navigate rows"),
        Line::from("   Enter               primary action on selected row"),
        Line::from("   /                   start filter"),
        Line::from("   Esc                 cancel / back"),
        Line::from(""),
        Line::from(Span::styled(" session", heading_style)),
        Line::from("   Ctrl-b d            detach"),
        Line::from("   Ctrl-b ?            toggle this help"),
        Line::from(""),
        Line::from(Span::styled(
            " any key or click dismisses the overlay",
            dim_style.add_modifier(Modifier::ITALIC),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_frame(width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(render).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn renders_five_core_bindings_in_overlay_body() {
        let rows = render_frame(120, 40);
        let joined = rows.join("\n");
        assert!(joined.contains("Tab"), "Tab binding must appear: {joined}");
        assert!(
            joined.contains("Shift-Tab"),
            "Shift-Tab binding must appear"
        );
        assert!(
            joined.contains("j") && joined.contains("k"),
            "j/k row nav must appear"
        );
        assert!(joined.contains("Enter"), "Enter binding must appear");
        assert!(joined.contains("Esc"), "Esc binding must appear");
        assert!(joined.contains("Ctrl-b d"), "Ctrl-b d (detach) must appear");
        assert!(
            joined.contains("Ctrl-b ?"),
            "Ctrl-b ? (toggle self) must appear"
        );
        assert!(
            joined.contains("PTY → shell"),
            "Slice 6.0.1 carve-out (Tab forwards to shell on PTY focus) must be documented: {joined}"
        );
    }

    #[test]
    fn renders_dismissal_hint_at_bottom() {
        let rows = render_frame(120, 40);
        let joined = rows.join("\n");
        assert!(
            joined.contains("dismisses the overlay"),
            "overlay must hint at dismissal: {joined}"
        );
    }

    #[test]
    fn too_small_terminal_falls_back_to_hint_line_without_crash() {
        let rows = render_frame(30, 6);
        let joined = rows.join("\n");
        assert!(
            joined.contains("help: Ctrl-b ? again to close"),
            "fallback hint must render on tiny terminals: {joined}"
        );
    }
}
