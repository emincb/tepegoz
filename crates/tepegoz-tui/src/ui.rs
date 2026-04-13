//! Render the TUI.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, ConnectionState};

pub(crate) fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    frame.render_widget(header(app), chunks[0]);
    frame.render_widget(body(app), chunks[1]);
    frame.render_widget(footer(), chunks[2]);
}

fn header(app: &App) -> Paragraph<'_> {
    let (indicator, color) = match &app.connection {
        ConnectionState::Connecting => ("● connecting", Color::Yellow),
        ConnectionState::Connected => ("● connected", Color::Green),
        ConnectionState::Disconnected(reason) => {
            return Paragraph::new(Line::from(vec![
                Span::styled(
                    " Tepegöz ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("● disconnected — {reason}"),
                    Style::default().fg(Color::Red),
                ),
            ]));
        }
    };

    Paragraph::new(Line::from(vec![
        Span::styled(
            " Tepegöz ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(indicator, Style::default().fg(color)),
    ]))
}

fn body(app: &App) -> Paragraph<'_> {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" daemon status ");

    let lines = match &app.last_status {
        Some(snap) => {
            let mut lines = Vec::with_capacity(16);
            lines.push(Line::from(""));
            lines.push(row("daemon pid", &snap.daemon_pid.to_string()));
            lines.push(row("daemon version", &snap.daemon_version));
            lines.push(row("socket", &snap.socket_path));
            lines.push(Line::from(""));
            lines.push(row("uptime", &format_uptime(snap.uptime_seconds)));
            lines.push(Line::from(""));
            lines.push(row("clients now", &snap.clients_now.to_string()));
            lines.push(row("clients total", &snap.clients_total.to_string()));
            lines.push(row("events sent", &snap.events_sent.to_string()));
            lines
        }
        None => vec![
            Line::from(""),
            Line::from("  waiting for first snapshot..."),
        ],
    };

    Paragraph::new(lines).block(block)
}

fn row<'a>(label: &'a str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{label:<16}"), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), Style::default().fg(Color::White)),
    ])
}

fn footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit  "),
        Span::styled(" Esc ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit"),
    ]))
}

fn format_uptime(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}
