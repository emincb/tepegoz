//! Phase 4 Slice 4c: Ports tile renderer with Processes toggle-view.
//!
//! Hosts two coequal views in one tile — `Ports` (default) and
//! `Processes`. The user toggles between them with `p`. Tile title and
//! help-bar adapt to the active view; the inactive view's subscription
//! stays live so toggling back never shows stale data.
//!
//! Layout and state shape mirror `scope::docker` — three-state
//! lifecycle (Connecting / Available / Unavailable), filter bar on top,
//! table in the middle, help bar at the bottom. The Processes view's
//! CPU% column renders `None` as an em-dash per the Phase 4 Slice 4b
//! wire contract, so "first sample" is visually distinct from "idle".

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};
use tepegoz_proto::{ProbePort, ProbeProcess};

use crate::app::{
    PortsActiveView, PortsScope, PortsView, PortsViewState, ProcessesView, ProcessesViewState,
};

/// Entry point. Draws the Ports tile into `area`, matching the scope
/// rendering contract in `docs/ARCHITECTURE.md` §9.
pub(crate) fn render(
    scope: &PortsScope,
    frame: &mut Frame<'_>,
    area: Rect,
    focused: bool,
    hovered: bool,
) {
    let (border_color, border_modifier) = crate::scope::border_style(focused, hovered);
    let title = match scope.active {
        PortsActiveView::Ports => "ports".to_string(),
        PortsActiveView::Processes => "processes".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
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

    match scope.active {
        PortsActiveView::Ports => render_ports_view(&scope.ports, frame, inner),
        PortsActiveView::Processes => render_processes_view(&scope.processes, frame, inner),
    }
}

// ---------- Ports view ----------

fn render_ports_view(view: &PortsView, frame: &mut Frame<'_>, inner: Rect) {
    let show_filter_bar = view.filter_active || !view.filter.is_empty();

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

    render_ports_status_bar(view, frame, chunks[0]);

    let body_area = if show_filter_bar {
        render_filter_bar(&view.filter, view.filter_active, frame, chunks[1]);
        chunks[2]
    } else {
        chunks[1]
    };

    match &view.state {
        PortsViewState::Connecting => {
            render_centered(
                frame,
                body_area,
                "Connecting to the ports probe…",
                Color::Yellow,
            );
        }
        PortsViewState::Available { rows, .. } => {
            let visible: Vec<&ProbePort> = rows.iter().filter(|p| view.matches_filter(p)).collect();
            if visible.is_empty() {
                let message = if view.filter.is_empty() {
                    "No listening ports"
                } else {
                    "No ports match filter"
                };
                render_centered(frame, body_area, message, Color::DarkGray);
            } else {
                render_ports_table(view, &visible, frame, body_area);
            }
        }
        PortsViewState::Unavailable { reason } => {
            render_unavailable(frame, body_area, "Ports probe unavailable", reason);
        }
    }

    render_help_bar(
        PortsActiveView::Ports,
        view.filter_active,
        frame,
        chunks[chunks.len() - 1],
    );
}

fn render_ports_status_bar(view: &PortsView, frame: &mut Frame<'_>, area: Rect) {
    let (status_text, status_style) = match &view.state {
        PortsViewState::Connecting => (
            "connecting…".to_string(),
            Style::default().fg(Color::Yellow),
        ),
        PortsViewState::Available { rows, source } => {
            let visible = rows.iter().filter(|p| view.matches_filter(p)).count();
            let total = rows.len();
            let filter_note = if view.filter.is_empty() {
                String::new()
            } else {
                format!(" · filter: {}", view.filter)
            };
            (
                format!(
                    "{visible}/{total} port(s) · source: {source}{filter_note} · UDP coming v1.1"
                ),
                Style::default().fg(Color::Green),
            )
        }
        PortsViewState::Unavailable { .. } => {
            ("unavailable".to_string(), Style::default().fg(Color::Red))
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(status_text, status_style)),
        area,
    );
}

fn render_ports_table(view: &PortsView, visible: &[&ProbePort], frame: &mut Frame<'_>, area: Rect) {
    let rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let selected = idx == view.selection;
            let marker = if selected { "▶ " } else { "  " };
            let container = p
                .container_id
                .as_deref()
                .map(|id| {
                    // Truncate long container ids for table readability.
                    let short: String = id.chars().take(12).collect();
                    short
                })
                .unwrap_or_default();
            let partial_cue = if p.partial { "?" } else { " " };
            let cells = vec![
                Span::styled(
                    format!("{marker}{partial_cue} {}", p.protocol),
                    row_style(selected),
                ),
                Span::styled(
                    format!("{}:{}", truncate(&p.local_ip, 15), p.local_port),
                    row_style(selected),
                ),
                Span::styled(format!("{}", p.pid), row_style(selected)),
                Span::styled(
                    truncate(&p.process_name, 24).to_string(),
                    row_style(selected),
                ),
                Span::styled(container, row_style(selected).fg(Color::Cyan)),
            ];
            Row::new(cells)
        })
        .collect();

    let header = Row::new(vec!["    PROTO", "ADDRESS", "PID", "PROCESS", "CONTAINER"]).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let widths = [
        Constraint::Length(12),
        Constraint::Length(22),
        Constraint::Length(7),
        Constraint::Length(26),
        Constraint::Min(12),
    ];

    let table = Table::new(rows, widths).header(header).column_spacing(1);
    frame.render_widget(table, area);
}

// ---------- Processes view ----------

fn render_processes_view(view: &ProcessesView, frame: &mut Frame<'_>, inner: Rect) {
    let show_filter_bar = view.filter_active || !view.filter.is_empty();

    let constraints = if show_filter_bar {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ]
    };
    let chunks = Layout::vertical(constraints).split(inner);

    render_processes_status_bar(view, frame, chunks[0]);

    let body_area = if show_filter_bar {
        render_filter_bar(&view.filter, view.filter_active, frame, chunks[1]);
        chunks[2]
    } else {
        chunks[1]
    };

    match &view.state {
        ProcessesViewState::Connecting => {
            render_centered(
                frame,
                body_area,
                "Connecting to the processes probe…",
                Color::Yellow,
            );
        }
        ProcessesViewState::Available { rows, .. } => {
            let visible: Vec<&ProbeProcess> =
                rows.iter().filter(|p| view.matches_filter(p)).collect();
            if visible.is_empty() {
                let message = if view.filter.is_empty() {
                    "No running processes"
                } else {
                    "No processes match filter"
                };
                render_centered(frame, body_area, message, Color::DarkGray);
            } else {
                render_processes_table(view, &visible, frame, body_area);
            }
        }
        ProcessesViewState::Unavailable { reason } => {
            render_unavailable(frame, body_area, "Processes probe unavailable", reason);
        }
    }

    render_help_bar(
        PortsActiveView::Processes,
        view.filter_active,
        frame,
        chunks[chunks.len() - 1],
    );
}

fn render_processes_status_bar(view: &ProcessesView, frame: &mut Frame<'_>, area: Rect) {
    let (status_text, status_style) = match &view.state {
        ProcessesViewState::Connecting => (
            "connecting…".to_string(),
            Style::default().fg(Color::Yellow),
        ),
        ProcessesViewState::Available { rows, source } => {
            let visible = rows.iter().filter(|p| view.matches_filter(p)).count();
            let total = rows.len();
            let filter_note = if view.filter.is_empty() {
                String::new()
            } else {
                format!(" · filter: {}", view.filter)
            };
            (
                format!("{visible}/{total} process(es) · source: {source}{filter_note}"),
                Style::default().fg(Color::Green),
            )
        }
        ProcessesViewState::Unavailable { .. } => {
            ("unavailable".to_string(), Style::default().fg(Color::Red))
        }
    };
    frame.render_widget(
        Paragraph::new(Span::styled(status_text, status_style)),
        area,
    );
}

fn render_processes_table(
    view: &ProcessesView,
    visible: &[&ProbeProcess],
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(idx, p)| {
            let selected = idx == view.selection;
            let marker = if selected { "▶ " } else { "  " };
            let partial_cue = if p.partial { "?" } else { " " };
            // CPU% em-dash semantic: `None` means "not yet measured"
            // (first sample after subscribe, no prior delta). Render
            // as an em-dash so the user doesn't misread first-refresh
            // idleness as "everything is idle" (Phase 4 Slice 4b
            // wire contract).
            let cpu_text = match p.cpu_percent {
                None => "   —".to_string(),
                Some(v) => format!("{v:>5.1}"),
            };
            let mem_text = format_bytes(p.mem_bytes);
            let cells = vec![
                Span::styled(
                    format!("{marker}{partial_cue} {}", p.pid),
                    row_style(selected),
                ),
                Span::styled(
                    format!("{}", p.parent_pid),
                    row_style(selected).fg(Color::DarkGray),
                ),
                Span::styled(cpu_text, row_style(selected)),
                Span::styled(mem_text, row_style(selected)),
                Span::styled(truncate(&p.command, 48).to_string(), row_style(selected)),
            ];
            Row::new(cells)
        })
        .collect();

    let header = Row::new(vec!["      PID", "PPID", "  CPU%", "    MEM", "COMMAND"]).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    let widths = [
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Min(20),
    ];

    let table = Table::new(rows, widths).header(header).column_spacing(1);
    frame.render_widget(table, area);
}

// ---------- Shared chrome ----------

fn render_filter_bar(filter: &str, filter_active: bool, frame: &mut Frame<'_>, area: Rect) {
    let caret = if filter_active { "_" } else { "" };
    let text = format!("filter: {filter}{caret}");
    let style = if filter_active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

fn render_help_bar(view: PortsActiveView, filter_active: bool, frame: &mut Frame<'_>, area: Rect) {
    let help = match (view, filter_active) {
        (_, true) => "[Enter] apply · [Esc] clear · [Backspace] delete",
        (PortsActiveView::Ports, false) => "[j/k] nav · [/] filter · [p] Processes",
        (PortsActiveView::Processes, false) => "[j/k] nav · [/] filter · [p] Ports",
    };
    frame.render_widget(
        Paragraph::new(Span::styled(help, Style::default().fg(Color::DarkGray))),
        area,
    );
}

fn render_centered(frame: &mut Frame<'_>, area: Rect, text: &str, color: Color) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(text, Style::default().fg(color))).alignment(Alignment::Center),
    ];
    let para = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(para, area);
}

fn render_unavailable(frame: &mut Frame<'_>, area: Rect, title: &str, reason: &str) {
    let mut lines = vec![
        Line::from(Span::styled(
            title,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center),
        Line::from(""),
    ];
    for line in reason.lines().take(10) {
        lines.push(
            Line::from(Span::styled(line, Style::default().fg(Color::Red)))
                .alignment(Alignment::Center),
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn row_style(selected: bool) -> Style {
    if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

/// Truncate a string to at most `max` characters, replacing the
/// trailing overflow with `…`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// Format a byte count with one of KiB / MiB / GiB, matching the
/// resolution `sysinfo` reports (mem_bytes is always in bytes).
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:>6.1}G", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:>6.1}M", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:>6.1}K", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes:>6}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn fresh_ports_view() -> PortsView {
        PortsView {
            state: PortsViewState::Connecting,
            selection: 0,
            filter: String::new(),
            filter_active: false,
        }
    }

    fn fresh_processes_view() -> ProcessesView {
        ProcessesView {
            state: ProcessesViewState::Connecting,
            selection: 0,
            filter: String::new(),
            filter_active: false,
        }
    }

    fn scope_with_ports(state: PortsViewState) -> PortsScope {
        PortsScope {
            ports: PortsView {
                state,
                selection: 0,
                filter: String::new(),
                filter_active: false,
            },
            processes: fresh_processes_view(),
            active: PortsActiveView::Ports,
            ports_sub_id: 3,
            processes_sub_id: 4,
        }
    }

    fn scope_with_processes(state: ProcessesViewState) -> PortsScope {
        PortsScope {
            ports: fresh_ports_view(),
            processes: ProcessesView {
                state,
                selection: 0,
                filter: String::new(),
                filter_active: false,
            },
            active: PortsActiveView::Processes,
            ports_sub_id: 3,
            processes_sub_id: 4,
        }
    }

    /// Render the scope into a `TestBackend`-backed frame and return
    /// the rendered rows as trimmed strings. Mirrors the Docker
    /// scope test harness.
    fn draw_and_rows(scope: &PortsScope, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(scope, frame, Rect::new(0, 0, width, height), true, false))
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

    fn port(protocol: &str, port: u16, pid: u32, name: &str, container: Option<&str>) -> ProbePort {
        ProbePort {
            local_ip: "0.0.0.0".into(),
            local_port: port,
            protocol: protocol.into(),
            pid,
            process_name: name.into(),
            container_id: container.map(|s| s.to_string()),
            partial: false,
        }
    }

    fn process_row(pid: u32, cmd: &str, cpu: Option<f32>, mem: u64) -> ProbeProcess {
        ProbeProcess {
            pid,
            parent_pid: 1,
            start_time_unix_secs: 1_700_000_000,
            command: cmd.into(),
            cpu_percent: cpu,
            mem_bytes: mem,
            partial: false,
        }
    }

    #[test]
    fn connecting_state_renders_connecting_message_ports() {
        let scope = scope_with_ports(PortsViewState::Connecting);
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "Connecting to the ports probe"));
        assert!(any_row_contains(&rows, "connecting…"));
    }

    #[test]
    fn connecting_state_renders_connecting_message_processes() {
        let scope = scope_with_processes(ProcessesViewState::Connecting);
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "Connecting to the processes probe"));
    }

    #[test]
    fn available_state_renders_port_table_with_selection_marker() {
        let mut scope = scope_with_ports(PortsViewState::Available {
            rows: vec![
                port("tcp", 3000, 200, "web", None),
                port("tcp", 5432, 300, "postgres", Some("abc123def456")),
                port("tcp", 6379, 400, "redis", None),
            ],
            source: "linux-procfs".into(),
        });
        scope.ports.selection = 1;
        let rows = draw_and_rows(&scope, 120, 30);

        assert!(any_row_contains(&rows, "web"));
        assert!(any_row_contains(&rows, "postgres"));
        assert!(any_row_contains(&rows, "redis"));
        assert!(any_row_contains(&rows, "3000"));
        assert!(any_row_contains(&rows, "5432"));
        assert!(
            any_row_contains(&rows, "abc123def456"),
            "short container id column must render when container_id is Some"
        );

        let selected_row = rows
            .iter()
            .find(|r| r.contains("postgres"))
            .expect("postgres row present");
        assert!(
            selected_row.contains("▶ "),
            "selected row must show ▶ marker; got {selected_row:?}"
        );
        let web_row = rows.iter().find(|r| r.contains("web")).unwrap();
        assert!(
            !web_row.contains("▶ "),
            "non-selected row must not show ▶ marker"
        );

        assert!(
            any_row_contains(&rows, "3/3 port(s)"),
            "status bar must show visible/total count"
        );
        assert!(
            any_row_contains(&rows, "UDP coming v1.1"),
            "status bar must flag the UDP-deferred hint per CTO's 4c UDP resolution"
        );
    }

    #[test]
    fn available_state_renders_processes_table_with_selection_marker() {
        let mut scope = scope_with_processes(ProcessesViewState::Available {
            rows: vec![
                process_row(200, "web", Some(2.5), 8_388_608),
                process_row(300, "postgres -D /var/lib", Some(12.1), 134_217_728),
            ],
            source: "sysinfo".into(),
        });
        scope.processes.selection = 1;
        let rows = draw_and_rows(&scope, 120, 30);

        assert!(any_row_contains(&rows, "web"));
        assert!(any_row_contains(&rows, "postgres"));
        assert!(any_row_contains(&rows, "200"));
        assert!(any_row_contains(&rows, "300"));
        let selected_row = rows
            .iter()
            .find(|r| r.contains("postgres"))
            .expect("postgres row present");
        assert!(
            selected_row.contains("▶ "),
            "selected row must show ▶; got {selected_row:?}"
        );
    }

    #[test]
    fn first_sample_cpu_none_renders_as_em_dash() {
        let scope = scope_with_processes(ProcessesViewState::Available {
            rows: vec![
                process_row(200, "web", None, 8_388_608),
                process_row(300, "postgres", None, 16_777_216),
            ],
            source: "sysinfo".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        // The em-dash character `—` must appear in the rendered rows
        // for each cpu_percent: None process. Rendering as `0.0` would
        // misleadingly suggest "idle" on first sample.
        let em_dash_count = rows
            .iter()
            .filter(|r| r.contains("—") && !r.contains("stream: ended"))
            .count();
        assert!(
            em_dash_count >= 2,
            "each process row with cpu_percent: None must render em-dash; \
             got rows {rows:?}"
        );
        assert!(
            !any_row_contains(&rows, "0.0"),
            "None must not render as 0.0% — that would mask \"not yet measured\""
        );
    }

    #[test]
    fn measured_cpu_renders_as_number_not_em_dash() {
        let scope = scope_with_processes(ProcessesViewState::Available {
            rows: vec![process_row(200, "nginx", Some(12.5), 8_388_608)],
            source: "sysinfo".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(
            any_row_contains(&rows, "12.5"),
            "Some(12.5) must render as 12.5, not em-dash; got {rows:?}"
        );
    }

    #[test]
    fn ports_unavailable_renders_reason_verbatim() {
        let reason = "ports probe failed: /proc/net/tcp permission denied";
        let scope = scope_with_ports(PortsViewState::Unavailable {
            reason: reason.into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "Ports probe unavailable"));
        assert!(
            any_row_contains(&rows, "permission denied"),
            "verbatim reason must render; got {rows:?}"
        );
    }

    #[test]
    fn processes_unavailable_renders_reason() {
        let scope = scope_with_processes(ProcessesViewState::Unavailable {
            reason: "sysinfo refresh panicked: signal 11".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "Processes probe unavailable"));
        assert!(any_row_contains(&rows, "sysinfo refresh panicked"));
    }

    #[test]
    fn empty_ports_list_shows_no_listeners_message() {
        let scope = scope_with_ports(PortsViewState::Available {
            rows: Vec::new(),
            source: "linux-procfs".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "No listening ports"));
    }

    #[test]
    fn empty_processes_list_shows_no_processes_message() {
        let scope = scope_with_processes(ProcessesViewState::Available {
            rows: Vec::new(),
            source: "sysinfo".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(any_row_contains(&rows, "No running processes"));
    }

    #[test]
    fn help_bar_on_ports_view_shows_processes_toggle_hint() {
        let scope = scope_with_ports(PortsViewState::Available {
            rows: vec![port("tcp", 8080, 100, "nginx", None)],
            source: "test".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(
            any_row_contains(&rows, "[p] Processes"),
            "Ports help bar must advertise the Processes toggle; got {rows:?}"
        );
    }

    #[test]
    fn help_bar_on_processes_view_shows_ports_toggle_hint() {
        let scope = scope_with_processes(ProcessesViewState::Available {
            rows: vec![process_row(200, "nginx", Some(1.0), 8_388_608)],
            source: "sysinfo".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(
            any_row_contains(&rows, "[p] Ports"),
            "Processes help bar must advertise the Ports toggle; got {rows:?}"
        );
    }

    #[test]
    fn filter_bar_shows_caret_when_active() {
        let mut scope = scope_with_ports(PortsViewState::Available {
            rows: vec![port("tcp", 8080, 100, "nginx", None)],
            source: "test".into(),
        });
        scope.ports.filter = "ng".into();
        scope.ports.filter_active = true;
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(
            any_row_contains(&rows, "filter: ng_"),
            "filter bar must render a trailing caret while active; got {rows:?}"
        );
    }

    #[test]
    fn partial_row_shows_question_mark_cue() {
        let mut p = port("tcp", 8080, 0, "", None);
        p.partial = true;
        let scope = scope_with_ports(PortsViewState::Available {
            rows: vec![p],
            source: "test".into(),
        });
        let rows = draw_and_rows(&scope, 120, 30);
        assert!(
            any_row_contains(&rows, "? tcp"),
            "partial: true must render a `?` cue next to the protocol cell"
        );
    }
}
