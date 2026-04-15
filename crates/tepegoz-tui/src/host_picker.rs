//! Host picker modal (Phase 6 Slice 6c-iii).
//!
//! Opens on `Ctrl-b t` (with a target-capable scope tile focused) or
//! on a click over the title bar of a target-capable tile. Lets the
//! user pick which host's agent the tile's subscription should route
//! through: `Local` (the daemon's own probes) or any Fleet host alias.
//!
//! Layout — centered modal, mirrors the `help` overlay pattern from
//! Slice 6.0 so the two modals feel visually kin:
//!
//! ```text
//! ┌ docker target · arrows/j/k select · Enter commits · Esc cancels ┐
//! │                                                                  │
//! │ ▶ Local                                                          │
//! │   prod-box         ● connected                                   │
//! │   staging          ◐ connecting                                  │
//! │   bastion          ○ (agent not deployed)                        │
//! │                                                                  │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The selected row gets the cyan `▶` marker + a bold text style.
//! Rows whose host isn't in `Connected` state are greyed out with an
//! inline annotation so the user can see the full fleet and
//! remediate — hiding unusable hosts would leave them guessing why
//! a retarget they expected to work can't be found.
//!
//! Forward-looking: the modal is tile-agnostic — 6d's Ports +
//! Processes tiles reuse it with `required_capability = "ports"` /
//! `"processes"` and the same modal shape.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use tepegoz_proto::HostState;

use crate::app::{App, HostPickerRow};

/// Rendered width; matches the help overlay so both modals feel
/// visually kin on 120-col terminals. Shrinks on narrower frames.
const HOST_PICKER_WIDTH: u16 = 68;
/// Chrome rows (borders + title + spacer + footer-hint). The picker's
/// minimum height is this plus one row for the Local entry, so the
/// modal always has something to select.
const HOST_PICKER_CHROME: u16 = 5;

pub(crate) fn render(app: &App, frame: &mut Frame<'_>) {
    let Some(picker) = &app.host_picker else {
        return;
    };

    let area = frame.area();
    let rows = app.host_picker_rows();
    // Desired height: one row per entry plus chrome, capped at frame
    // height. Minimum keeps Local + first fleet host visible when
    // the frame is squeezed.
    let desired_height = HOST_PICKER_CHROME + u16::try_from(rows.len()).unwrap_or(u16::MAX);
    let width = area.width.min(HOST_PICKER_WIDTH);
    let height = area.height.min(desired_height);
    if width < 30 || height < HOST_PICKER_CHROME + 1 {
        // Terminal too small — render a 1-line hint at the top rather
        // than trying to lay out a modal that wouldn't fit. Matches
        // help.rs's tiny-terminal fallback tone.
        let hint = Paragraph::new(Line::from(Span::styled(
            "too small — Esc closes",
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

    frame.render_widget(Clear, rect);

    let title = format!(
        " {} target · ↑/↓/j/k select · Enter commits · Esc cancels ",
        picker.target_tile_label()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::from(""));
    for (idx, row) in rows.iter().enumerate() {
        lines.push(format_row(row, idx == picker.selected));
    }
    lines.push(Line::from(""));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn format_row(row: &HostPickerRow, selected: bool) -> Line<'static> {
    let marker_style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let label_style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let dim_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);

    let marker = if selected { " ▶ " } else { "   " };

    match row {
        HostPickerRow::Local => Line::from(vec![
            Span::styled(marker, marker_style),
            Span::styled("Local", label_style),
        ]),
        HostPickerRow::Remote {
            alias,
            state,
            usable,
        } => {
            let alias_span = if *usable {
                Span::styled(format!("{alias:<24}"), label_style)
            } else {
                Span::styled(
                    format!("{alias:<24}"),
                    dim_style.add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::DIM
                    }),
                )
            };
            let (glyph, glyph_color) = state_glyph(*state);
            let glyph_span = Span::styled(glyph, Style::default().fg(glyph_color));
            let annotation = annotation_for(*state, *usable);
            let annotation_span = Span::styled(format!(" {annotation}"), dim_style);
            Line::from(vec![
                Span::styled(marker, marker_style),
                alias_span,
                Span::raw(" "),
                glyph_span,
                annotation_span,
            ])
        }
    }
}

/// Mirrors `scope::fleet::state_glyph` so picker rows use the same
/// vocabulary the Fleet tile uses. Duplicated rather than re-exported
/// to keep the Fleet module's public surface narrow; if the glyph
/// vocabulary ever needs a third caller, hoist to `scope/mod.rs`.
fn state_glyph(state: HostState) -> (&'static str, Color) {
    match state {
        HostState::Connected => ("●", Color::Green),
        HostState::Connecting | HostState::Degraded => ("◐", Color::Yellow),
        HostState::Disconnected => ("○", Color::Gray),
        HostState::AuthFailed
        | HostState::HostKeyMismatch
        | HostState::AgentNotDeployed
        | HostState::AgentVersionMismatch => ("⚠", Color::Red),
    }
}

/// Human-readable annotation for a host row. Usable rows get a terse
/// state label ("connected"); unusable rows get a parenthesized
/// explanation so the user sees *why* a host can't serve the current
/// retarget, without having to reach for the Fleet tile.
fn annotation_for(state: HostState, usable: bool) -> &'static str {
    if usable {
        "connected"
    } else {
        match state {
            HostState::Disconnected => "(not connected)",
            HostState::Connecting => "(connecting)",
            HostState::Connected => "connected", // defensive — shouldn't hit
            HostState::Degraded => "(degraded)",
            HostState::AuthFailed => "(auth failed)",
            HostState::HostKeyMismatch => "(host-key mismatch)",
            HostState::AgentNotDeployed => "(agent not deployed)",
            HostState::AgentVersionMismatch => "(agent version mismatch)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, HostPickerModal, HostPickerTargetTile};
    use tepegoz_proto::{HostEntry, HostState};

    fn test_app(fleet_hosts: Vec<(&str, HostState)>) -> App {
        let mut app = App::new(1, "/bin/sh".into(), (40, 160));
        // Seed fleet with provided hosts + states.
        let entries: Vec<HostEntry> = fleet_hosts
            .iter()
            .map(|(alias, _)| HostEntry {
                alias: (*alias).into(),
                hostname: format!("{alias}.example"),
                user: "test".into(),
                port: 22,
                identity_files: vec![],
                proxy_jump: None,
            })
            .collect();
        let states = fleet_hosts
            .into_iter()
            .map(|(a, s)| (a.to_string(), s))
            .collect();
        app.fleet.state = crate::app::FleetScopeState::Available {
            hosts: entries,
            states,
            source: "test".into(),
        };
        app
    }

    fn render_frame(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(app, f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn local_row_first_even_with_hosts() {
        let mut app = test_app(vec![("alpha", HostState::Connected)]);
        app.host_picker = Some(HostPickerModal {
            target_tile: HostPickerTargetTile::Docker,
            required_capability: "docker",
            selected: 0,
        });
        let rows = render_frame(&app, 100, 20);
        let joined = rows.join("\n");
        // Local's Y position must be less than alpha's.
        let local_y = joined.find("Local").expect("local row present");
        let alpha_y = joined.find("alpha").expect("alpha row present");
        assert!(
            local_y < alpha_y,
            "Local must appear before any Fleet alias"
        );
    }

    #[test]
    fn disconnected_host_row_renders_annotation() {
        let mut app = test_app(vec![("gone", HostState::Disconnected)]);
        app.host_picker = Some(HostPickerModal {
            target_tile: HostPickerTargetTile::Docker,
            required_capability: "docker",
            selected: 0,
        });
        let rows = render_frame(&app, 100, 20);
        let joined = rows.join("\n");
        assert!(
            joined.contains("not connected"),
            "disconnected host must show (not connected): {joined}"
        );
    }

    #[test]
    fn connected_host_row_renders_connected_marker() {
        let mut app = test_app(vec![("alive", HostState::Connected)]);
        app.host_picker = Some(HostPickerModal {
            target_tile: HostPickerTargetTile::Docker,
            required_capability: "docker",
            selected: 0,
        });
        let rows = render_frame(&app, 100, 20);
        let joined = rows.join("\n");
        assert!(
            joined.contains("●"),
            "connected host must show the green ● glyph: {joined}"
        );
        assert!(
            joined.contains("connected"),
            "connected host must be labeled connected: {joined}"
        );
    }

    #[test]
    fn selected_row_gets_marker() {
        let mut app = test_app(vec![("alpha", HostState::Connected)]);
        app.host_picker = Some(HostPickerModal {
            target_tile: HostPickerTargetTile::Docker,
            required_capability: "docker",
            selected: 1, // select alpha, not Local
        });
        let rows = render_frame(&app, 100, 20);
        let joined = rows.join("\n");
        assert!(
            joined.contains("▶"),
            "selected row must carry the ▶ marker: {joined}"
        );
    }

    #[test]
    fn tiny_terminal_falls_back_to_hint_line() {
        let mut app = test_app(vec![]);
        app.host_picker = Some(HostPickerModal {
            target_tile: HostPickerTargetTile::Docker,
            required_capability: "docker",
            selected: 0,
        });
        let rows = render_frame(&app, 20, 4);
        let joined = rows.join("\n");
        assert!(
            joined.contains("too small"),
            "tiny terminal must show a fallback hint: {joined}"
        );
    }
}
