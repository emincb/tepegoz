//! Pure state-machine for the TUI.
//!
//! [`App`] holds every piece of client-side state: the tile layout + focus,
//! the pty vt100 parser, the docker scope, subscription ids, and any
//! in-flight one-shot requests. The single mutator [`App::handle_event`]
//! takes an [`AppEvent`] and returns zero-or-more [`AppAction`]s the I/O
//! runtime ([`crate::session::AppRuntime`]) executes.
//!
//! View shape (per `docs/DECISIONS.md#7`): the god-view is a fixed tiled
//! layout. All scopes render simultaneously; all subscriptions live
//! concurrently for the life of the session. Focus moves between tiles
//! via `Ctrl-b h/j/k/l` (+ arrow keys); the focused tile owns the
//! keystroke stream, unfocused tiles continue to update live.
//!
//! The pure-function shape (state, event → state', actions) is kept
//! from C1 for testability and for inheritance: Phase 4 (Ports/
//! Processes), 5 (SSH remote pty), 7 (port scanner), and 9 (Claude Code)
//! all plug into this same shape — add a `TileKind::Scope(ScopeKind::X)`,
//! add a per-scope state struct, route subscription envelopes via
//! [`App::handle_daemon_envelope`], and the tile slot already exists as
//! a labeled placeholder during development.

use std::collections::HashMap;

use ratatui::layout::Rect;
use tepegoz_proto::{
    DockerActionOutcome, DockerContainer, Envelope, ErrorInfo, Event, EventFrame, PROTOCOL_VERSION,
    PaneId, Payload, Subscription,
};
use vt100::Parser;

use crate::input::{InputAction, InputFilter};
use crate::tile::{FocusDir, TileId, TileLayout};

/// Scrollback budget for the vt100 parser, in rows. Mirrors the daemon's
/// 2 MiB scrollback ring in terms of practical replay depth; `1000` rows
/// × ~200 bytes/row ≈ 200 KiB in parser memory, well under the daemon's
/// 2 MiB.
const VT100_SCROLLBACK_ROWS: usize = 1000;

/// Fallback pty-tile dimensions when the layout has no `TileId::Pty`
/// (tiny-terminal fallback). `vt100::Parser::new` panics on zero, so we
/// need non-zero defaults even when the pty tile isn't rendered.
const FALLBACK_PTY_ROWS: u16 = 24;
const FALLBACK_PTY_COLS: u16 = 80;

/// Which scope panel a tile hosts. Slice C1.5 has only `Docker`; Phases
/// 4 / 5 / 7 / 9 extend this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKind {
    Docker,
}

/// TUI view state: fixed tile layout + the id of the focused tile.
#[derive(Debug)]
pub(crate) struct View {
    pub layout: TileLayout,
    pub focused: TileId,
}

impl View {
    fn new(area: Rect) -> Self {
        let layout = TileLayout::default_for(area);
        let focused = layout.default_focus;
        Self { layout, focused }
    }
}

/// Inputs to [`App::handle_event`]. Every external happening — keystroke,
/// daemon frame, signal, timer — funnels through this enum.
#[derive(Debug)]
pub(crate) enum AppEvent {
    /// Raw bytes read from stdin.
    StdinChunk(Vec<u8>),
    /// An envelope decoded from the daemon socket.
    DaemonEnvelope(Envelope),
    /// SIGWINCH; terminal reports new dimensions.
    Resize { rows: u16, cols: u16 },
    /// 30 Hz redraw tick. Always-on in C1.5+ (no mode gating).
    Tick,
    /// A pending one-shot request (e.g. `DockerAction`) hit its deadline.
    /// C3 wires this; the variant exists now so the runtime's loop shape
    /// doesn't need a second refactor when the sweeper arrives.
    #[allow(dead_code)]
    PendingActionTimeout(u64),
}

/// Side effects emitted by [`App::handle_event`]. The runtime executes
/// these in order; the App itself never touches I/O.
#[derive(Debug)]
pub(crate) enum AppAction {
    /// Send an envelope to the daemon over the writer mpsc.
    SendEnvelope(Envelope),
    /// Request a ratatui redraw of the tile grid.
    DrawFrame,
    /// Focus moved to `TileId`. The App has already updated
    /// `self.view.focused`; this action is observational — the runtime
    /// may use it for debug logging or future side effects (e.g. OSC 0
    /// title refresh). No-op at the runtime level in C1.5.
    FocusTile(TileId),
    /// Detach gracefully — exit the runtime loop.
    Detach(DetachReason),
    /// Surface a one-line status/error to the user. The runtime stubs
    /// this as `tracing::warn!`/`info!` until C3 implements the overlay.
    ShowToast { kind: ToastKind, message: String },
}

/// Severity / classification for [`AppAction::ShowToast`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Info/Success produced by C3 once actions exist
pub(crate) enum ToastKind {
    Info,
    Success,
    Error,
}

/// Why the App is asking the runtime to leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DetachReason {
    /// User pressed `Ctrl-b d` / `Ctrl-b q`.
    User,
    /// The pane's child process exited; nothing to attach to.
    PaneExited { exit_code: Option<i32> },
}

/// Per-scope state for the docker panel.
#[derive(Debug)]
pub(crate) struct DockerScope {
    pub(crate) state: DockerScopeState,
    /// Index into the visible (filter-respecting) row set. Clamped on
    /// every `ContainerList` update.
    pub(crate) selection: usize,
    pub(crate) filter: String,
    /// True while the filter bar has focus (user typed `/`). While
    /// active: chars append, backspace trims, Esc clears + deactivates,
    /// Enter deactivates but keeps the filter applied.
    pub(crate) filter_active: bool,
    /// Subscription id for `Subscribe(Docker)`. Allocated once at
    /// [`App::new`] and never cleared — the tile is always subscribed.
    pub(crate) sub_id: u64,
}

impl DockerScope {
    fn new(sub_id: u64) -> Self {
        Self {
            // Subscribe is sent in initial_actions, so we open at
            // Connecting rather than Idle — there's no "haven't
            // subscribed yet" moment the user can observe.
            state: DockerScopeState::Connecting,
            selection: 0,
            filter: String::new(),
            filter_active: false,
            sub_id,
        }
    }

    /// True if `c` passes the current filter (name or image contains
    /// the filter text, case-insensitive). Empty filter matches
    /// everything.
    pub(crate) fn matches_filter(&self, c: &DockerContainer) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        c.names.iter().any(|n| n.to_lowercase().contains(&q)) || c.image.to_lowercase().contains(&q)
    }

    /// Number of containers the renderer would show (respects the
    /// filter). `0` when not in `Available` state.
    pub(crate) fn visible_count(&self) -> usize {
        match &self.state {
            DockerScopeState::Available { containers, .. } => {
                containers.iter().filter(|c| self.matches_filter(c)).count()
            }
            _ => 0,
        }
    }

    /// Clamp `selection` into `[0, visible_count)` (or `0` when empty).
    /// Call after any state/filter change that can shrink the visible
    /// set.
    fn clamp_selection(&mut self) {
        let n = self.visible_count();
        if n == 0 {
            self.selection = 0;
        } else if self.selection >= n {
            self.selection = n - 1;
        }
    }
}

/// Three-state lifecycle for the docker scope panel. Distinct visual
/// states — don't conflate "haven't heard yet" with "engine said no
/// containers" with "engine unreachable".
#[derive(Debug)]
pub(crate) enum DockerScopeState {
    /// Pre-subscription. Kept as an enum variant for completeness; in
    /// practice the App opens at `Connecting` because Subscribe is in
    /// `initial_actions`.
    #[allow(dead_code)]
    Idle,
    /// We sent `Subscribe(Docker)` but no event has arrived yet.
    Connecting,
    /// First (or refreshed) `ContainerList` arrived. May still be empty
    /// (no containers) — renderer distinguishes empty from unavailable.
    Available {
        containers: Vec<DockerContainer>,
        engine_source: String,
    },
    /// Engine is unreachable. `reason` is verbatim from the daemon.
    Unavailable { reason: String },
}

/// Pending one-shot request awaiting a response from the daemon. Slice
/// C3 uses this for `DockerAction → DockerActionResult` correlation.
#[derive(Debug)]
#[allow(dead_code)] // C3 fills in the consumers
pub(crate) struct PendingAction {
    pub(crate) deadline: std::time::Instant,
    pub(crate) description: String,
}

/// Semantic key events parsed from raw stdin bytes when the Docker
/// tile is focused. C3 adds `Char` variants for `r`/`s`/`K`/`X`/`l`
/// lifecycle actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKey {
    Up,
    Down,
    Top,
    Bottom,
    Home,
    End,
    FilterStart,
    Escape,
    Enter,
    Backspace,
    Char(u8),
}

/// State-machine parser for stdin bytes → [`ScopeKey`]s inside a scope
/// tile. CSI sequences (arrows, Home/End) can span multiple reads; the
/// parser buffers across calls.
#[derive(Debug, Default)]
pub(crate) struct ScopeKeyParser {
    state: KeyParserState,
}

#[derive(Debug, Default)]
enum KeyParserState {
    #[default]
    Normal,
    /// Received ESC; next byte disambiguates standalone Escape vs CSI.
    Escape,
    /// Received `ESC [`; accumulating CSI parameter bytes until a
    /// final byte arrives.
    Csi(Vec<u8>),
}

impl ScopeKeyParser {
    pub(crate) fn parse(&mut self, bytes: &[u8]) -> Vec<ScopeKey> {
        let mut out = Vec::new();
        for &b in bytes {
            match std::mem::take(&mut self.state) {
                KeyParserState::Normal => match b {
                    0x1b => self.state = KeyParserState::Escape,
                    0x7f | 0x08 => out.push(ScopeKey::Backspace),
                    b'\n' | b'\r' => out.push(ScopeKey::Enter),
                    other => out.push(ScopeKey::Char(other)),
                },
                KeyParserState::Escape => match b {
                    b'[' => self.state = KeyParserState::Csi(Vec::new()),
                    0x1b => {
                        out.push(ScopeKey::Escape);
                        self.state = KeyParserState::Escape;
                    }
                    other => {
                        out.push(ScopeKey::Escape);
                        match other {
                            0x7f | 0x08 => out.push(ScopeKey::Backspace),
                            b'\n' | b'\r' => out.push(ScopeKey::Enter),
                            c => out.push(ScopeKey::Char(c)),
                        }
                    }
                },
                KeyParserState::Csi(mut accum) => match b {
                    b'A' => out.push(ScopeKey::Up),
                    b'B' => out.push(ScopeKey::Down),
                    b'C' | b'D' => {
                        // Left/Right arrows: no horizontal navigation
                        // inside the docker list. Silently drop.
                    }
                    b'H' => out.push(ScopeKey::Home),
                    b'F' => out.push(ScopeKey::End),
                    b'~' => match accum.as_slice() {
                        b"1" | b"7" => out.push(ScopeKey::Home),
                        b"4" | b"8" => out.push(ScopeKey::End),
                        _ => {}
                    },
                    b'0'..=b'9' | b';' => {
                        accum.push(b);
                        self.state = KeyParserState::Csi(accum);
                        continue;
                    }
                    _ => {} // unknown final — abandon sequence
                },
            }
        }

        // Lone ESC at the end of a chunk → standalone Escape press.
        if matches!(self.state, KeyParserState::Escape) {
            out.push(ScopeKey::Escape);
            self.state = KeyParserState::Normal;
        }
        out
    }
}

/// The pure state machine.
pub(crate) struct App {
    pub(crate) view: View,
    pub(crate) pane: PaneId,
    /// Stable subscription id for the pty. Allocated at [`App::new`];
    /// the subscription lives for the entire session.
    pub(crate) pane_sub: u64,
    /// vt100 terminal parser for the pty. Bytes arriving via
    /// `Event::PaneOutput` / `Event::PaneSnapshot` feed the parser; the
    /// pty tile renderer reads `parser.screen()` and projects cells into
    /// ratatui.
    pub(crate) pty_parser: Parser,
    pub(crate) docker: DockerScope,
    pub(crate) terminal_size: (u16, u16),
    /// Sub-id allocator. Client-chosen, monotonically increasing.
    next_sub_id: u64,
    /// In-flight one-shot requests. C3 uses this for action correlation.
    #[allow(dead_code)]
    pub(crate) pending_actions: HashMap<u64, PendingAction>,
    input_filter: InputFilter,
    scope_key_parser: ScopeKeyParser,
}

impl App {
    pub(crate) fn new(pane: PaneId, terminal_size: (u16, u16)) -> Self {
        let (rows, cols) = terminal_size;
        let area = Rect::new(0, 0, cols, rows);
        let view = View::new(area);

        let (pty_rows, pty_cols) = pty_tile_dims(&view.layout);
        let pty_parser = Parser::new(pty_rows, pty_cols, VT100_SCROLLBACK_ROWS);

        let mut next_sub_id: u64 = 1;
        let pane_sub = next_sub_id;
        next_sub_id += 1;
        let docker_sub = next_sub_id;
        next_sub_id += 1;

        Self {
            view,
            pane,
            pane_sub,
            pty_parser,
            docker: DockerScope::new(docker_sub),
            terminal_size,
            next_sub_id,
            pending_actions: HashMap::new(),
            input_filter: InputFilter::new(),
            scope_key_parser: ScopeKeyParser::default(),
        }
    }

    /// Bootstrap actions for a fresh session: AttachPane, ResizePane
    /// (sized to the pty tile, not the whole terminal), Subscribe
    /// (Docker). All subscriptions are always-on for the life of the
    /// TUI; no mode switching.
    pub(crate) fn initial_actions(&mut self) -> Vec<AppAction> {
        let (pty_rows, pty_cols) = pty_tile_dims(&self.view.layout);
        vec![
            AppAction::SendEnvelope(envelope(Payload::AttachPane {
                pane_id: self.pane,
                subscription_id: self.pane_sub,
            })),
            AppAction::SendEnvelope(envelope(Payload::ResizePane {
                pane_id: self.pane,
                rows: pty_rows,
                cols: pty_cols,
            })),
            AppAction::SendEnvelope(envelope(Payload::Subscribe(Subscription::Docker {
                id: self.docker.sub_id,
            }))),
            AppAction::DrawFrame,
        ]
    }

    /// Single mutator: take an event, evolve state, emit zero-or-more
    /// side-effect actions for the runtime to execute. Pure; no I/O.
    pub(crate) fn handle_event(&mut self, event: AppEvent) -> Vec<AppAction> {
        let mut actions = Vec::new();
        match event {
            AppEvent::StdinChunk(bytes) => self.handle_stdin(&bytes, &mut actions),
            AppEvent::DaemonEnvelope(env) => self.handle_daemon_envelope(env, &mut actions),
            AppEvent::Resize { rows, cols } => self.handle_resize(rows, cols, &mut actions),
            AppEvent::Tick => actions.push(AppAction::DrawFrame),
            AppEvent::PendingActionTimeout(_id) => {
                // C3 wires this. Empty arm locks the event surface.
            }
        }
        actions
    }

    fn handle_stdin(&mut self, bytes: &[u8], actions: &mut Vec<AppAction>) {
        for input_action in self.input_filter.process(bytes) {
            match input_action {
                InputAction::Forward(b) => self.handle_forward_bytes(b, actions),
                InputAction::Detach => {
                    actions.push(AppAction::Detach(DetachReason::User));
                    return;
                }
                InputAction::FocusDirection(dir) => self.handle_focus_direction(dir, actions),
                InputAction::Help => {
                    // C3 implements the help overlay. C1.5 keeps
                    // Ctrl-b ? as a no-op so C3 can wire the overlay
                    // without renaming anything.
                }
            }
        }
    }

    fn handle_forward_bytes(&mut self, bytes: Vec<u8>, actions: &mut Vec<AppAction>) {
        if self.view.layout.routes_to_pty(self.view.focused) {
            actions.push(AppAction::SendEnvelope(envelope(Payload::SendInput {
                pane_id: self.pane,
                data: bytes,
            })));
            return;
        }
        if let Some(ScopeKind::Docker) = self.view.layout.routes_to_scope(self.view.focused) {
            for key in self.scope_key_parser.parse(&bytes) {
                self.handle_scope_key(key, actions);
            }
        }
        // Placeholder or TooSmall fall through: drop the bytes. The
        // tile renderer shows a "not yet implemented" hint; no action
        // needed here.
    }

    fn handle_focus_direction(&mut self, dir: FocusDir, actions: &mut Vec<AppAction>) {
        if let Some(next) = self.view.layout.next_focus(self.view.focused, dir) {
            if next != self.view.focused {
                self.view.focused = next;
                actions.push(AppAction::FocusTile(next));
                actions.push(AppAction::DrawFrame);
            }
        }
    }

    fn handle_scope_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        if self.docker.filter_active {
            match key {
                ScopeKey::Escape => {
                    self.docker.filter.clear();
                    self.docker.filter_active = false;
                    self.docker.clamp_selection();
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Enter => {
                    self.docker.filter_active = false;
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Backspace => {
                    if self.docker.filter.pop().is_some() {
                        self.docker.clamp_selection();
                        actions.push(AppAction::DrawFrame);
                    }
                }
                ScopeKey::Char(b) => {
                    if (0x20..=0x7e).contains(&b) {
                        self.docker.filter.push(b as char);
                        self.docker.clamp_selection();
                        actions.push(AppAction::DrawFrame);
                    }
                }
                ScopeKey::Up
                | ScopeKey::Down
                | ScopeKey::Home
                | ScopeKey::End
                | ScopeKey::Top
                | ScopeKey::Bottom
                | ScopeKey::FilterStart => {}
            }
            return;
        }

        match key {
            ScopeKey::Up => {
                self.docker.selection = self.docker.selection.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Down => {
                let n = self.docker.visible_count();
                if n > 0 && self.docker.selection + 1 < n {
                    self.docker.selection += 1;
                }
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Top | ScopeKey::Home => {
                self.docker.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Bottom | ScopeKey::End => {
                let n = self.docker.visible_count();
                self.docker.selection = n.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::FilterStart => {
                self.docker.filter_active = true;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Char(b'j') => self.handle_scope_key(ScopeKey::Down, actions),
            ScopeKey::Char(b'k') => self.handle_scope_key(ScopeKey::Up, actions),
            ScopeKey::Char(b'g') => self.handle_scope_key(ScopeKey::Top, actions),
            ScopeKey::Char(b'G') => self.handle_scope_key(ScopeKey::Bottom, actions),
            ScopeKey::Char(b'/') => self.handle_scope_key(ScopeKey::FilterStart, actions),
            ScopeKey::Escape => {}
            ScopeKey::Enter => {} // C3 (Slice D) uses this for exec.
            ScopeKey::Backspace | ScopeKey::Char(_) => {}
        }
    }

    fn handle_daemon_envelope(&mut self, env: Envelope, actions: &mut Vec<AppAction>) {
        match env.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event,
            }) => {
                if subscription_id == self.pane_sub {
                    self.handle_pane_event(event, actions);
                } else if subscription_id == self.docker.sub_id {
                    self.handle_docker_event(event, actions);
                }
                // Unknown sub id: stale event from a sub we've closed.
                // Drop silently.
            }
            Payload::Error(info) => {
                actions.push(daemon_error_toast(&info));
            }
            Payload::DockerActionResult(result) => {
                if let DockerActionOutcome::Failure { reason } = &result.outcome {
                    actions.push(AppAction::ShowToast {
                        kind: ToastKind::Error,
                        message: format!("{:?} failed: {reason}", result.kind),
                    });
                }
                // Success toasts are C3 — they need pending_actions
                // context ("Restarted nginx") rather than a bare
                // "succeeded."
            }
            // Welcome, Pong, PaneOpened, PaneList — consumed by the
            // handshake / ensure_pane reads, not the event loop.
            _ => {}
        }
    }

    fn handle_pane_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::PaneSnapshot { scrollback, .. } => {
                if !scrollback.is_empty() {
                    self.pty_parser.process(&scrollback);
                    actions.push(AppAction::DrawFrame);
                }
            }
            Event::PaneOutput { data } => {
                self.pty_parser.process(&data);
                actions.push(AppAction::DrawFrame);
            }
            Event::PaneExit { exit_code } => {
                actions.push(AppAction::Detach(DetachReason::PaneExited { exit_code }));
            }
            Event::PaneLagged { .. } => {
                // Visual lag indicator is future work; runtime logs
                // warn on the transport side.
            }
            _ => {}
        }
    }

    fn handle_docker_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::ContainerList {
                containers,
                engine_source,
            } => {
                self.docker.state = DockerScopeState::Available {
                    containers,
                    engine_source,
                };
                self.docker.clamp_selection();
                actions.push(AppAction::DrawFrame);
            }
            Event::DockerUnavailable { reason } => {
                self.docker.state = DockerScopeState::Unavailable { reason };
                self.docker.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            Event::DockerStreamEnded { .. } => {
                // Per-container logs/stats streams — C3 consumes these.
            }
            _ => {}
        }
    }

    fn handle_resize(&mut self, rows: u16, cols: u16, actions: &mut Vec<AppAction>) {
        self.terminal_size = (rows, cols);
        let area = Rect::new(0, 0, cols, rows);
        self.view.layout = TileLayout::default_for(area);
        // If the focused tile no longer exists in the new layout
        // (common when falling across the MIN_COLS/MIN_ROWS boundary),
        // drop back to the default focus.
        if self.view.layout.tile(self.view.focused).is_none() {
            self.view.focused = self.view.layout.default_focus;
        }

        let (pty_rows, pty_cols) = pty_tile_dims(&self.view.layout);
        self.pty_parser.screen_mut().set_size(pty_rows, pty_cols);
        actions.push(AppAction::SendEnvelope(envelope(Payload::ResizePane {
            pane_id: self.pane,
            rows: pty_rows,
            cols: pty_cols,
        })));
        actions.push(AppAction::DrawFrame);
    }

    #[allow(dead_code)] // C3 allocates per-action sub ids via this
    fn alloc_sub_id(&mut self) -> u64 {
        let id = self.next_sub_id;
        self.next_sub_id += 1;
        id
    }
}

fn pty_tile_dims(layout: &TileLayout) -> (u16, u16) {
    layout
        .rect_of(TileId::Pty)
        .map(|r| (r.height.max(1), r.width.max(1)))
        .unwrap_or((FALLBACK_PTY_ROWS, FALLBACK_PTY_COLS))
}

fn envelope(payload: Payload) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload,
    }
}

fn daemon_error_toast(info: &ErrorInfo) -> AppAction {
    AppAction::ShowToast {
        kind: ToastKind::Error,
        message: format!("daemon error {:?}: {}", info.kind, info.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tepegoz_proto::{
        DockerActionKind, DockerActionOutcome, DockerActionResult, DockerContainer, ErrorInfo,
        ErrorKind,
    };

    /// A 120×40 terminal fits the god-view layout cleanly: PTY top,
    /// Docker/Ports/Fleet in the middle row, Claude Code strip at
    /// bottom.
    fn test_app() -> App {
        App::new(7, (40, 120))
    }

    fn count<F: FnMut(&AppAction) -> bool>(actions: &[AppAction], mut pred: F) -> usize {
        actions.iter().filter(|a| pred(a)).count()
    }

    fn make_container(name: &str, image: &str, state: &str) -> DockerContainer {
        DockerContainer {
            id: format!("id-{name}"),
            names: vec![format!("/{name}")],
            image: image.into(),
            image_id: "sha256:deadbeef".into(),
            command: "cmd".into(),
            created_unix_secs: 0,
            state: state.into(),
            status: "Up".into(),
            ports: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// Populate the docker scope with a ContainerList on the correct
    /// sub id and then focus the docker tile so that scope keys (j/k/
    /// filter) route correctly.
    fn populate_docker_and_focus(app: &mut App, containers: Vec<DockerContainer>) {
        // initial_actions sends Subscribe(Docker) — we don't need to
        // actually call it for the state machine, but we do need the
        // sub_id which was allocated in App::new. Inject a
        // ContainerList on that sub.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.docker.sub_id,
                event: Event::ContainerList {
                    containers,
                    engine_source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        // Focus Docker tile: Ctrl-b j from the default (PTY) focus.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
    }

    #[test]
    fn initial_actions_emit_attach_resize_with_pty_tile_dims_and_subscribe_docker() {
        let mut app = test_app();
        let actions = app.initial_actions();
        assert_eq!(
            actions.len(),
            4,
            "initial actions: AttachPane + ResizePane + Subscribe(Docker) + DrawFrame"
        );

        // AttachPane with pane_sub.
        match &actions[0] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::AttachPane {
                    pane_id,
                    subscription_id,
                } => {
                    assert_eq!(*pane_id, 7);
                    assert_eq!(*subscription_id, app.pane_sub);
                }
                other => panic!("expected AttachPane, got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        // ResizePane with the pty tile's rows/cols — NOT the terminal
        // dims. This is the C1.5 invariant: the pane sized to fit its
        // tile, not the full terminal, so vim et al. render inside the
        // box.
        let (expected_pty_rows, expected_pty_cols) = pty_tile_dims(&app.view.layout);
        match &actions[1] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::ResizePane {
                    pane_id,
                    rows,
                    cols,
                } => {
                    assert_eq!(*pane_id, 7);
                    assert_eq!(*rows, expected_pty_rows);
                    assert_eq!(*cols, expected_pty_cols);
                    assert_ne!(
                        (*rows, *cols),
                        (40, 120),
                        "must size pane to pty tile, not terminal"
                    );
                }
                other => panic!("expected ResizePane, got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        // Subscribe(Docker) with the docker sub_id.
        match &actions[2] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::Subscribe(Subscription::Docker { id }) => {
                    assert_eq!(*id, app.docker.sub_id);
                }
                other => panic!("expected Subscribe(Docker), got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        assert!(matches!(actions[3], AppAction::DrawFrame));

        // Default view state: layout computed, PTY focused, docker
        // opens at Connecting (not Idle) because Subscribe is already
        // in-flight.
        assert_eq!(app.view.focused, TileId::Pty);
        assert!(matches!(app.docker.state, DockerScopeState::Connecting));
    }

    #[test]
    fn ctrl_b_d_emits_user_detach() {
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02d".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::Detach(DetachReason::User)
            )),
            1
        );
    }

    #[test]
    fn ctrl_b_j_from_pty_focuses_docker_and_emits_drawframe() {
        let mut app = test_app();
        assert_eq!(app.view.focused, TileId::Pty);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        assert_eq!(app.view.focused, TileId::Docker);
        let focus_count = count(&actions, |a| {
            matches!(a, AppAction::FocusTile(TileId::Docker))
        });
        assert_eq!(focus_count, 1);
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
    }

    #[test]
    fn ctrl_b_k_from_docker_focuses_pty() {
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // PTY → Docker
        assert_eq!(app.view.focused, TileId::Docker);
        app.handle_event(AppEvent::StdinChunk(b"\x02k".to_vec())); // Docker → PTY
        assert_eq!(app.view.focused, TileId::Pty);
    }

    #[test]
    fn ctrl_b_l_from_docker_focuses_ports() {
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec())); // Docker → Ports
        assert_eq!(app.view.focused, TileId::Ports);
    }

    #[test]
    fn ctrl_b_up_arrow_from_docker_focuses_pty() {
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02\x1b[A".to_vec()));
        assert_eq!(app.view.focused, TileId::Pty);
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::FocusTile(TileId::Pty))),
            1
        );
    }

    #[test]
    fn ctrl_b_h_from_pty_is_noop() {
        // PTY is full-width; nothing to the left.
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02h".to_vec()));
        assert_eq!(app.view.focused, TileId::Pty);
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::FocusTile(_))),
            0,
            "no-op focus moves must not emit FocusTile"
        );
    }

    #[test]
    fn ctrl_b_question_is_help_noop() {
        // Help is a C3 overlay; C1.5 pins Ctrl-b ? as a no-op so C3
        // can wire the overlay without renaming.
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(actions.is_empty());
    }

    #[test]
    fn pty_focused_pane_keystrokes_forward_to_daemon_as_send_input() {
        let mut app = test_app();
        // Default focus is PTY.
        let actions = app.handle_event(AppEvent::StdinChunk(b"hello\n".to_vec()));
        let send_input_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::SendInput { pane_id, data }
                        if *pane_id == 7 && data == b"hello\n")
            )
        });
        assert_eq!(send_input_count, 1);
    }

    #[test]
    fn docker_focused_stdin_routes_to_scope_key_parser() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("a", "a", "running"),
                make_container("b", "b", "running"),
                make_container("c", "c", "running"),
            ],
        );
        assert_eq!(app.view.focused, TileId::Docker);

        // Bare `j` while Docker is focused: navigates the list, NOT
        // focus movement (Ctrl-b j would be focus movement).
        assert_eq!(app.docker.selection, 0);
        let actions = app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.docker.selection, 1);
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
        // And NOT SendInput — `j` would otherwise be typed into the
        // pty, but Docker owns the keystream while focused.
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::SendEnvelope(_))),
            0
        );
    }

    #[test]
    fn placeholder_focused_stdin_is_dropped() {
        let mut app = test_app();
        // Walk focus from PTY → Docker → Ports (placeholder).
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        assert_eq!(app.view.focused, TileId::Ports);
        let actions = app.handle_event(AppEvent::StdinChunk(b"hello".to_vec()));
        // No SendInput (pty is not focused), no DrawFrame (placeholder
        // doesn't re-render on typed input).
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::SendEnvelope(_))),
            0,
            "placeholder tile must not route bytes to SendInput"
        );
    }

    #[test]
    fn container_list_transitions_state_to_available_and_clamps_selection() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.docker.sub_id,
                event: Event::ContainerList {
                    containers: vec![
                        make_container("web", "nginx", "running"),
                        make_container("db", "postgres", "running"),
                    ],
                    engine_source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        match &app.docker.state {
            DockerScopeState::Available {
                containers,
                engine_source,
            } => {
                assert_eq!(containers.len(), 2);
                assert_eq!(engine_source, "test");
            }
            other => panic!("expected Available, got {other:?}"),
        }
        assert_eq!(app.docker.visible_count(), 2);
    }

    #[test]
    fn docker_unavailable_transitions_state() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.docker.sub_id,
                event: Event::DockerUnavailable {
                    reason: "no socket".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        match &app.docker.state {
            DockerScopeState::Unavailable { reason } => assert_eq!(reason, "no socket"),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn j_and_k_move_selection_and_clamp_at_bounds() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("a", "a", "running"),
                make_container("b", "b", "running"),
                make_container("c", "c", "running"),
            ],
        );
        assert_eq!(app.docker.selection, 0);
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.docker.selection, 1);
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.docker.selection, 2);
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.docker.selection, 2, "clamp at bottom");
        app.handle_event(AppEvent::StdinChunk(b"k".to_vec()));
        assert_eq!(app.docker.selection, 1);
        app.handle_event(AppEvent::StdinChunk(b"kkk".to_vec()));
        assert_eq!(app.docker.selection, 0, "clamp at top");
    }

    #[test]
    fn arrow_down_and_up_move_selection_when_docker_focused() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("a", "a", "running"),
                make_container("b", "b", "running"),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"\x1b[B".to_vec())); // Down
        assert_eq!(app.docker.selection, 1);
        app.handle_event(AppEvent::StdinChunk(b"\x1b[A".to_vec())); // Up
        assert_eq!(app.docker.selection, 0);
    }

    #[test]
    fn capital_g_jumps_to_bottom_and_lowercase_g_to_top() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("a", "a", "running"),
                make_container("b", "b", "running"),
                make_container("c", "c", "running"),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"G".to_vec()));
        assert_eq!(app.docker.selection, 2);
        app.handle_event(AppEvent::StdinChunk(b"g".to_vec()));
        assert_eq!(app.docker.selection, 0);
    }

    #[test]
    fn filter_narrows_visible_list_and_clamps_selection() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("web", "nginx", "running"),
                make_container("db", "postgres", "running"),
                make_container("cache", "redis", "running"),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"G".to_vec()));
        assert_eq!(app.docker.selection, 2);

        app.handle_event(AppEvent::StdinChunk(b"/we".to_vec()));
        assert!(app.docker.filter_active);
        assert_eq!(app.docker.filter, "we");
        assert_eq!(app.docker.visible_count(), 1);
        assert_eq!(app.docker.selection, 0);

        app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec()));
        assert!(!app.docker.filter_active);
        assert_eq!(app.docker.filter, "");
        assert_eq!(app.docker.visible_count(), 3);
    }

    #[test]
    fn filter_enter_commits_without_clearing() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("web", "nginx", "running"),
                make_container("db", "postgres", "running"),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"/nginx\n".to_vec()));
        assert!(!app.docker.filter_active);
        assert_eq!(app.docker.filter, "nginx");
        assert_eq!(app.docker.visible_count(), 1);
    }

    #[test]
    fn filter_backspace_removes_last_char() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("a", "a", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"/abc".to_vec()));
        assert_eq!(app.docker.filter, "abc");
        app.handle_event(AppEvent::StdinChunk(b"\x7f".to_vec()));
        assert_eq!(app.docker.filter, "ab");
        app.handle_event(AppEvent::StdinChunk(b"\x08".to_vec()));
        assert_eq!(app.docker.filter, "a");
    }

    #[test]
    fn daemon_error_envelope_routes_to_show_toast() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Error(ErrorInfo {
                kind: ErrorKind::Internal,
                message: "disk full".into(),
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        let toast_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast {
                    kind: ToastKind::Error,
                    message,
                } if message.contains("disk full")
            )
        });
        assert_eq!(toast_count, 1);
    }

    #[test]
    fn docker_action_result_failure_routes_to_show_toast() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerActionResult(DockerActionResult {
                request_id: 42,
                container_id: "abc".into(),
                kind: DockerActionKind::Restart,
                outcome: DockerActionOutcome::Failure {
                    reason: "container not running".into(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        let toast_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast {
                    kind: ToastKind::Error,
                    message,
                } if message.contains("container not running")
            )
        });
        assert_eq!(toast_count, 1);
    }

    #[test]
    fn docker_action_result_success_does_not_toast_yet() {
        // Success toasts are C3 — they need pending_actions to name
        // what the user acted on ("Restarted nginx"). In C1.5 no
        // success branch.
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerActionResult(DockerActionResult {
                request_id: 42,
                container_id: "abc".into(),
                kind: DockerActionKind::Restart,
                outcome: DockerActionOutcome::Success,
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, AppAction::ShowToast { .. }))
        );
    }

    #[test]
    fn pane_output_feeds_vt100_parser_and_emits_drawframe() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.pane_sub,
                event: Event::PaneOutput {
                    data: b"hello".to_vec(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        // vt100 received the bytes: first cell should now be 'h'.
        let cell = app
            .pty_parser
            .screen()
            .cell(0, 0)
            .expect("cell (0,0) exists");
        assert_eq!(
            cell.contents(),
            "h",
            "PaneOutput bytes must flow into the vt100 parser"
        );
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
    }

    #[test]
    fn pane_snapshot_feeds_vt100_parser() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.pane_sub,
                event: Event::PaneSnapshot {
                    scrollback: b"replayed".to_vec(),
                    rows: 24,
                    cols: 80,
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        let cell = app.pty_parser.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "r");
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
    }

    #[test]
    fn pane_exit_event_emits_pane_exited_detach() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.pane_sub,
                event: Event::PaneExit {
                    exit_code: Some(42),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::Detach(DetachReason::PaneExited {
                    exit_code: Some(42)
                })
            )),
            1
        );
    }

    #[test]
    fn stale_subscription_event_is_dropped() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 99_999,
                event: Event::PaneOutput {
                    data: b"ghost".to_vec(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert!(actions.is_empty());
    }

    #[test]
    fn resize_recomputes_layout_and_sends_resizepane_with_pty_tile_dims() {
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::Resize {
            rows: 50,
            cols: 160,
        });
        assert_eq!(app.terminal_size, (50, 160));
        // Layout must be recomputed for the new terminal size.
        let pty_rect = app.view.layout.rect_of(TileId::Pty).unwrap();
        assert_eq!(pty_rect.width, 160);

        // ResizePane carries the NEW pty tile dims, not the terminal
        // dims.
        let (expected_rows, expected_cols) = pty_tile_dims(&app.view.layout);
        let resize_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::ResizePane { rows, cols, .. }
                        if *rows == expected_rows && *cols == expected_cols
                    )
            )
        });
        assert_eq!(resize_count, 1);
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
    }

    #[test]
    fn resize_below_minimum_falls_back_to_too_small_layout() {
        let mut app = test_app();
        app.handle_event(AppEvent::Resize { rows: 10, cols: 40 });
        assert_eq!(app.view.focused, TileId::TooSmall);
        assert_eq!(app.view.layout.tiles.len(), 1);
    }

    #[test]
    fn tick_always_emits_drawframe() {
        // C1.5 drops the mode-gating: the tile grid is always
        // rendered, so every tick draws.
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::Tick);
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);

        // Focusing a scope tile doesn't change the cadence.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        let actions = app.handle_event(AppEvent::Tick);
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
    }
}
