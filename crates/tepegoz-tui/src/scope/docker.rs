//! Docker scope renderer.
//!
//! Slice C1 ships a stub: a centered "Docker scope — Slice C2 incoming"
//! message with the current `DockerScopeState` discriminant in the corner
//! so the bus is visibly working end-to-end. Slice C2 replaces this with
//! the real container table (filter input, three-state lifecycle,
//! navigation, action keybinds).

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{DockerScope, DockerScopeState};

pub(crate) fn render(scope: &DockerScope, frame: &mut Frame<'_>) {
    let area = frame.area();

    let layout = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);

    // Status bar at top: shows the active state for at-a-glance debugging.
    // C2 will replace this with a proper status line including engine
    // source, refresh interval, container count.
    let status_text = match &scope.state {
        DockerScopeState::Idle => "docker scope · idle".to_string(),
        DockerScopeState::Connecting => "docker scope · connecting…".to_string(),
        DockerScopeState::Available {
            containers,
            engine_source,
        } => format!(
            "docker scope · {} container(s) · {engine_source}",
            containers.len()
        ),
        DockerScopeState::Unavailable { reason } => format!("docker scope · unavailable: {reason}"),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            status_text,
            Style::default().fg(Color::DarkGray),
        )),
        layout[0],
    );

    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Docker scope view",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::raw(
            "Slice C1 ships only the bus + view switch; C2 wires the container",
        )),
        Line::from(Span::raw("table, filter, navigation, and action keybinds.")),
        Line::from(""),
        Line::from(Span::styled(
            "Press Ctrl-b a to return to the attached pane, Ctrl-b d to detach.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL).title(" tepegöz "));

    frame.render_widget(body, layout[1]);
}
