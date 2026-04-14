//! Phase 5 Slice 5b: SSH Fleet tile renderer.
//!
//! Mirrors the layout of `scope::docker` and `scope::ports` — three-state
//! lifecycle (Connecting / Available), filter bar on top, table in the
//! middle, help bar at the bottom. The Fleet tile has no "toggle" sub-
//! view (unlike Ports/Processes), and no `Unavailable` state — a
//! discovery failure surfaces as Available with zero hosts plus an
//! error-labeled source string.
//!
//! State marker column renders Q6's four-state glyph set (plus two
//! Phase 6 future states):
//!
//! - `●` green — `Connected`
//! - `◐` yellow — `Connecting` / `Degraded` (transient)
//! - `○` gray — `Disconnected`
//! - `⚠` red — `AuthFailed` / `HostKeyMismatch` / Phase 6 agent errors
//!
//! 5b emits only `Disconnected` — 5c's connection supervisor drives the
//! full state machine through the other glyphs.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use tepegoz_proto::{HostEntry, HostState};

use crate::app::{FleetScope, FleetScopeState};

pub(crate) fn render(scope: &FleetScope, frame: &mut Frame<'_>, area: Rect, focused: bool) {
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
        .title("fleet")
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

    match &scope.state {
        FleetScopeState::Connecting => render_connecting(frame, inner),
        FleetScopeState::Available {
            hosts,
            states,
            source,
        } => render_available(scope, hosts, states, source, frame, inner),
    }
}

fn render_connecting(frame: &mut Frame<'_>, inner: Rect) {
    let p = Paragraph::new(Line::from(Span::styled(
        "Discovering SSH hosts…",
        Style::default().fg(Color::Yellow),
    )))
    .alignment(Alignment::Center);
    frame.render_widget(p, inner);
}

fn render_available(
    scope: &FleetScope,
    hosts: &[HostEntry],
    states: &std::collections::HashMap<String, HostState>,
    source: &str,
    frame: &mut Frame<'_>,
    inner: Rect,
) {
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

    let visible: Vec<&HostEntry> = hosts.iter().filter(|h| scope.matches_filter(h)).collect();

    render_status_bar(hosts.len(), visible.len(), source, frame, chunks[0]);

    let (body_idx, help_idx) = if show_filter_bar {
        render_filter_bar(&scope.filter, frame, chunks[1]);
        (2, 3)
    } else {
        (1, 2)
    };

    if visible.is_empty() {
        let msg = if hosts.is_empty() {
            "No SSH hosts configured — add entries to ~/.ssh/config or set TEPEGOZ_SSH_HOSTS"
        } else {
            "No hosts match filter"
        };
        let p = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(p, chunks[body_idx]);
    } else {
        render_host_table(scope, &visible, states, frame, chunks[body_idx]);
    }

    render_help_bar(scope.filter_active, frame, chunks[help_idx]);
}

fn render_status_bar(
    total: usize,
    visible: usize,
    source: &str,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let procs_placeholder = Span::styled(
        " procs: — (Phase 6)",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    );
    let mut spans = vec![
        Span::styled(
            format!("{visible}/{total} hosts"),
            Style::default().fg(Color::Green),
        ),
        Span::raw("  "),
        Span::styled(
            format!("source: {source}"),
            Style::default().fg(Color::DarkGray),
        ),
        procs_placeholder,
    ];
    // Shift the alignment so long source labels truncate gracefully
    // rather than pushing the procs hint off-screen.
    spans[2] = Span::styled(
        format!("source: {}", short_source(source, area.width as usize)),
        Style::default().fg(Color::DarkGray),
    );
    let p = Paragraph::new(Line::from(spans));
    frame.render_widget(p, area);
}

fn short_source(s: &str, max: usize) -> String {
    if s.len() <= max.saturating_sub(24) {
        return s.to_string();
    }
    let budget = max.saturating_sub(30);
    if budget < 8 {
        return s.chars().take(8).collect();
    }
    format!("…{}", &s[s.len().saturating_sub(budget)..])
}

fn render_filter_bar(filter: &str, frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(vec![
        Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{filter}_"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_host_table(
    scope: &FleetScope,
    visible: &[&HostEntry],
    states: &std::collections::HashMap<String, HostState>,
    frame: &mut Frame<'_>,
    area: Rect,
) {
    let rows = visible.iter().enumerate().map(|(i, h)| {
        let selected = i == scope.selection;
        let marker = if selected { "▶" } else { " " };
        let state = states
            .get(&h.alias)
            .copied()
            .unwrap_or(HostState::Disconnected);
        let (glyph, glyph_color) = state_glyph(state);
        let endpoint = if h.port == 22 {
            h.hostname.clone()
        } else {
            format!("{}:{}", h.hostname, h.port)
        };
        Row::new([
            Cell::from(marker),
            Cell::from(Span::styled(
                glyph.to_string(),
                Style::default().fg(glyph_color),
            )),
            Cell::from(h.alias.clone()),
            Cell::from(Span::styled(endpoint, Style::default().fg(Color::DarkGray))),
            Cell::from(h.user.clone()),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),  // selection marker
            Constraint::Length(2),  // state glyph
            Constraint::Min(10),    // alias
            Constraint::Length(24), // endpoint
            Constraint::Length(12), // user
        ],
    )
    .column_spacing(1);
    frame.render_widget(table, area);
}

fn render_help_bar(filter_active: bool, frame: &mut Frame<'_>, area: Rect) {
    let text = if filter_active {
        "[Enter] apply · [Esc] clear · [Backspace] delete"
    } else {
        "[j/k] nav · [/] filter · Ctrl-b h/j/k/l focus"
    };
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(p, area);
}

/// Fleet-row state glyph + color per the Q6 four-state scheme.
fn state_glyph(state: HostState) -> (&'static str, Color) {
    match state {
        HostState::Connected => ("●", Color::Green),
        HostState::Connecting | HostState::Degraded => ("◐", Color::Yellow),
        HostState::Disconnected => ("○", Color::DarkGray),
        HostState::AuthFailed
        | HostState::HostKeyMismatch
        | HostState::AgentNotDeployed
        | HostState::AgentVersionMismatch => ("⚠", Color::Red),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{FleetScope, FleetScopeState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;

    fn render_to_string(scope: &FleetScope, focused: bool) -> String {
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 60, 16);
                render(scope, f, area, focused);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn scope_with(
        hosts: Vec<HostEntry>,
        states: HashMap<String, HostState>,
        source: &str,
    ) -> FleetScope {
        FleetScope {
            state: FleetScopeState::Available {
                hosts,
                states,
                source: source.to_string(),
            },
            selection: 0,
            filter: String::new(),
            filter_active: false,
            sub_id: 1,
        }
    }

    fn host(alias: &str) -> HostEntry {
        HostEntry {
            alias: alias.into(),
            hostname: format!("{alias}.box"),
            user: "alice".into(),
            port: 22,
            identity_files: vec![],
            proxy_jump: None,
        }
    }

    #[test]
    fn connecting_renders_discovering_hint() {
        let scope = FleetScope {
            state: FleetScopeState::Connecting,
            selection: 0,
            filter: String::new(),
            filter_active: false,
            sub_id: 1,
        };
        let out = render_to_string(&scope, true);
        assert!(out.contains("Discovering"));
    }

    #[test]
    fn empty_host_list_shows_first_run_hint() {
        let scope = scope_with(vec![], HashMap::new(), "(none)");
        let out = render_to_string(&scope, false);
        assert!(
            out.contains("No SSH hosts") || out.contains("~/.ssh/config"),
            "empty host list should render the first-run hint, got:\n{out}"
        );
    }

    #[test]
    fn filtered_empty_list_shows_no_match_hint() {
        let mut states = HashMap::new();
        states.insert("staging".to_string(), HostState::Disconnected);
        let mut scope = scope_with(vec![host("staging")], states, "ssh_config");
        scope.filter = "nope".to_string();
        let out = render_to_string(&scope, false);
        assert!(out.contains("No hosts match filter"));
    }

    #[test]
    fn available_renders_one_row_per_host_with_selection_marker() {
        let mut states = HashMap::new();
        states.insert("staging".into(), HostState::Disconnected);
        states.insert("dev-eu".into(), HostState::Disconnected);
        let scope = scope_with(vec![host("staging"), host("dev-eu")], states, "ssh_config");
        let out = render_to_string(&scope, true);
        assert!(out.contains("staging"));
        assert!(out.contains("dev-eu"));
        assert!(out.contains("▶"), "selection marker should appear");
    }

    #[test]
    fn state_glyphs_distinguish_all_four_ui_states() {
        let mut states = HashMap::new();
        states.insert("a".into(), HostState::Connected);
        states.insert("b".into(), HostState::Connecting);
        states.insert("c".into(), HostState::Disconnected);
        states.insert("d".into(), HostState::AuthFailed);
        let scope = scope_with(
            vec![host("a"), host("b"), host("c"), host("d")],
            states,
            "ssh_config",
        );
        let out = render_to_string(&scope, true);
        assert!(out.contains("●"), "Connected → ●");
        assert!(out.contains("◐"), "Connecting → ◐");
        assert!(out.contains("○"), "Disconnected → ○");
        assert!(out.contains("⚠"), "AuthFailed → ⚠");
    }

    #[test]
    fn footer_help_bar_shows_nav_and_filter_hints() {
        let mut states = HashMap::new();
        states.insert("h".into(), HostState::Disconnected);
        let scope = scope_with(vec![host("h")], states, "ssh_config");
        let out = render_to_string(&scope, true);
        assert!(out.contains("[j/k] nav"));
        assert!(out.contains("[/] filter"));
    }

    #[test]
    fn filter_active_swaps_help_bar_to_filter_keybinds() {
        let mut states = HashMap::new();
        states.insert("h".into(), HostState::Disconnected);
        let mut scope = scope_with(vec![host("h")], states, "ssh_config");
        scope.filter_active = true;
        let out = render_to_string(&scope, true);
        assert!(out.contains("[Enter] apply"));
        assert!(out.contains("[Esc] clear"));
    }

    #[test]
    fn procs_column_is_em_dash_placeholder_in_phase_5() {
        let mut states = HashMap::new();
        states.insert("h".into(), HostState::Disconnected);
        let scope = scope_with(vec![host("h")], states, "ssh_config");
        let out = render_to_string(&scope, true);
        // Phase 6 will fill this column; for now it's an honest em-dash.
        assert!(
            out.contains("—"),
            "procs column should render em-dash placeholder in Phase 5"
        );
    }
}
