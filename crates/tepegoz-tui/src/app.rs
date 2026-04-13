//! Pure state-machine for the TUI.
//!
//! [`App`] holds the entire client-side state (current view, active pane,
//! pending subscriptions, scope-panel data) and exposes one mutator —
//! [`App::handle_event`] — that takes an [`AppEvent`] and returns a list of
//! [`AppAction`]s the I/O runtime ([`crate::session::AppRuntime`]) should
//! execute.
//!
//! The pure-function shape (state, event → state', actions) is deliberate:
//!
//! - **Testability.** State-machine tests can drive arbitrary event
//!   sequences without any sockets, terminal, or async runtime — see the
//!   `tests` module at the bottom of this file.
//! - **Inheritance.** Phases 4 (Ports/Processes), 5 (SSH remote pty), and 7
//!   (port scanner) all add new scope panels that plug into this same
//!   shape: extend [`ScopeKind`], add a per-scope state struct, route
//!   subscription envelopes via [`App::handle_daemon_envelope`].
//! - **Auditability.** Every side effect ([`AppAction`]) is enumerated in
//!   one place; the I/O runtime only ever reacts to those, so it's easy
//!   to see what the TUI can and can't do at a glance.

use std::collections::HashMap;

use tepegoz_proto::{
    DockerActionOutcome, DockerContainer, Envelope, ErrorInfo, Event, EventFrame, PROTOCOL_VERSION,
    PaneId, Payload, Subscription,
};

use crate::input::{InputAction, InputFilter};

/// Top-level UI mode. Determines which renderer runs and where stdin
/// keystrokes are routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum View {
    /// Attached to a pty pane: stdin → daemon, PaneOutput → stdout (raw
    /// passthrough; ratatui draw cycle is idle).
    Pane,
    /// Browsing a scope panel: ratatui owns rendering; navigation /
    /// action keys are parsed locally.
    Scope(ScopeKind),
}

/// Which scope panel is active. Slice C ships only `Docker`. Phases 4/7
/// add `Ports`, `Processes`, `Scan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeKind {
    Docker,
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
    /// Coalesced redraw tick (~30 Hz); meaningful only in scope mode.
    Tick,
    /// A pending one-shot request (e.g. `DockerAction`) hit its deadline.
    /// Slice C3 wires this; included now so the runtime's loop shape
    /// doesn't need a second refactor when timeouts arrive.
    #[allow(dead_code)] // emitted by C3's pending-action sweeper
    PendingActionTimeout(u64),
}

/// Side effects emitted by [`App::handle_event`]. The runtime executes
/// these in order; the App itself never touches I/O.
#[derive(Debug)]
pub(crate) enum AppAction {
    /// Send an envelope to the daemon over the writer mpsc.
    SendEnvelope(Envelope),
    /// Write bytes directly to stdout (pane-mode passthrough). Ignored if
    /// the runtime knows it's in scope mode — defensive only; the App is
    /// supposed to only emit this when the view is [`View::Pane`].
    WriteStdout(Vec<u8>),
    /// Mode-switch lifecycle: clear the screen and stop ratatui drawing.
    /// The runtime no longer calls `terminal.draw()` until [`Self::DrawScope`]
    /// arrives.
    EnterPaneMode,
    /// Mode-switch lifecycle: clear the screen and start the ratatui draw
    /// cycle. The next [`Self::DrawScope`] paints the initial scope view.
    EnterScopeMode,
    /// Request a ratatui redraw of the current scope view.
    DrawScope,
    /// Detach gracefully — exit the runtime loop. The terminal guard
    /// restores raw mode and alt-screen on the way out. Carries the
    /// reason so the runtime can pick the right exit message (user
    /// detach vs pane exit).
    Detach(DetachReason),
    /// Surface a one-line status/error to the user. In C2 the runtime
    /// stubs this as `tracing::warn!`; C3 implements a proper overlay.
    /// The action surface is defined now so handle_daemon_envelope can
    /// route `Payload::Error` + `DockerActionResult::Failure` without a
    /// second refactor when C3's overlay lands.
    ShowToast { kind: ToastKind, message: String },
}

/// Severity / classification for [`AppAction::ShowToast`]. C3 may use this
/// to pick colors or auto-dismiss timings in the overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Info/Success produced by C3 once actions exist
pub(crate) enum ToastKind {
    /// Neutral information (e.g. "subscription started"). C3 only.
    Info,
    /// Action completed successfully (e.g. "restarted nginx"). C3 only.
    Success,
    /// Something the user needs to see — daemon error, action failure.
    /// C2 emits this for `Payload::Error` + `DockerActionResult::Failure`.
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
#[derive(Debug, Default)]
pub(crate) struct DockerScope {
    pub(crate) state: DockerScopeState,
    /// Index into the visible (filter-respecting) row set. Clamped on
    /// every `ContainerList` update so it doesn't overshoot when the
    /// filter narrows or containers disappear.
    pub(crate) selection: usize,
    pub(crate) filter: String,
    /// True while the filter bar has focus (user typed `/`). While active,
    /// chars go into `filter`; backspace removes the last; Esc clears +
    /// deactivates; Enter deactivates but keeps the filter applied.
    pub(crate) filter_active: bool,
    /// Subscription id for `Subscribe(Docker)`. `Some(_)` while we're in
    /// scope view and the daemon is streaming container lists; `None`
    /// when idle or between view switches.
    pub(crate) sub_id: Option<u64>,
}

impl DockerScope {
    /// True if `c` passes the current filter (name or image contains the
    /// filter text, case-insensitive). Empty filter matches everything.
    pub(crate) fn matches_filter(&self, c: &DockerContainer) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        c.names.iter().any(|n| n.to_lowercase().contains(&q)) || c.image.to_lowercase().contains(&q)
    }

    /// Number of containers the renderer would show (respects the filter).
    /// `0` when not in `Available` state.
    pub(crate) fn visible_count(&self) -> usize {
        match &self.state {
            DockerScopeState::Available { containers, .. } => {
                containers.iter().filter(|c| self.matches_filter(c)).count()
            }
            _ => 0,
        }
    }

    /// Clamp `selection` into `[0, visible_count)` (or `0` when empty).
    /// Call after any state/filter change that can shrink the visible set.
    fn clamp_selection(&mut self) {
        let n = self.visible_count();
        if n == 0 {
            self.selection = 0;
        } else if self.selection >= n {
            self.selection = n - 1;
        }
    }
}

/// Three-state lifecycle for the docker scope panel. Per CTO §2: distinct
/// visual states. Don't conflate "haven't heard yet" with "engine said no
/// containers" with "engine unreachable".
#[derive(Debug, Default)]
pub(crate) enum DockerScopeState {
    /// We've never subscribed (initial state, or after leaving scope view).
    #[default]
    Idle,
    /// We sent `Subscribe(Docker)` but no event has arrived yet. Renderer
    /// shows "Connecting to docker engine…".
    Connecting,
    /// First (or refreshed) `ContainerList` arrived. May still be empty
    /// (no containers) — renderer distinguishes empty list from
    /// unavailability.
    Available {
        containers: Vec<DockerContainer>,
        engine_source: String,
    },
    /// Engine is unreachable. `reason` is the structured explanation from
    /// `Engine::connect`; renderer shows it verbatim.
    Unavailable { reason: String },
}

/// Pending one-shot request awaiting a response from the daemon. Slice C3
/// uses this for `DockerAction → DockerActionResult` correlation and
/// timeout sweeps.
#[derive(Debug)]
#[allow(dead_code)] // C3 fills in the consumers
pub(crate) struct PendingAction {
    pub(crate) deadline: std::time::Instant,
    pub(crate) description: String,
}

/// Semantic key events parsed out of raw stdin bytes in scope mode. Slice
/// C3 will add `Char` variants for `r`/`s`/`K`/`X`/`l`/`Enter` lifecycle
/// actions and treat these as higher-level events; for now any printable
/// byte routes to filter input when the filter bar is focused and is
/// dispatched by single-byte match otherwise.
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
    /// A single byte of raw input. In filter-input mode this feeds the
    /// filter string; otherwise (currently) unused.
    Char(u8),
}

/// State-machine parser for stdin bytes → [`ScopeKey`]s. Escape sequences
/// (arrows, Home/End) can span multiple reads, so the parser buffers
/// partial sequences across calls.
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
    /// Received `ESC [`; accumulating CSI parameter bytes until a final
    /// byte arrives.
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
                        // ESC ESC → first is a standalone Escape; second
                        // starts a new pending escape.
                        out.push(ScopeKey::Escape);
                        self.state = KeyParserState::Escape;
                    }
                    other => {
                        // ESC followed by an unrecognized byte — treat
                        // ESC as standalone Escape, dispatch the other
                        // byte as a normal key.
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
                        // Right/Left arrows: no horizontal navigation
                        // for the docker list view; silently drop.
                    }
                    b'H' => out.push(ScopeKey::Home),
                    b'F' => out.push(ScopeKey::End),
                    b'~' => match accum.as_slice() {
                        b"1" | b"7" => out.push(ScopeKey::Home),
                        b"4" | b"8" => out.push(ScopeKey::End),
                        _ => {} // unknown ~-terminated CSI
                    },
                    b'0'..=b'9' | b';' => {
                        accum.push(b);
                        self.state = KeyParserState::Csi(accum);
                        continue;
                    }
                    _ => {} // unknown final byte — abandon sequence
                },
            }
        }

        // If we ended the chunk holding a lone ESC with nothing following,
        // treat it as a standalone Escape press. Terminal chunking in
        // practice delivers full `ESC [ A` etc. in a single read (crossterm's
        // tokio::io::stdin returns as much as the kernel has; arrow-key
        // sequences are 3 contiguous bytes from xterm and never split).
        // The alternative — holding ESC forever pending — would swallow
        // the user's Esc keypress until they typed another byte.
        if matches!(self.state, KeyParserState::Escape) {
            out.push(ScopeKey::Escape);
            self.state = KeyParserState::Normal;
        }
        out
    }
}

/// The pure state machine. Owns nothing that talks to the outside world.
pub(crate) struct App {
    pub(crate) view: View,
    pub(crate) pane: PaneId,
    /// Subscription id for the active `AttachPane`. `None` between the
    /// constructor and the first call to [`App::initial_actions`], or
    /// briefly during a Scope→Pane synthetic re-attach.
    pub(crate) pane_attach_sub: Option<u64>,
    pub(crate) docker: DockerScope,
    pub(crate) terminal_size: (u16, u16),
    /// Sub-id allocator. Client-chosen, monotonically increasing. The
    /// daemon doesn't impose any structure on these — they're opaque
    /// labels for routing events back to the correct subscription.
    pub(crate) next_sub_id: u64,
    /// In-flight one-shot requests. Slice C3 uses this for action
    /// correlation + the 30 s timeout sweep.
    #[allow(dead_code)]
    pub(crate) pending_actions: HashMap<u64, PendingAction>,
    input_filter: InputFilter,
    scope_key_parser: ScopeKeyParser,
}

impl App {
    pub(crate) fn new(pane: PaneId, terminal_size: (u16, u16)) -> Self {
        Self {
            view: View::Pane,
            pane,
            pane_attach_sub: None,
            docker: DockerScope::default(),
            terminal_size,
            next_sub_id: 1,
            pending_actions: HashMap::new(),
            input_filter: InputFilter::new(),
            scope_key_parser: ScopeKeyParser::default(),
        }
    }

    /// Bootstrap actions to issue once at session start, before the event
    /// loop spins. Allocates the first pane subscription, sends
    /// `AttachPane`, and tells the daemon our terminal size.
    pub(crate) fn initial_actions(&mut self) -> Vec<AppAction> {
        let sub_id = self.alloc_sub_id();
        self.pane_attach_sub = Some(sub_id);
        let (rows, cols) = self.terminal_size;
        vec![
            AppAction::SendEnvelope(envelope(Payload::AttachPane {
                pane_id: self.pane,
                subscription_id: sub_id,
            })),
            AppAction::SendEnvelope(envelope(Payload::ResizePane {
                pane_id: self.pane,
                rows,
                cols,
            })),
        ]
    }

    /// Single mutator: take an event, evolve state, emit zero or more
    /// side-effect actions for the runtime to execute. Pure; no I/O.
    pub(crate) fn handle_event(&mut self, event: AppEvent) -> Vec<AppAction> {
        let mut actions = Vec::new();
        match event {
            AppEvent::StdinChunk(bytes) => {
                self.handle_stdin(&bytes, &mut actions);
            }
            AppEvent::DaemonEnvelope(env) => {
                self.handle_daemon_envelope(env, &mut actions);
            }
            AppEvent::Resize { rows, cols } => {
                self.terminal_size = (rows, cols);
                actions.push(AppAction::SendEnvelope(envelope(Payload::ResizePane {
                    pane_id: self.pane,
                    rows,
                    cols,
                })));
                if matches!(self.view, View::Scope(_)) {
                    actions.push(AppAction::DrawScope);
                }
            }
            AppEvent::Tick => {
                if matches!(self.view, View::Scope(_)) {
                    actions.push(AppAction::DrawScope);
                }
            }
            AppEvent::PendingActionTimeout(_id) => {
                // Slice C3 wires this. Intentional empty arm so the event
                // surface is locked in; runtime can already emit timeout
                // ticks once the C3 sweeper exists.
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
                InputAction::SwitchToScope => self.switch_to_scope(actions),
                InputAction::SwitchToPane => self.switch_to_pane(actions),
                InputAction::Help => {
                    // Slice C3 implements the help overlay. In Pane mode the
                    // C1 pinning test confirms this arm produces zero actions.
                }
            }
        }
    }

    fn handle_forward_bytes(&mut self, bytes: Vec<u8>, actions: &mut Vec<AppAction>) {
        match self.view {
            View::Pane => {
                actions.push(AppAction::SendEnvelope(envelope(Payload::SendInput {
                    pane_id: self.pane,
                    data: bytes,
                })));
            }
            View::Scope(_) => {
                for key in self.scope_key_parser.parse(&bytes) {
                    self.handle_scope_key(key, actions);
                }
            }
        }
    }

    fn handle_scope_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        if self.docker.filter_active {
            // Filter input mode. Most bytes append to `filter`; Esc/Enter
            // exit the mode; backspace trims.
            match key {
                ScopeKey::Escape => {
                    self.docker.filter.clear();
                    self.docker.filter_active = false;
                    self.docker.clamp_selection();
                    actions.push(AppAction::DrawScope);
                }
                ScopeKey::Enter => {
                    // Commit: keep the filter content applied, leave input mode.
                    self.docker.filter_active = false;
                    actions.push(AppAction::DrawScope);
                }
                ScopeKey::Backspace => {
                    if self.docker.filter.pop().is_some() {
                        self.docker.clamp_selection();
                        actions.push(AppAction::DrawScope);
                    }
                }
                ScopeKey::Char(b) => {
                    // Only printable ASCII; control bytes dropped.
                    if (0x20..=0x7e).contains(&b) {
                        self.docker.filter.push(b as char);
                        self.docker.clamp_selection();
                        actions.push(AppAction::DrawScope);
                    }
                }
                // Navigation keys while typing a filter are dropped — the
                // user is editing; let them commit (Enter) first.
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

        // Non-filter mode: navigation + filter activation.
        match key {
            ScopeKey::Up => {
                self.docker.selection = self.docker.selection.saturating_sub(1);
                actions.push(AppAction::DrawScope);
            }
            ScopeKey::Down => {
                let n = self.docker.visible_count();
                if n > 0 && self.docker.selection + 1 < n {
                    self.docker.selection += 1;
                }
                actions.push(AppAction::DrawScope);
            }
            ScopeKey::Top | ScopeKey::Home => {
                self.docker.selection = 0;
                actions.push(AppAction::DrawScope);
            }
            ScopeKey::Bottom | ScopeKey::End => {
                let n = self.docker.visible_count();
                self.docker.selection = n.saturating_sub(1);
                actions.push(AppAction::DrawScope);
            }
            ScopeKey::FilterStart => {
                self.docker.filter_active = true;
                actions.push(AppAction::DrawScope);
            }
            // j / k / g / G map to Up / Down / Top / Bottom.
            ScopeKey::Char(b'j') => self.handle_scope_key(ScopeKey::Down, actions),
            ScopeKey::Char(b'k') => self.handle_scope_key(ScopeKey::Up, actions),
            ScopeKey::Char(b'g') => self.handle_scope_key(ScopeKey::Top, actions),
            ScopeKey::Char(b'G') => self.handle_scope_key(ScopeKey::Bottom, actions),
            ScopeKey::Char(b'/') => self.handle_scope_key(ScopeKey::FilterStart, actions),
            // Esc outside filter mode: no-op for now. C3 may use it to
            // dismiss overlays.
            ScopeKey::Escape => {}
            // Enter in scope mode: Slice D uses this for "exec into
            // container". Ignored in C2c2.
            ScopeKey::Enter => {}
            // Backspace / other Chars outside filter mode: dropped.
            ScopeKey::Backspace | ScopeKey::Char(_) => {}
        }
    }

    fn handle_daemon_envelope(&mut self, env: Envelope, actions: &mut Vec<AppAction>) {
        match env.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event,
            }) => {
                if Some(subscription_id) == self.pane_attach_sub {
                    self.handle_pane_event(event, actions);
                } else if Some(subscription_id) == self.docker.sub_id {
                    self.handle_docker_event(event, actions);
                }
                // Other subscription ids: a stale event from a sub we've
                // already unsubscribed from. Drop silently.
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
                // Success: Slice C3 wires success toasts tied to the
                // pending_actions map (so the toast can reference the
                // container the user acted on). In C2 we have no actions
                // in flight from the TUI, so no success branch needed.
            }
            // Welcome, Pong, PaneOpened, PaneList — consumed by the
            // handshake / inline response reads, not the event loop.
            _ => {}
        }
    }

    fn handle_pane_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::PaneSnapshot { scrollback, .. } => {
                if matches!(self.view, View::Pane) && !scrollback.is_empty() {
                    actions.push(AppAction::WriteStdout(scrollback));
                }
            }
            Event::PaneOutput { data } => {
                if matches!(self.view, View::Pane) {
                    actions.push(AppAction::WriteStdout(data));
                }
                // Scope mode: drop. The synthetic re-attach on Scope→Pane
                // emits a fresh PaneSnapshot — that replays whatever
                // happened while we were away. Buffering here would
                // duplicate the daemon's ring buffer.
            }
            Event::PaneExit { exit_code } => {
                actions.push(AppAction::Detach(DetachReason::PaneExited { exit_code }));
            }
            Event::PaneLagged { .. } => {
                // Visual lag indicator is future work; runtime currently
                // logs warns. No action.
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
                actions.push(AppAction::DrawScope);
            }
            Event::DockerUnavailable { reason } => {
                self.docker.state = DockerScopeState::Unavailable { reason };
                self.docker.selection = 0;
                actions.push(AppAction::DrawScope);
            }
            Event::DockerStreamEnded { .. } => {
                // Per-container logs/stats streams — not wired into
                // DockerScope. Slice C3 consumes these for the logs panel.
            }
            _ => {}
        }
    }

    fn switch_to_scope(&mut self, actions: &mut Vec<AppAction>) {
        if matches!(self.view, View::Scope(_)) {
            return;
        }
        self.view = View::Scope(ScopeKind::Docker);

        // Subscribe to docker on enter. Starts the daemon-side container-
        // list polling; first event transitions Connecting → Available or
        // Unavailable.
        let sub_id = self.alloc_sub_id();
        self.docker.sub_id = Some(sub_id);
        self.docker.state = DockerScopeState::Connecting;
        self.docker.selection = 0;
        actions.push(AppAction::SendEnvelope(envelope(Payload::Subscribe(
            Subscription::Docker { id: sub_id },
        ))));

        actions.push(AppAction::EnterScopeMode);
        actions.push(AppAction::DrawScope);
    }

    fn switch_to_pane(&mut self, actions: &mut Vec<AppAction>) {
        if matches!(self.view, View::Pane) {
            return;
        }

        // Unsubscribe from docker on leave. The daemon's forwarder task is
        // tracked in docker_subs; Unsubscribe { id } aborts it. (The pane
        // Unsubscribe bug fix at `43b28eb` makes this actually work —
        // before that commit, pane Unsubscribe silently no-op'd.)
        if let Some(id) = self.docker.sub_id.take() {
            actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
                id,
            })));
        }
        self.docker.state = DockerScopeState::Idle;
        self.docker.filter.clear();
        self.docker.filter_active = false;
        self.docker.selection = 0;

        self.view = View::Pane;
        actions.push(AppAction::EnterPaneMode);

        // Synthetic re-attach: cancel the old AttachPane subscription and
        // send a fresh one so the daemon replays the current scrollback as
        // a PaneSnapshot. Byte-level invariant verified by
        // `tests/vim_preservation.rs`; real-terminal confirmation is the
        // C2c3 manual demo's Step 1.
        //
        // If the eyeball demo reveals problems, see `docs/ISSUES.md` for
        // the ranked fallback mitigations (Resize-after-attach first,
        // keep-AttachPane-alive only if that doesn't fix it).
        //
        // TODO(phase-5): scrollback re-transfer cost will matter over SSH;
        // revisit if SSH bandwidth becomes a concern.
        if let Some(old_sub) = self.pane_attach_sub.take() {
            actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
                id: old_sub,
            })));
        }
        let new_sub = self.alloc_sub_id();
        self.pane_attach_sub = Some(new_sub);
        actions.push(AppAction::SendEnvelope(envelope(Payload::AttachPane {
            pane_id: self.pane,
            subscription_id: new_sub,
        })));
    }

    fn alloc_sub_id(&mut self) -> u64 {
        let id = self.next_sub_id;
        self.next_sub_id += 1;
        id
    }
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

    fn pane_app() -> App {
        App::new(7, (24, 80))
    }

    fn pane_subscription_id_after_init(app: &mut App) -> u64 {
        let _ = app.initial_actions();
        app.pane_attach_sub
            .expect("initial_actions allocates pane_attach_sub")
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

    fn populate_docker_state(app: &mut App, containers: Vec<DockerContainer>) -> u64 {
        // Puts the app in scope mode and delivers a ContainerList on the
        // docker sub. Returns the docker sub_id.
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let sub_id = app
            .docker
            .sub_id
            .expect("switch_to_scope must allocate docker sub_id");
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
                event: Event::ContainerList {
                    containers,
                    engine_source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        sub_id
    }

    #[test]
    fn initial_actions_attach_then_resize() {
        let mut app = pane_app();
        let actions = app.initial_actions();
        assert_eq!(actions.len(), 2, "AttachPane + ResizePane");
        match &actions[0] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::AttachPane {
                    pane_id,
                    subscription_id,
                } => {
                    assert_eq!(*pane_id, 7);
                    assert!(*subscription_id > 0);
                }
                other => panic!("expected AttachPane, got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }
        match &actions[1] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::ResizePane {
                    pane_id,
                    rows,
                    cols,
                } => {
                    assert_eq!(*pane_id, 7);
                    assert_eq!(*rows, 24);
                    assert_eq!(*cols, 80);
                }
                other => panic!("expected ResizePane, got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }
        assert!(app.pane_attach_sub.is_some());
        assert_eq!(app.view, View::Pane);
    }

    #[test]
    fn ctrl_b_d_emits_user_detach() {
        let mut app = pane_app();
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
    fn pane_keystrokes_forward_to_daemon_as_send_input() {
        let mut app = pane_app();
        app.initial_actions();
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
    fn ctrl_b_s_switches_to_scope_and_subscribes_docker() {
        let mut app = pane_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        assert_eq!(app.view, View::Scope(ScopeKind::Docker));
        // Connecting state while we wait for the first event.
        assert!(
            matches!(app.docker.state, DockerScopeState::Connecting),
            "state after switch_to_scope must be Connecting (not Idle or Available); \
             the renderer distinguishes these three states"
        );
        let docker_sub_id = app.docker.sub_id.expect("docker sub_id must be allocated");
        let subscribe_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::Subscribe(Subscription::Docker { id })
                        if *id == docker_sub_id)
            )
        });
        assert_eq!(subscribe_count, 1);
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::EnterScopeMode)),
            1
        );
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawScope)), 1);
    }

    #[test]
    fn second_switch_to_scope_is_idempotent() {
        let mut app = pane_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let sub_before = app.docker.sub_id;
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::EnterScopeMode)),
            0,
            "switching to scope while already in scope must be a no-op"
        );
        let subscribe_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::Subscribe(Subscription::Docker { .. }))
            )
        });
        assert_eq!(
            subscribe_count, 0,
            "redundant switch must not re-subscribe to docker"
        );
        assert_eq!(
            app.docker.sub_id, sub_before,
            "sub_id must not change on redundant switch"
        );
    }

    #[test]
    fn second_switch_to_pane_is_idempotent() {
        // Symmetric counterpart to second_switch_to_scope_is_idempotent.
        // Catches a regression where a redundant switch_to_pane would
        // emit Unsubscribe + AttachPane needlessly, burning bandwidth
        // and making the daemon replay scrollback for no reason.
        let mut app = pane_app();
        pane_subscription_id_after_init(&mut app);
        let pane_sub_before = app.pane_attach_sub;
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02a".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::EnterPaneMode)),
            0,
            "switching to pane while already in pane must not re-enter pane mode"
        );
        let envelope_count = count(&actions, |a| matches!(a, AppAction::SendEnvelope(_)));
        assert_eq!(
            envelope_count, 0,
            "redundant switch_to_pane must not re-send Unsubscribe/AttachPane — that \
             would trigger a pointless scrollback replay on the daemon side"
        );
        assert_eq!(
            app.pane_attach_sub, pane_sub_before,
            "pane_attach_sub must not change on redundant switch"
        );
    }

    #[test]
    fn ctrl_b_question_in_pane_mode_is_dropped() {
        // Help overlay is a C3 concern; C2 intentionally produces zero
        // actions for Ctrl-b ?. Locking the current behavior as a
        // regression test means C3's overlay implementation can't
        // accidentally route help-key handling through the wrong arm.
        let mut app = pane_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(
            actions.is_empty(),
            "Ctrl-b ? in pane mode must not emit any actions (help overlay is C3)"
        );
    }

    #[test]
    fn ctrl_b_a_returns_to_pane_with_synthetic_reattach_and_unsubscribes_docker() {
        let mut app = pane_app();
        let prev_pane_sub = pane_subscription_id_after_init(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let docker_sub = app.docker.sub_id.expect("scope mode allocated docker sub");

        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02a".to_vec()));

        assert_eq!(app.view, View::Pane);
        assert!(
            matches!(app.docker.state, DockerScopeState::Idle),
            "leaving scope view must reset DockerScope to Idle"
        );
        assert_eq!(
            app.docker.sub_id, None,
            "docker sub_id must be cleared on leave"
        );

        // Both subscriptions cancelled (docker AND old pane attach).
        let docker_unsub = count(&actions, |a| {
            matches!(a, AppAction::SendEnvelope(env)
                if matches!(&env.payload, Payload::Unsubscribe { id } if *id == docker_sub))
        });
        assert_eq!(docker_unsub, 1, "docker subscription must be cancelled");

        let pane_unsub = count(&actions, |a| {
            matches!(a, AppAction::SendEnvelope(env)
                if matches!(&env.payload, Payload::Unsubscribe { id } if *id == prev_pane_sub))
        });
        assert_eq!(
            pane_unsub, 1,
            "old pane subscription must also be cancelled (synthetic re-attach)"
        );

        let new_attach = count(&actions, |a| {
            matches!(a, AppAction::SendEnvelope(env)
                if matches!(&env.payload, Payload::AttachPane { pane_id: 7, .. }))
        });
        assert_eq!(new_attach, 1, "fresh AttachPane must be sent");
        assert_ne!(
            app.pane_attach_sub.expect("new sub allocated"),
            prev_pane_sub
        );
    }

    #[test]
    fn container_list_transitions_state_to_available_and_clamps_selection() {
        let mut app = pane_app();
        let sub_id = populate_docker_state(
            &mut app,
            vec![
                make_container("web", "nginx", "running"),
                make_container("db", "postgres", "running"),
            ],
        );
        let _ = sub_id; // silence unused
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
        let mut app = pane_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let sub_id = app.docker.sub_id.unwrap();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
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
        let mut app = pane_app();
        populate_docker_state(
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
        // Clamp at bottom.
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.docker.selection, 2);
        app.handle_event(AppEvent::StdinChunk(b"k".to_vec()));
        assert_eq!(app.docker.selection, 1);
        // Clamp at top.
        app.handle_event(AppEvent::StdinChunk(b"kkk".to_vec()));
        assert_eq!(app.docker.selection, 0);
    }

    #[test]
    fn arrow_down_and_arrow_up_move_selection() {
        let mut app = pane_app();
        populate_docker_state(
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
        let mut app = pane_app();
        populate_docker_state(
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
        let mut app = pane_app();
        populate_docker_state(
            &mut app,
            vec![
                make_container("web", "nginx", "running"),
                make_container("db", "postgres", "running"),
                make_container("cache", "redis", "running"),
            ],
        );
        // Move selection to the last row.
        app.handle_event(AppEvent::StdinChunk(b"GG".to_vec()));
        assert_eq!(app.docker.selection, 2);

        // Open filter and type "we" — only "web" matches.
        app.handle_event(AppEvent::StdinChunk(b"/we".to_vec()));
        assert!(app.docker.filter_active);
        assert_eq!(app.docker.filter, "we");
        assert_eq!(app.docker.visible_count(), 1);
        // Selection clamped to single visible row.
        assert_eq!(app.docker.selection, 0);

        // Esc clears the filter AND exits input mode.
        app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec()));
        assert!(!app.docker.filter_active);
        assert_eq!(app.docker.filter, "");
        assert_eq!(app.docker.visible_count(), 3);
    }

    #[test]
    fn filter_enter_commits_without_clearing() {
        let mut app = pane_app();
        populate_docker_state(
            &mut app,
            vec![
                make_container("web", "nginx", "running"),
                make_container("db", "postgres", "running"),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"/nginx\n".to_vec()));
        assert!(!app.docker.filter_active, "Enter exits filter-input mode");
        assert_eq!(
            app.docker.filter, "nginx",
            "Enter must keep the filter applied, unlike Esc which clears"
        );
        assert_eq!(app.docker.visible_count(), 1);
    }

    #[test]
    fn filter_backspace_removes_last_char() {
        let mut app = pane_app();
        populate_docker_state(&mut app, vec![make_container("a", "a", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"/abc".to_vec()));
        assert_eq!(app.docker.filter, "abc");
        app.handle_event(AppEvent::StdinChunk(b"\x7f".to_vec()));
        assert_eq!(app.docker.filter, "ab");
        app.handle_event(AppEvent::StdinChunk(b"\x08".to_vec())); // legacy backspace
        assert_eq!(app.docker.filter, "a");
    }

    #[test]
    fn daemon_error_envelope_routes_to_show_toast() {
        let mut app = pane_app();
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
        assert_eq!(
            toast_count, 1,
            "Payload::Error must produce a ShowToast with ToastKind::Error carrying the message"
        );
    }

    #[test]
    fn docker_action_result_failure_routes_to_show_toast() {
        let mut app = pane_app();
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
        assert_eq!(
            toast_count, 1,
            "DockerActionResult::Failure must surface as an error toast; \
             silently dropping action failures would leave the user with no feedback"
        );
    }

    #[test]
    fn docker_action_result_success_does_not_toast_yet() {
        // Success toasts are wired in C3 (they need pending_actions to
        // reference what the user acted on, e.g. "Restarted nginx").
        // In C2 the App has no actions in flight, so Success must be a
        // no-op rather than a bare "action succeeded" toast.
        let mut app = pane_app();
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
                .all(|a| !matches!(a, AppAction::ShowToast { .. })),
            "no ShowToast for Success in C2 — C3 wires this with pending_actions context"
        );
    }

    #[test]
    fn pane_output_in_pane_mode_emits_writestdout() {
        let mut app = pane_app();
        let pane_sub = pane_subscription_id_after_init(&mut app);
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: pane_sub,
                event: Event::PaneOutput {
                    data: b"hello".to_vec(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            count(
                &actions,
                |a| matches!(a, AppAction::WriteStdout(d) if d == b"hello")
            ),
            1
        );
    }

    #[test]
    fn pane_snapshot_in_pane_mode_emits_writestdout() {
        let mut app = pane_app();
        let pane_sub = pane_subscription_id_after_init(&mut app);
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: pane_sub,
                event: Event::PaneSnapshot {
                    scrollback: b"replayed".to_vec(),
                    rows: 24,
                    cols: 80,
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            count(
                &actions,
                |a| matches!(a, AppAction::WriteStdout(d) if d == b"replayed")
            ),
            1
        );
    }

    #[test]
    fn pane_output_in_scope_mode_is_dropped() {
        let mut app = pane_app();
        let pane_sub = pane_subscription_id_after_init(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: pane_sub,
                event: Event::PaneOutput {
                    data: b"in-scope-mode".to_vec(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::WriteStdout(_))),
            0,
            "PaneOutput while in Scope mode must be dropped (no WriteStdout). \
             The synthetic re-attach on Scope→Pane replays from the daemon's ring buffer."
        );
    }

    #[test]
    fn pane_exit_event_emits_pane_exited_detach() {
        let mut app = pane_app();
        let pane_sub = pane_subscription_id_after_init(&mut app);
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: pane_sub,
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
            1,
            "PaneExit must carry the exit code through DetachReason — \
             without it, the user can't tell why the session ended."
        );
    }

    #[test]
    fn stale_subscription_event_is_dropped() {
        let mut app = pane_app();
        pane_subscription_id_after_init(&mut app);
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
        assert!(
            actions.is_empty(),
            "events for unknown subscription ids must be silently dropped"
        );
    }

    #[test]
    fn resize_forwards_to_daemon_and_redraws_only_in_scope() {
        let mut app = pane_app();
        let actions = app.handle_event(AppEvent::Resize {
            rows: 30,
            cols: 100,
        });
        assert_eq!(app.terminal_size, (30, 100));
        let resize_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::ResizePane { rows: 30, cols: 100, .. })
            )
        });
        assert_eq!(resize_count, 1);
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::DrawScope)),
            0,
            "no DrawScope in pane mode"
        );

        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let actions = app.handle_event(AppEvent::Resize {
            rows: 32,
            cols: 120,
        });
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawScope)), 1);
    }

    #[test]
    fn tick_in_pane_mode_is_noop() {
        let mut app = pane_app();
        let actions = app.handle_event(AppEvent::Tick);
        assert!(actions.is_empty());
    }

    #[test]
    fn tick_in_scope_mode_emits_drawscope() {
        let mut app = pane_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let actions = app.handle_event(AppEvent::Tick);
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawScope)), 1);
    }
}
