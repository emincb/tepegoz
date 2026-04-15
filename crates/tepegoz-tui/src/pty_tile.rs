//! Pty tile renderer: 1-row tab strip + projection of the active
//! pane's [`vt100::Parser`] screen into ratatui cells.
//!
//! The pty bytes from `Event::PaneOutput` / `Event::PaneSnapshot` feed
//! each pane's parser in [`crate::app::App::handle_pane_event`]; this
//! renderer reads the active entry's `screen()` and draws each cell
//! into the buffer position. Cell attributes (fg/bg color, bold,
//! italic, reverse) are translated from vt100's attr types into
//! ratatui's `Style`.
//!
//! Tab strip (Phase 5 Slice 5d-ii): a 1-row strip at the top of the
//! tile's content area renders one labeled slot per pane in
//! `[N label]` form, with a `*` suffix on the active slot.
//! Inactive slots render dimmed; active is terminal-default foreground.
//! Not bright cyan — bright cyan is reserved for tile-focus indication
//! per the existing scope renderer contract. Panes past the 9th collapse
//! into a `[+N]` overflow indicator (the full list-view overlay lives
//! behind the 5e/v1.1 `Ctrl-b w` keybind).
//!
//! Focus styling: the border is bright cyan when the tile is focused,
//! dim gray otherwise. The cursor is rendered as a reversed cell only
//! when the tile is focused — unfocused tiles show their vt100 buffer
//! without a caret, since only the focused tile owns the keystroke
//! stream and a blinking cursor in an unfocused tile is misleading.

use ratatui::Frame;
use ratatui::buffer::Cell as RatCell;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{MAX_TAB_DIGIT_SLOTS, PaneEntry};

pub(crate) fn render(
    panes: &[PaneEntry],
    active: usize,
    frame: &mut Frame<'_>,
    area: Rect,
    focused: bool,
) {
    // Border. Title is fixed ("pty"); Phase 3's OSC 0 behavior (title
    // per attached pane) is orthogonal — that's the *terminal window*
    // title, not this tile's border title.
    let border_color = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let border_modifier = if focused {
        Modifier::empty()
    } else {
        Modifier::DIM
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title("pty")
        .title_style(
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(
            Style::default()
                .fg(border_color)
                .add_modifier(border_modifier),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner into a 1-row tab strip + the vt100 content area.
    // If the inner area is 1 row or less (tiny terminal), skip the
    // strip and use everything for content — the App's resize path
    // already clamped pty dims so this is rare.
    let (strip_rect, content_rect) = if inner.height >= 2 {
        (
            Rect::new(inner.x, inner.y, inner.width, 1),
            Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
        )
    } else {
        (Rect::new(inner.x, inner.y, inner.width, 0), inner)
    };

    if strip_rect.height > 0 {
        render_tab_strip(panes, active, frame, strip_rect);
    }

    let Some(entry) = panes.get(active) else {
        return;
    };

    // Project vt100 cells into the ratatui buffer. The parser's screen
    // dimensions should match `content_rect`'s dimensions (the App resizes
    // every pane's parser on each Resize event); if they drift by one
    // column (e.g. between recomputing the layout and the next SIGWINCH),
    // clip to whichever is smaller so we never index out of bounds.
    let screen = entry.parser.screen();
    let (screen_rows, screen_cols) = screen.size();
    let rows = content_rect.height.min(screen_rows);
    let cols = content_rect.width.min(screen_cols);

    let buf = frame.buffer_mut();
    for row in 0..rows {
        for col in 0..cols {
            let src = match screen.cell(row, col) {
                Some(c) => c,
                None => continue,
            };
            let bx = content_rect.x + col;
            let by = content_rect.y + row;
            if let Some(dst) = buf.cell_mut((bx, by)) {
                write_cell(dst, src);
            }
        }
    }

    // Cursor: show only when focused. Render by reversing the cell
    // at the cursor's (row, col). If the cursor is off-screen (can
    // happen during resize), just skip.
    if focused && !screen.hide_cursor() {
        let (crow, ccol) = screen.cursor_position();
        if crow < rows && ccol < cols {
            let bx = content_rect.x + ccol;
            let by = content_rect.y + crow;
            if let Some(dst) = buf.cell_mut((bx, by)) {
                dst.set_style(dst.style().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

fn render_tab_strip(panes: &[PaneEntry], active: usize, frame: &mut Frame<'_>, area: Rect) {
    let visible_slots = panes.len().min(MAX_TAB_DIGIT_SLOTS);
    let overflow = panes.len().saturating_sub(MAX_TAB_DIGIT_SLOTS);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(visible_slots * 2 + 2);
    for (i, entry) in panes.iter().take(visible_slots).enumerate() {
        let is_active = i == active;
        // 1-indexed — the 10th slot is keybind-only via `Ctrl-b 0`
        // and never gets a visible digit (the full list overlay is
        // reserved for 5e / v1.1 per `docs/ISSUES.md`).
        let digit = i + 1;
        let star = if is_active { "*" } else { "" };
        let label = format!("[{digit} {}{star}]", entry.label);
        let style = if is_active {
            // Terminal default foreground, not bright cyan — bright
            // cyan is reserved for tile-focus indication per the
            // existing scope-renderer contract.
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if overflow > 0 {
        spans.push(Span::styled(
            format!("[+{overflow}]"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Write one vt100 cell's contents + style into a ratatui cell.
fn write_cell(dst: &mut RatCell, src: &vt100::Cell) {
    let contents = src.contents();
    if contents.is_empty() {
        dst.set_symbol(" ");
    } else {
        dst.set_symbol(contents);
    }

    let mut style = Style::default();
    if let Some(color) = convert_color(src.fgcolor()) {
        style = style.fg(color);
    }
    if let Some(color) = convert_color(src.bgcolor()) {
        style = style.bg(color);
    }
    if src.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if src.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if src.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if src.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    dst.set_style(style);
}

fn convert_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use vt100::Parser;

    fn pane(label: &str, parser: Parser) -> PaneEntry {
        PaneEntry {
            pane_id: 1,
            sub_id: 1,
            label: label.to_string(),
            parser,
        }
    }

    fn render_to_rows(panes: &[PaneEntry], active: usize, focused: bool) -> Vec<String> {
        render_to_rows_sized(panes, active, focused, 50, 12)
    }

    fn render_to_rows_sized(
        panes: &[PaneEntry],
        active: usize,
        focused: bool,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(panes, active, f, Rect::new(0, 0, width, height), focused))
            .unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn pty_tile_renders_vt100_screen_contents() {
        // Content area is 10 rows (tile 12 − borders 2 − strip 1).
        let mut parser = Parser::new(9, 48, 100);
        parser.process(b"hello world");
        let panes = vec![pane("zsh", parser)];
        let rows = render_to_rows(&panes, 0, false);
        let joined = rows.join("\n");
        assert!(
            joined.contains("hello world"),
            "pty cells must appear inside the tile: {joined}"
        );
        assert!(rows[0].contains('─'));
        assert!(rows[11].contains('─'));
    }

    #[test]
    fn pty_tile_places_vt100_cursor_when_focused() {
        // vt100 cursor starts at (0, 0). Positioning to (row 2, col 5)
        // via CUP (ESC [ 3 ; 6 H — row/col are 1-indexed in CSI).
        let mut parser = Parser::new(9, 48, 100);
        parser.process(b"\x1b[3;6HX");
        let panes = vec![pane("zsh", parser)];

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(&panes, 0, f, Rect::new(0, 0, 50, 12), true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        // Border top (row 0), tab strip (row 1), vt100 rows start at 2.
        // CUP(3, 6) writes 'X' then cursor advances; the marker sits
        // at content row 2 (vt100 0-indexed row 2) → terminal y = 2+2 = 4.
        // Column 5 (vt100 0-indexed col 5) → terminal x = 1+5 = 6.
        let marker_cell = buffer.cell((6u16, 4u16)).unwrap();
        assert_eq!(
            marker_cell.symbol(),
            "X",
            "vt100 cursor positioning must land the marker at the expected cell"
        );
    }

    #[test]
    fn pty_tile_handles_empty_screen() {
        let parser = Parser::new(9, 48, 100);
        let panes = vec![pane("zsh", parser)];
        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        // Shouldn't panic on an untouched parser.
        terminal
            .draw(|f| render(&panes, 0, f, Rect::new(0, 0, 50, 12), false))
            .unwrap();
    }

    #[test]
    fn tab_strip_renders_slot_numbers_and_labels() {
        let panes = vec![
            pane("zsh", Parser::new(9, 48, 100)),
            pane("ssh:staging", Parser::new(9, 48, 100)),
        ];
        let rows = render_to_rows(&panes, 0, true);
        // Row 1 is the tab strip (row 0 is top border).
        assert!(
            rows[1].contains("[1 zsh*]"),
            "active pane marked with *; got: {}",
            rows[1]
        );
        assert!(
            rows[1].contains("[2 ssh:staging]"),
            "inactive pane without *; got: {}",
            rows[1]
        );
    }

    #[test]
    fn tab_strip_moves_star_to_active_slot() {
        let panes = vec![
            pane("zsh", Parser::new(9, 48, 100)),
            pane("ssh:staging", Parser::new(9, 48, 100)),
        ];
        let rows = render_to_rows(&panes, 1, true);
        assert!(rows[1].contains("[1 zsh]"), "inactive got: {}", rows[1]);
        assert!(
            rows[1].contains("[2 ssh:staging*]"),
            "active got: {}",
            rows[1]
        );
    }

    #[test]
    fn tab_strip_shows_overflow_indicator_beyond_nine_panes() {
        // 9 tabs × 7 chars each ≈ 63 chars; add overflow + margin —
        // a 120-wide tile fits everything legibly.
        let panes: Vec<PaneEntry> = (0..12)
            .map(|i| pane(&format!("p{i}"), Parser::new(9, 118, 100)))
            .collect();
        let rows = render_to_rows_sized(&panes, 0, true, 120, 12);
        assert!(
            rows[1].contains("[+3]"),
            "12 panes should show [+3] overflow; got: {}",
            rows[1]
        );
        assert!(
            rows[1].contains("[1 p0*]"),
            "first pane still rendered: {}",
            rows[1]
        );
    }

    #[test]
    fn tab_strip_caps_at_nine_slots_plus_overflow() {
        // CTO spec: "the strip shows the first 9 as numbered tabs +
        // a [+N] overflow indicator" when there are more than 9
        // panes. The 10th pane is still reachable via the `Ctrl-b 0`
        // keybind (pinned in an App-level state-machine test), but
        // it doesn't get its own visible digit slot — the overlay
        // for listing >9 panes is deferred to 5e/v1.1.
        let panes: Vec<PaneEntry> = (0..10)
            .map(|i| pane(&format!("p{i}"), Parser::new(9, 118, 100)))
            .collect();
        let rows = render_to_rows_sized(&panes, 0, true, 120, 12);
        assert!(
            rows[1].contains("[+1]"),
            "10 panes still show [+1] overflow with 9 numbered slots; got: {}",
            rows[1]
        );
        for digit in 1..=9 {
            assert!(
                rows[1].contains(&format!("[{digit} p{}", digit - 1)),
                "slot {digit} should render; got: {}",
                rows[1]
            );
        }
        assert!(
            !rows[1].contains("[0 p"),
            "digit 0 is keybind-only; got: {}",
            rows[1]
        );
    }
}
