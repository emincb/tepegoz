//! Toast overlay strip rendered on top of the tile grid.
//!
//! Per C3a UX clarification #2: toasts appear as a 1-line-per-toast
//! strip at the bottom of the scope row (above the Claude Code tile,
//! below Docker / Ports / Fleet content). Max [`MAX_TOASTS`] visible
//! at once; a fourth arrival drops the oldest silently. Render does
//! not block keystrokes — the App's input routing is unchanged while
//! toasts are on screen.
//!
//! Durations, drop-oldest, and `Instant`-based expiry all live in
//! [`crate::app::App::push_toast`] / `sweep_expired`. This module is
//! pure presentation.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::app::{MAX_TOASTS, Toast, ToastKind};
use crate::tile::{TileId, TileLayout};

/// Paint the toast overlay directly above the Claude Code tile. No-op
/// when `toasts` is empty, and quietly no-ops if the layout has no
/// Claude Code tile (e.g. `TooSmall` fallback).
pub(crate) fn render(toasts: &[Toast], layout: &TileLayout, frame: &mut Frame<'_>) {
    if toasts.is_empty() {
        return;
    }
    let Some(anchor) = layout.rect_of(TileId::ClaudeCode) else {
        return;
    };
    let height = toasts.len().min(MAX_TOASTS) as u16;
    if height == 0 || anchor.y == 0 {
        return;
    }
    let y = anchor.y.saturating_sub(height);
    let area = Rect::new(anchor.x, y, anchor.width, height);
    frame.render_widget(Clear, area);

    let lines: Vec<Line> = toasts
        .iter()
        .rev()
        .take(MAX_TOASTS)
        .rev()
        .map(|t| render_line(t))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_line(toast: &Toast) -> Line<'_> {
    let (color, prefix) = match toast.kind {
        ToastKind::Success => (Color::Green, "ok:"),
        ToastKind::Error => (Color::Red, "err:"),
        ToastKind::Info => (Color::Cyan, "info:"),
    };
    Line::from(vec![
        Span::styled(
            format!(" {prefix} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(toast.message.as_str(), Style::default().fg(color)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{MAX_TOASTS, Toast, ToastKind};
    use crate::tile::TileLayout;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use std::time::{Duration, Instant};

    fn toast(kind: ToastKind, msg: &str) -> Toast {
        Toast {
            kind,
            message: msg.to_string(),
            expires_at: Instant::now() + Duration::from_secs(3),
        }
    }

    fn render_full(toasts: &[Toast]) -> Vec<String> {
        let area = Rect::new(0, 0, 120, 40);
        let layout = TileLayout::default_for(area);
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(toasts, &layout, frame))
            .unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        (0..buf.area.height as usize)
            .map(|row| {
                let start = row * w;
                (0..w)
                    .map(|col| buf.content[start + col].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn empty_toast_list_renders_nothing() {
        let rows = render_full(&[]);
        // Every row should be entirely whitespace (no Clear, no
        // content written).
        assert!(
            rows.iter().all(|r| r.trim().is_empty()),
            "empty toast list should paint no cells; got {rows:?}"
        );
    }

    #[test]
    fn single_error_toast_renders_above_claude_code() {
        let rows = render_full(&[toast(
            ToastKind::Error,
            "Restart nginx failed: container not running",
        )]);
        let joined = rows.join("\n");
        assert!(
            joined.contains("err:"),
            "error toast must show 'err:' prefix; got {joined}"
        );
        assert!(
            joined.contains("container not running"),
            "error message must render verbatim; got {joined}"
        );
        // Claude Code strip is 3 rows at the bottom; the toast strip
        // sits directly above it. For a 40-tall terminal, Claude Code
        // occupies rows 37/38/39 and the 1-line toast strip lands on
        // row 36.
        assert!(
            rows[36].contains("err:"),
            "toast must land on row 36 (directly above the Claude Code strip); got row {:?}",
            rows[36]
        );
    }

    #[test]
    fn three_toasts_render_three_lines() {
        let rows = render_full(&[
            toast(ToastKind::Success, "Restart nginx — succeeded"),
            toast(ToastKind::Error, "Stop db failed: engine unavailable"),
            toast(ToastKind::Success, "Kill sidecar — succeeded"),
        ]);
        // Rows 34, 35, 36 should each contain a toast (3 lines stacked
        // above Claude Code starting at row 37).
        assert!(rows[34].contains("Restart nginx"), "row 34: {:?}", rows[34]);
        assert!(rows[35].contains("Stop db"), "row 35: {:?}", rows[35]);
        assert!(rows[36].contains("Kill sidecar"), "row 36: {:?}", rows[36]);
    }

    #[test]
    fn more_than_max_caps_at_max_visible() {
        // Four toasts — only the top MAX_TOASTS should be rendered.
        // (The App's push_toast drops-oldest into the queue before we
        // get here; this test verifies the renderer is defensive even
        // if somehow given more.)
        let mut toasts: Vec<Toast> = Vec::new();
        for i in 0..MAX_TOASTS + 2 {
            toasts.push(toast(ToastKind::Success, &format!("msg-{i}")));
        }
        let rows = render_full(&toasts);
        let overlay_area: String = rows[(40 - 3 - MAX_TOASTS)..(40 - 3)].join("\n");
        // The renderer keeps the newest MAX_TOASTS (i.e. the last
        // MAX_TOASTS entries); older ones should not be on screen.
        for i in 0..(toasts.len() - MAX_TOASTS) {
            assert!(
                !overlay_area.contains(&format!("msg-{i}")),
                "oldest toasts past the cap must not render; saw msg-{i} in {overlay_area}"
            );
        }
        for i in (toasts.len() - MAX_TOASTS)..toasts.len() {
            assert!(
                overlay_area.contains(&format!("msg-{i}")),
                "newest toasts must render; missing msg-{i} from {overlay_area}"
            );
        }
    }

    #[test]
    fn too_small_layout_no_ops_rather_than_panic() {
        // Layout without a Claude Code tile (tiny fallback): render
        // should silently do nothing instead of panicking.
        let area = Rect::new(0, 0, 60, 20);
        let layout = TileLayout::default_for(area);
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(&[toast(ToastKind::Error, "x")], &layout, frame))
            .unwrap();
    }
}
