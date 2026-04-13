//! Docker scope renderer.
//!
//! Three distinct visual states (per CTO §2 sign-off):
//!
//! - **Connecting** — "Connecting to docker engine…" centered. The
//!   moment `Subscribe(Docker)` is sent, before the first event.
//! - **Available** — container table. `containers.len() == 0` renders
//!   a separate "No containers" empty-state (or "No containers match
//!   filter" when the filter is narrowing nothing). Don't conflate
//!   "engine said no containers" with "engine unavailable".
//! - **Unavailable** — the structured reason from the daemon's
//!   `DockerUnavailable` event, verbatim. Multi-line; wraps.
//!
//! Layout within the tile: optional filter bar · status bar · body ·
//! help bar. The outer bordered block signals focus (bright cyan
//! border when focused, dim gray when not).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};

use tepegoz_proto::{DockerActionKind, DockerContainer};

use crate::app::{DockerScope, DockerScopeState, PendingConfirm, action_verb};

pub(crate) fn render(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect, focused: bool) {
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
        .title("docker")
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
    let chunks = Layout::vertical(constraints).split(inner);

    render_status_bar(scope, frame, chunks[0]);

    let body_area = if show_filter_bar {
        render_filter_bar(scope, frame, chunks[1]);
        chunks[2]
    } else {
        chunks[1]
    };

    match &scope.state {
        DockerScopeState::Idle => {
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

    // Confirm modal overlays the tile's inner area when a K/X action
    // is awaiting confirmation (per C3a UX clarification #3). It's
    // drawn last so it paints over the table/help bar, but stays
    // inside the tile's Rect so other tiles keep rendering.
    if let Some(pending) = &scope.pending_confirm {
        render_confirm_modal(frame, inner, pending);
    }
}

/// Inline confirm prompt for `Kill` / `Remove` (the destructive
/// actions). Centered inside the Docker tile's inner Rect; never
/// covers the whole screen. Input routing is handled in
/// `App::handle_scope_key`: while `pending_confirm` is `Some`, `y`/`Y`
/// confirms; any other key cancels.
fn render_confirm_modal(frame: &mut Frame<'_>, tile_inner: Rect, pending: &PendingConfirm) {
    let verb = match pending.kind {
        DockerActionKind::Kill => "Kill",
        DockerActionKind::Remove => "Remove",
        // begin_confirm is currently only called for Kill/Remove, but
        // fall through to action_verb rather than unreachable!() so a
        // future caller can't crash the TUI by adding a new confirm
        // kind.
        other => action_verb(other),
    };
    let width = tile_inner
        .width
        .saturating_sub(4)
        .min(50)
        .max(tile_inner.width.min(20));
    let height = 5u16.min(tile_inner.height);
    if width == 0 || height == 0 {
        return;
    }
    let x = tile_inner.x + (tile_inner.width.saturating_sub(width)) / 2;
    let y = tile_inner.y + (tile_inner.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .title(Span::styled(
            " confirm ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner_area = block.inner(area);
    frame.render_widget(block, area);

    let prompt = format!("{verb} container {}?", pending.container_name);
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            prompt,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(Span::styled(
            "[y] confirm · any other key cancels",
            Style::default().fg(Color::DarkGray),
        ))
        .alignment(Alignment::Center),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(body, inner_area);
}

fn render_status_bar(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect) {
    let (text, fg) = match &scope.state {
        DockerScopeState::Idle => ("idle".to_string(), Color::DarkGray),
        DockerScopeState::Connecting => ("connecting…".to_string(), Color::Yellow),
        DockerScopeState::Available {
            containers,
            engine_source,
        } => (
            format!(
                "{}/{} container(s){} · {engine_source}",
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
        DockerScopeState::Unavailable { .. } => ("unavailable".to_string(), Color::Red),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(fg))),
        area,
    );
}

fn render_filter_bar(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect) {
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
            Constraint::Length(20), // name
            Constraint::Length(20), // image
            Constraint::Length(10), // state
            Constraint::Length(16), // status
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
    .alignment(Alignment::Center);
    frame.render_widget(widget, area);
}

fn render_unavailable(frame: &mut Frame<'_>, area: Rect, reason: &str) {
    // Verbatim. The daemon's ConnectError lists every socket
    // candidate it tried; don't truncate — the user needs to see it.
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
    .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn render_help_bar(scope: &DockerScope, frame: &mut Frame<'_>, area: Rect) {
    let help = if scope.pending_confirm.is_some() {
        "[y] confirm · any other key cancels"
    } else if scope.filter_active {
        "[Enter] apply · [Esc] clear · [Backspace] delete"
    } else {
        "[j/k] nav · [/] filter · [r] restart · [s] stop · [K] kill · [X] remove"
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

    fn scope_with(state: DockerScopeState, filter: &str, filter_active: bool) -> DockerScope {
        DockerScope {
            state,
            selection: 0,
            filter: filter.to_string(),
            filter_active,
            sub_id: 42,
            pending_confirm: None,
        }
    }

    /// Render the docker tile into a TestBackend-backed frame, filling
    /// the whole backend area (equivalent to the docker tile being the
    /// only tile drawn in these focused render tests).
    fn draw_and_rows(scope: &DockerScope, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(scope, frame, Rect::new(0, 0, width, height), true))
            .unwrap();
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
        let mut scope = scope_with(
            DockerScopeState::Available {
                containers: vec![
                    make_container("webapp", "nginx:latest", "running", "Up 5 min"),
                    make_container("postgres-db", "postgres:14", "running", "Up 10 min"),
                    make_container("stale", "alpine:latest", "exited", "Exited (0)"),
                ],
                engine_source: "Docker Desktop".into(),
            },
            "",
            false,
        );
        scope.selection = 1;

        let rows = draw_and_rows(&scope, 120, 30);

        assert!(any_row_contains(&rows, "webapp"));
        assert!(any_row_contains(&rows, "postgres-db"));
        assert!(any_row_contains(&rows, "stale"));
        assert!(any_row_contains(&rows, "nginx:latest"));
        assert!(any_row_contains(&rows, "running"));
        assert!(any_row_contains(&rows, "exited"));

        let selected_row = rows
            .iter()
            .find(|r| r.contains("postgres-db"))
            .expect("postgres-db row present");
        assert!(
            selected_row.contains("▶ "),
            "selected row must show ▶ marker; got {selected_row:?}"
        );

        let web_row = rows.iter().find(|r| r.contains("webapp")).unwrap();
        assert!(
            !web_row.contains("▶ "),
            "non-selected row must not show ▶ marker; got {web_row:?}"
        );

        assert!(any_row_contains(&rows, "Docker Desktop"));
        assert!(any_row_contains(&rows, "3/3"));
    }

    #[test]
    fn connecting_state_renders_connecting_message() {
        let scope = scope_with(DockerScopeState::Connecting, "", false);
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "Connecting to docker engine"));
        assert!(any_row_contains(&rows, "connecting…"));
    }

    #[test]
    fn unavailable_state_renders_reason_verbatim() {
        let reason = "docker engine unreachable. Tried:\n  - Docker Desktop: socket file not found";
        let scope = scope_with(
            DockerScopeState::Unavailable {
                reason: reason.into(),
            },
            "",
            false,
        );
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "Docker engine unavailable"));
        assert!(
            any_row_contains(&rows, "Docker Desktop: socket file not found"),
            "Unavailable reason text must render verbatim. Rows: {rows:?}"
        );
        assert!(any_row_contains(&rows, "unavailable"));
    }

    #[test]
    fn available_but_empty_shows_distinct_no_containers_message() {
        let scope = scope_with(
            DockerScopeState::Available {
                containers: Vec::new(),
                engine_source: "Docker Desktop".into(),
            },
            "",
            false,
        );
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "No containers"));
        assert!(!any_row_contains(&rows, "Docker engine unavailable"));
    }

    #[test]
    fn filter_that_matches_nothing_shows_no_match_message() {
        let scope = scope_with(
            DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            "no-such-container",
            false,
        );
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "No containers match filter"));
        assert!(any_row_contains(&rows, "filter: no-such-container"));
    }

    #[test]
    fn filter_bar_shows_caret_when_active() {
        let scope = scope_with(
            DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            "we",
            true,
        );
        let rows = draw_and_rows(&scope, 120, 30);
        let filter_row = rows.iter().find(|r| r.contains("filter:")).unwrap();
        assert!(
            filter_row.contains("we_"),
            "active filter must end with the caret `_`; got {filter_row:?}"
        );
    }

    #[test]
    fn ports_column_shows_public_and_internal_mappings() {
        // 180×20 backend for this test — the port formatting needs
        // more columns after NAME/IMAGE/STATE/STATUS consume their
        // fixed shares.
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
        let scope = scope_with(
            DockerScopeState::Available {
                containers: vec![c],
                engine_source: "test".into(),
            },
            "",
            false,
        );
        let rows = draw_and_rows(&scope, 180, 20);
        let web_row = rows
            .iter()
            .find(|r| r.contains("/web"))
            .expect("web row present");
        assert!(
            web_row.contains("80") && web_row.contains("8080"),
            "public port mapping must render; got {web_row:?}"
        );
        assert!(
            web_row.contains("9090/tcp"),
            "internal-only port must render as `9090/tcp`; got {web_row:?}"
        );
    }

    #[test]
    fn pending_confirm_renders_modal_with_container_name_and_prompt() {
        use std::time::{Duration, Instant};
        let mut scope = scope_with(
            DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            "",
            false,
        );
        scope.pending_confirm = Some(PendingConfirm {
            kind: DockerActionKind::Kill,
            container_id: "id-web".into(),
            container_name: "web".into(),
            deadline: Instant::now() + Duration::from_secs(10),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        let joined = rows.join("\n");
        assert!(
            joined.contains("Kill container web?"),
            "confirm prompt must name the action + container; got {joined}"
        );
        assert!(
            joined.contains("[y] confirm"),
            "confirm body must list the y hint; got {joined}"
        );
        // Help bar swaps to the confirm-specific hint.
        assert!(
            any_row_contains(&rows, "[y] confirm"),
            "help bar must change when confirm is active"
        );
    }

    #[test]
    fn confirm_modal_is_absent_without_pending_confirm() {
        let scope = scope_with(
            DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            "",
            false,
        );
        let rows = draw_and_rows(&scope, 120, 30);
        let joined = rows.join("\n");
        assert!(!joined.contains("confirm"));
        assert!(!joined.contains("Kill container"));
    }

    #[test]
    fn help_bar_shows_action_keybinds_when_idle() {
        let scope = scope_with(
            DockerScopeState::Available {
                containers: vec![make_container("web", "nginx", "running", "Up")],
                engine_source: "test".into(),
            },
            "",
            false,
        );
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(
            any_row_contains(&rows, "[r] restart"),
            "help bar must advertise r/s/K/X keybinds in the idle state"
        );
        assert!(any_row_contains(&rows, "[K] kill"));
        assert!(any_row_contains(&rows, "[X] remove"));
    }

    #[test]
    fn unfocused_border_is_dimmed() {
        // Smoke test: rendering with focused=false doesn't panic and
        // still shows content. Full visual distinction is a manual
        // check — TestBackend doesn't preserve color attributes in
        // symbols, but we can verify the structure.
        let scope = scope_with(DockerScopeState::Connecting, "", false);
        let backend = TestBackend::new(60, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(&scope, frame, Rect::new(0, 0, 60, 15), false))
            .unwrap();
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
        assert!(rows[0].contains('─'), "top border must still be drawn");
        assert!(
            rows.iter().any(|r| r.contains("Connecting")),
            "content must still render when unfocused"
        );
    }
}
