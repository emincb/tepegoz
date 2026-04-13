//! Docker scope renderer.
//!
//! Three distinct visual states (per CTO §2 sign-off):
//!
//! - **Connecting** — "Connecting to docker engine…" centered. The moment
//!   [`Subscription::Docker`] is sent, before the first event arrives.
//! - **Available** — container table. If `containers.len() == 0` shows a
//!   separate "No containers" empty-state (or "No containers match filter"
//!   when the filter is narrowing nothing). Don't conflate "engine said no
//!   containers" with "engine unavailable".
//! - **Unavailable** — the structured reason from the daemon's
//!   `DockerUnavailable` event, rendered verbatim. Multi-line; wraps.
//!
//! Layout: top status bar · optional filter bar · body · bottom help bar.
//! Selected row is highlighted with reversed colors; at the table level a
//! left marker `▶` replaces any column padding so selection is obvious
//! even on monochrome terminals.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};

use tepegoz_proto::DockerContainer;

use crate::app::{DockerScope, DockerScopeState};

pub(crate) fn render(scope: &DockerScope, frame: &mut Frame<'_>) {
    let area = frame.area();

    let show_filter_bar = scope.filter_active || !scope.filter.is_empty();

    let constraints = if show_filter_bar {
        vec![
            Constraint::Length(1), // status bar
            Constraint::Length(1), // filter bar
            Constraint::Min(1),    // body
            Constraint::Length(1), // help bar
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ]
    };
    let chunks = Layout::vertical(constraints).split(area);

    render_status_bar(scope, frame, chunks[0]);

    let body_area = if show_filter_bar {
        render_filter_bar(scope, frame, chunks[1]);
        chunks[2]
    } else {
        chunks[1]
    };

    match &scope.state {
        DockerScopeState::Idle => {
            // Transient: we just left scope view (switch_to_pane resets to
            // Idle). The renderer shouldn't normally be invoked in Idle
            // because the runtime enters Pane mode on switch_to_pane. But
            // render something sensible in case of a race.
            render_centered(frame, body_area, "idle", Color::DarkGray);
        }
        DockerScopeState::Connecting => {
            render_centered(
                frame,
                body_area,
                "Connecting to docker engine…",
                Color::Yellow,
            );
        }
        DockerScopeState::Available { containers, .. } => {
            let visible: Vec<&DockerContainer> = containers
                .iter()
                .filter(|c| scope.matches_filter(c))
                .collect();
            if visible.is_empty() {
                let message = if scope.filter.is_empty() {
                    "No containers"
                } else {
                    "No containers match filter"
                };
                render_centered(frame, body_area, message, Color::DarkGray);
            } else {
                render_table(scope, &visible, frame, body_area);
            }
        }
        DockerScopeState::Unavailable { reason } => {
            render_unavailable(frame, body_area, reason);
        }
    }

    render_help_bar(scope, frame, chunks[chunks.len() - 1]);
}

fn render_status_bar(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect) {
    let (text, fg) = match &scope.state {
        DockerScopeState::Idle => ("docker scope · idle".to_string(), Color::DarkGray),
        DockerScopeState::Connecting => ("docker scope · connecting…".to_string(), Color::Yellow),
        DockerScopeState::Available {
            containers,
            engine_source,
        } => (
            format!(
                "docker scope · {}/{} container(s){} · {engine_source}",
                scope.visible_count(),
                containers.len(),
                if scope.filter.is_empty() {
                    String::new()
                } else {
                    format!(" (filter \"{}\")", scope.filter)
                },
            ),
            Color::Green,
        ),
        DockerScopeState::Unavailable { .. } => {
            ("docker scope · unavailable".to_string(), Color::Red)
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(fg))),
        area,
    );
}

fn render_filter_bar(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect) {
    // "filter: <input>_" with a caret when editing; "filter: <input>" when
    // applied but not editing. The caret is a literal underscore because
    // ratatui's `show_cursor` relies on a full terminal cursor move that
    // doesn't play nicely with our alt-screen setup.
    let mut spans = vec![
        Span::styled(
            "filter: ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(scope.filter.clone()),
    ];
    if scope.filter_active {
        spans.push(Span::styled(
            "_",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::SLOW_BLINK),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_table(
    scope: &DockerScope,
    visible: &[&DockerContainer],
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let header = Row::new([
        Cell::from("  "),
        Cell::from("NAME"),
        Cell::from("IMAGE"),
        Cell::from("STATE"),
        Cell::from("STATUS"),
        Cell::from("PORTS"),
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let selected = i == scope.selection;
            let marker = if selected { "▶ " } else { "  " };
            let name = c
                .names
                .first()
                .map(String::as_str)
                .unwrap_or("")
                .to_string();
            let state_style = state_color(&c.state);
            let row = Row::new([
                Cell::from(marker),
                Cell::from(name),
                Cell::from(c.image.clone()),
                Cell::from(Span::styled(c.state.clone(), state_style)),
                Cell::from(c.status.clone()),
                Cell::from(format_ports(&c.ports)),
            ]);
            if selected {
                row.style(
                    Style::default()
                        .bg(Color::Rgb(40, 40, 60))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                row
            }
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),  // selection marker
            Constraint::Length(30), // name
            Constraint::Length(30), // image
            Constraint::Length(10), // state
            Constraint::Length(22), // status
            Constraint::Min(10),    // ports
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::NONE));

    frame.render_widget(table, area);
}

fn state_color(state: &str) -> Style {
    match state {
        "running" => Style::default().fg(Color::Green),
        "exited" | "dead" => Style::default().fg(Color::Red),
        "paused" | "restarting" => Style::default().fg(Color::Yellow),
        "created" | "removing" => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn format_ports(ports: &[tepegoz_proto::DockerPort]) -> String {
    // "host:80->container:8080/tcp, 443->8443/tcp" — public mappings first,
    // then internal-only. Keeps the table terse; the full list lives in
    // the per-container detail view (C3).
    let mut bits: Vec<String> = Vec::new();
    for p in ports.iter().take(3) {
        if p.public_port != 0 {
            bits.push(format!(
                "{}:{}→{}/{}",
                p.ip.as_str().split(':').next().unwrap_or(""),
                p.public_port,
                p.private_port,
                p.protocol,
            ));
        } else {
            bits.push(format!("{}/{}", p.private_port, p.protocol));
        }
    }
    if ports.len() > 3 {
        bits.push(format!("+{}", ports.len() - 3));
    }
    bits.join(", ")
}

fn render_centered(frame: &mut Frame<'_>, area: Rect, text: &str, color: Color) {
    let widget = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
    ])
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(widget, area);
}

fn render_unavailable(frame: &mut Frame<'_>, area: Rect, reason: &str) {
    // Verbatim. The daemon's ConnectError lists every socket candidate it
    // tried with the reason each failed — that's exactly what the user
    // needs to see. Don't truncate, don't restyle the reason text.
    let widget = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Docker engine unavailable",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            reason.to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Tepegöz will keep retrying every 5s. Start docker and we'll pick it up.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red)),
    );
    frame.render_widget(widget, area);
}

fn render_help_bar(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect) {
    let help = if scope.filter_active {
        "[Enter] apply · [Esc] clear · [Backspace] delete"
    } else {
        "[j/k] nav · [g/G] top/bot · [/] filter · [Ctrl-b a] pane · [Ctrl-b d] detach"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(help, Style::default().fg(Color::DarkGray))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DockerScope, DockerScopeState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tepegoz_proto::{DockerContainer, DockerPort};

    fn make_container(name: &str, image: &str, state: &str, status: &str) -> DockerContainer {
        DockerContainer {
            id: format!("id-{name}"),
            names: vec![format!("/{name}")],
            image: image.into(),
            image_id: "sha256:dead".into(),
            command: "cmd".into(),
            created_unix_secs: 0,
            state: state.into(),
            status: status.into(),
            ports: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// Helper: render and return the buffer as a `Vec<String>`, one entry
    /// per row, with trailing whitespace trimmed so `contains` checks are
    /// robust.
    fn draw_and_rows(scope: &DockerScope) -> Vec<String> {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(scope, frame)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        (0..buf.area.height as usize)
            .map(|row| {
                let start = row * w;
                (0..w)
                    .map(|col| buf.content[start + col].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn any_row_contains(rows: &[String], needle: &str) -> bool {
        rows.iter().any(|row| row.contains(needle))
    }

    #[test]
    fn available_state_renders_container_table_with_names_states_and_selection_marker() {
        let scope = DockerScope {
            state: DockerScopeState::Available {
                containers: vec![
                    make_container("webapp", "nginx:latest", "running", "Up 5 minutes"),
                    make_container("postgres-db", "postgres:14", "running", "Up 10 minutes"),
                    make_container("stale", "alpine:latest", "exited", "Exited (0)"),
                ],
                engine_source: "Docker Desktop".into(),
            },
            selection: 1,
            filter: String::new(),
            filter_active: false,
            sub_id: Some(42),
        };

        let rows = draw_and_rows(&scope);

        assert!(
            any_row_contains(&rows, "webapp"),
            "name column must contain 'webapp'. Rows: {rows:?}"
        );
        assert!(any_row_contains(&rows, "postgres-db"));
        assert!(any_row_contains(&rows, "stale"));
        assert!(any_row_contains(&rows, "nginx:latest"));
        assert!(any_row_contains(&rows, "running"));
        assert!(any_row_contains(&rows, "exited"));

        // Selected row (index 1 = postgres-db) must have the ▶ marker.
        let selected_row = rows
            .iter()
            .find(|r| r.contains("postgres-db"))
            .expect("postgres-db row present");
        assert!(
            selected_row.starts_with('▶') || selected_row.contains("▶ "),
            "selected row must start with ▶ marker; got {selected_row:?}"
        );

        // Non-selected rows must NOT have the marker.
        let web_row = rows.iter().find(|r| r.contains("webapp")).unwrap();
        assert!(
            !web_row.starts_with('▶'),
            "non-selected row must not show ▶ marker; got {web_row:?}"
        );

        // Status bar references the engine source and the live count.
        assert!(any_row_contains(&rows, "Docker Desktop"));
        assert!(any_row_contains(&rows, "3/3"));
    }

    #[test]
    fn connecting_state_renders_connecting_message() {
        let scope = DockerScope {
            state: DockerScopeState::Connecting,
            ..Default::default()
        };
        let rows = draw_and_rows(&scope);
        assert!(any_row_contains(&rows, "Connecting to docker engine"));
        assert!(any_row_contains(&rows, "connecting…"));
    }

    #[test]
    fn unavailable_state_renders_reason_verbatim() {
        let reason = "docker engine unreachable. Tried:\n  - Docker Desktop: socket file not found";
        let scope = DockerScope {
            state: DockerScopeState::Unavailable {
                reason: reason.into(),
            },
            ..Default::default()
        };
        let rows = draw_and_rows(&scope);
        assert!(any_row_contains(&rows, "Docker engine unavailable"));
        assert!(
            any_row_contains(&rows, "Docker Desktop: socket file not found"),
            "Unavailable reason text must render verbatim — user needs the diagnostic. Rows: {rows:?}"
        );
        // Status bar colored red (content-wise, "unavailable" present).
        assert!(any_row_contains(&rows, "unavailable"));
    }

    #[test]
    fn available_but_empty_shows_distinct_no_containers_message() {
        let scope = DockerScope {
            state: DockerScopeState::Available {
                containers: Vec::new(),
                engine_source: "Docker Desktop".into(),
            },
            ..Default::default()
        };
        let rows = draw_and_rows(&scope);
        assert!(any_row_contains(&rows, "No containers"));
        // Must NOT show the Unavailable text — empty list is a distinct
        // state from engine unreachable.
        assert!(!any_row_contains(&rows, "Docker engine unavailable"));
    }

    #[test]
    fn filter_that_matches_nothing_shows_no_match_message() {
        let scope = DockerScope {
            state: DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            filter: "no-such-container".into(),
            filter_active: false,
            selection: 0,
            sub_id: None,
        };
        let rows = draw_and_rows(&scope);
        assert!(any_row_contains(&rows, "No containers match filter"));
        // Filter bar shows the active filter.
        assert!(any_row_contains(&rows, "filter: no-such-container"));
    }

    #[test]
    fn filter_bar_shows_caret_when_active() {
        let scope = DockerScope {
            state: DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            filter: "we".into(),
            filter_active: true,
            selection: 0,
            sub_id: None,
        };
        let rows = draw_and_rows(&scope);
        let filter_row = rows.iter().find(|r| r.contains("filter:")).unwrap();
        assert!(
            filter_row.contains("we_") || filter_row.ends_with("we_"),
            "active filter must end with the caret `_`; got {filter_row:?}"
        );
    }

    #[test]
    fn ports_column_shows_public_and_internal_mappings() {
        // Wider TestBackend for this test: the full port formatting
        // ("0.0.0.0:80→8080/tcp, 9090/tcp") needs more than 120 cols after
        // the fixed-width NAME + IMAGE + STATE + STATUS columns consume
        // their share. 180 is realistic for a side-monitor terminal.
        let c = DockerContainer {
            id: "id".into(),
            names: vec!["/web".into()],
            image: "nginx".into(),
            image_id: "sha256:d".into(),
            command: String::new(),
            created_unix_secs: 0,
            state: "running".into(),
            status: "Up".into(),
            ports: vec![
                DockerPort {
                    ip: "0.0.0.0".into(),
                    private_port: 8080,
                    public_port: 80,
                    protocol: "tcp".into(),
                },
                DockerPort {
                    ip: "".into(),
                    private_port: 9090,
                    public_port: 0,
                    protocol: "tcp".into(),
                },
            ],
            labels: Vec::new(),
        };
        let scope = DockerScope {
            state: DockerScopeState::Available {
                containers: vec![c],
                engine_source: "test".into(),
            },
            ..Default::default()
        };
        let backend = TestBackend::new(180, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(&scope, frame)).unwrap();
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        let rows: Vec<String> = (0..buf.area.height as usize)
            .map(|row| {
                let start = row * w;
                (0..w)
                    .map(|col| buf.content[start + col].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let web_row = rows
            .iter()
            .find(|r| r.contains("/web"))
            .expect("web row present");
        assert!(
            web_row.contains("80") && web_row.contains("8080"),
            "public port mapping must be rendered; got {web_row:?}"
        );
        assert!(
            web_row.contains("9090/tcp"),
            "internal-only port must render as `9090/tcp`; got {web_row:?}"
        );
    }
}
