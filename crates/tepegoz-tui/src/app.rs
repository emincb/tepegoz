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
    DockerContainer, Envelope, Event, EventFrame, PROTOCOL_VERSION, PaneId, Payload,
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
}

/// Why the App is asking the runtime to leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DetachReason {
    /// User pressed `Ctrl-b d` / `Ctrl-b q`.
    User,
    /// The pane's child process exited; nothing to attach to.
    PaneExited { exit_code: Option<i32> },
}

/// Per-scope state for the docker panel. Slice C1 only stubs this; Slice
/// C2 wires `Subscribe(Docker)` / `ContainerList` / `DockerUnavailable`
/// into [`Self::state`] and renders the container table.
///
/// `selection`, `filter`, `sub_id` are unused in C1 by design — the
/// fields live here so C2 doesn't have to grow the struct shape (which
/// would propagate to the constructor and tests).
#[derive(Debug, Default)]
#[allow(dead_code)] // selection / filter / sub_id wired in C2
pub(crate) struct DockerScope {
    pub(crate) state: DockerScopeState,
    /// Index into the visible (filter-respecting) row set. Persists across
    /// `Connecting → Available` transitions so the user doesn't lose their
    /// place when an event arrives.
    pub(crate) selection: usize,
    pub(crate) filter: String,
    /// Subscription id we use for `Subscribe(Docker)`. `None` until C2
    /// wires the subscribe-on-enter behavior.
    pub(crate) sub_id: Option<u64>,
}

/// Three-state lifecycle for the docker scope panel. Per CTO §2: distinct
/// visual states. Don't conflate "haven't heard yet" with "engine said no
/// containers".
///
/// Slice C1 only constructs `Idle` (the default); the renderer matches on
/// all four variants so the wire is in place. Slice C2 wires
/// `Subscribe(Docker)` and starts producing the other three states.
#[derive(Debug, Default)]
#[allow(dead_code)] // Connecting/Available/Unavailable produced by C2
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
                    // Slice C2/C3 implements the help overlay. Stub.
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
                // Slice C2 parses these as navigation keys (j/k, arrows,
                // r/s/K/X/l, /, etc.). C1 stub: drop. Bytes typed during
                // the empty C1 scope view have nowhere meaningful to go.
            }
        }
    }

    fn handle_daemon_envelope(&mut self, env: Envelope, actions: &mut Vec<AppAction>) {
        let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        else {
            // Welcome, Pong, PaneOpened, PaneList, DockerActionResult, Error
            // — the App doesn't currently react to those after handshake.
            // Slice C3 will route DockerActionResult into pending_actions.
            return;
        };

        if Some(subscription_id) == self.pane_attach_sub {
            self.handle_pane_event(event, actions);
        } else if Some(subscription_id) == self.docker.sub_id {
            self.handle_docker_event(event, actions);
        }
        // Other subscription ids: a stale event from a sub we've already
        // unsubscribed from. Drop.
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

    fn handle_docker_event(&mut self, _event: Event, _actions: &mut Vec<AppAction>) {
        // Slice C2 wires this. Translation:
        //   Event::ContainerList { containers, engine_source } →
        //       self.docker.state = Available { containers, engine_source };
        //       actions.push(DrawScope);
        //   Event::DockerUnavailable { reason } →
        //       self.docker.state = Unavailable { reason };
        //       actions.push(DrawScope);
        //   Event::DockerStreamEnded — only relevant for DockerLogs/Stats
        //       (Slice C2/C3 introduces those subs).
    }

    fn switch_to_scope(&mut self, actions: &mut Vec<AppAction>) {
        if matches!(self.view, View::Scope(_)) {
            return;
        }
        self.view = View::Scope(ScopeKind::Docker);
        // Slice C2: send Subscribe(Docker) here, set
        // self.docker.state = Connecting, set self.docker.sub_id = Some(...).
        actions.push(AppAction::EnterScopeMode);
        actions.push(AppAction::DrawScope);
    }

    fn switch_to_pane(&mut self, actions: &mut Vec<AppAction>) {
        if matches!(self.view, View::Pane) {
            return;
        }
        // Slice C2: if self.docker.sub_id is Some, send Unsubscribe and
        // reset self.docker.state = Idle.
        self.view = View::Pane;
        actions.push(AppAction::EnterPaneMode);

        // Synthetic re-attach: cancel the old AttachPane subscription and
        // send a fresh one so the daemon replays the current scrollback as
        // a PaneSnapshot. The simpler alternative — keeping the
        // subscription alive across mode switches and buffering bytes
        // locally — would duplicate the daemon's ring buffer in the TUI.
        //
        // Per CTO sign-off on §3: prove vim-preservation works in C2. If
        // the snapshot replay doesn't redraw vim cleanly, the fallback
        // options are (a) Resize after re-attach to force vim's redraw,
        // (b) emit Ctrl-L equivalent into the pane, (c) keep AttachPane
        // alive across mode switches and accept the buffering cost.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_app() -> App {
        App::new(7, (24, 80))
    }

    fn pane_subscription_id_after_init(app: &mut App) -> u64 {
        let _ = app.initial_actions();
        app.pane_attach_sub
            .expect("initial_actions allocates pane_attach_sub")
    }

    /// Helper: count actions matching a predicate.
    fn count<F: FnMut(&AppAction) -> bool>(actions: &[AppAction], mut pred: F) -> usize {
        actions.iter().filter(|a| pred(a)).count()
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
    fn ctrl_b_s_switches_to_scope_and_draws() {
        let mut app = pane_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        assert_eq!(app.view, View::Scope(ScopeKind::Docker));
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
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::EnterScopeMode)),
            0,
            "switching to scope while already in scope must be a no-op"
        );
    }

    #[test]
    fn ctrl_b_a_returns_to_pane_with_synthetic_reattach() {
        let mut app = pane_app();
        let prev_sub = pane_subscription_id_after_init(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02s".to_vec()));
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02a".to_vec()));

        assert_eq!(app.view, View::Pane);
        assert_eq!(
            count(&actions, |a| matches!(a, AppAction::EnterPaneMode)),
            1
        );

        let unsub_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::Unsubscribe { id } if *id == prev_sub)
            )
        });
        assert_eq!(unsub_count, 1, "old pane subscription must be cancelled");

        let new_attach_count = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::AttachPane { pane_id: 7, .. })
            )
        });
        assert_eq!(new_attach_count, 1, "fresh AttachPane must be sent");

        assert_ne!(
            app.pane_attach_sub.expect("new sub allocated"),
            prev_sub,
            "new subscription_id must differ from the old one"
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
        // An event with a sub_id that was never allocated. Must not crash,
        // must not panic, must not emit any action.
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

        // Now in scope mode → resize must trigger a redraw.
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
