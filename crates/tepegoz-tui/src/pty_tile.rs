//! Pty tile renderer: projects a [`vt100::Parser`]'s screen buffer
//! into ratatui cells within the tile's `Rect`.
//!
//! The pty bytes from `Event::PaneOutput` / `Event::PaneSnapshot` feed
//! the parser in [`crate::app::App::handle_pane_event`]; this renderer
//! reads the parser's `screen()` and draws each cell into the
//! corresponding buffer position. Cell attributes (fg/bg color, bold,
//! italic, reverse) are translated from vt100's attr types into
//! ratatui's `Style`.
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
use ratatui::widgets::{Block, Borders};
use vt100::Parser;

pub(crate) fn render(parser: &Parser, frame: &mut Frame<'_>, area: Rect, focused: bool) {
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

    // Project vt100 cells into the ratatui buffer. The parser's screen
    // dimensions should match `inner`'s dimensions (the App resizes
    // the parser on each Resize event); if they drift by one column
    // (e.g. between recomputing the layout and the next SIGWINCH),
    // clip to whichever is smaller so we never index out of bounds.
    let screen = parser.screen();
    let (screen_rows, screen_cols) = screen.size();
    let rows = inner.height.min(screen_rows);
    let cols = inner.width.min(screen_cols);

    let buf = frame.buffer_mut();
    for row in 0..rows {
        for col in 0..cols {
            let src = match screen.cell(row, col) {
                Some(c) => c,
                None => continue,
            };
            let bx = inner.x + col;
            let by = inner.y + row;
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
            let bx = inner.x + ccol;
            let by = inner.y + crow;
            if let Some(dst) = buf.cell_mut((bx, by)) {
                dst.set_style(dst.style().add_modifier(Modifier::REVERSED));
            }
        }
    }
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

    #[test]
    fn pty_tile_renders_vt100_screen_contents() {
        let mut parser = Parser::new(10, 40, 100);
        parser.process(b"hello world");

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(&parser, f, Rect::new(0, 0, 50, 12), false))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = buffer
            .content()
            .chunks(50)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect();
        let joined = rows.join("\n");
        assert!(
            joined.contains("hello world"),
            "pty cells must appear inside the tile: {joined}"
        );
        // Border top + bottom.
        assert!(rows[0].contains('─'));
        assert!(rows[11].contains('─'));
    }

    #[test]
    fn pty_tile_places_vt100_cursor_when_focused() {
        // vt100 cursor starts at (0, 0). Positioning to (row 2, col 5)
        // via CUP (ESC [ 3 ; 6 H — row/col are 1-indexed in CSI).
        let mut parser = Parser::new(10, 40, 100);
        parser.process(b"\x1b[3;6HX");

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render(&parser, f, Rect::new(0, 0, 50, 12), true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        // After the marker 'X' the cursor advances to (2, 6). The
        // tile border occupies (0,0)-(0,49) on top and column 0 on
        // the left, so the marker sits at terminal cell (1+5, 1+2) =
        // (col=6, row=3).
        let marker_cell = buffer.cell((6u16, 3u16)).unwrap();
        assert_eq!(
            marker_cell.symbol(),
            "X",
            "vt100 cursor positioning must land the marker at the expected cell"
        );
    }

    #[test]
    fn pty_tile_handles_empty_screen() {
        let parser = Parser::new(10, 40, 100);
        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        // Shouldn't panic on an untouched parser.
        terminal
            .draw(|f| render(&parser, f, Rect::new(0, 0, 50, 12), false))
            .unwrap();
    }
}
