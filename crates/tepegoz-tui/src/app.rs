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

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use tepegoz_proto::{
    DockerActionKind, DockerActionOutcome, DockerActionRequest, DockerContainer, Envelope, Event,
    EventFrame, FleetActionKind, FleetActionOutcome, FleetActionRequest, HostEntry, HostState,
    LogStream, OpenPaneSpec, PROTOCOL_VERSION, PaneId, PaneInfo, PaneTarget, Payload, ProbePort,
    ProbeProcess, Subscription,
};
use vt100::Parser;

use crate::input::{InputAction, InputFilter};
use crate::pty_tile;
use crate::tile::{FocusDir, TileId, TileKind, TileLayout};

/// Max visible toasts at once. A fourth arrival drops the oldest
/// silently (per C3 UX clarification: never block a keystroke on a
/// toast).
pub(crate) const MAX_TOASTS: usize = 3;

/// Slice 6.0: max delta between two row clicks that count as a
/// double-click. Tuned a touch snappier than the usual 500 ms OS
/// default — TUI users click with intent more often than not, and
/// a quicker threshold feels more responsive.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Slice 6.0: inset a tile's `Rect` by one cell on every side (the
/// border the block renders). Returns `None` when the tile is too
/// small to have an interior area — the caller treats that as
/// "click absorbed as focus-only, nothing to hit-test inside."
fn inset_rect(r: Rect) -> Option<Rect> {
    if r.width < 2 || r.height < 2 {
        return None;
    }
    Some(Rect::new(r.x + 1, r.y + 1, r.width - 2, r.height - 2))
}

/// Auto-dismiss cadence per toast kind. Error toasts hang around longer
/// because the user needs time to read the engine's reason text.
const TOAST_SUCCESS_DURATION: Duration = Duration::from_secs(3);
const TOAST_ERROR_DURATION: Duration = Duration::from_secs(8);
const TOAST_INFO_DURATION: Duration = Duration::from_secs(4);

/// How long a DockerAction may sit without a DockerActionResult before
/// the App declares it lost and toasts a timeout. Covers daemon dead /
/// engine hung / lost event.
const PENDING_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle auto-cancel on the inline K/X confirm modal. If the user
/// walks away and forgets the prompt, we don't leave stale modal state
/// sitting on the tile forever.
const PENDING_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// Rolling cap on the LogsView transcript buffer. Past this the
/// oldest line is dropped silently on each append. 10 000 lines
/// ≈ 1–2 MiB in practice; bounded memory for a live follow stream
/// on a talkative container.
pub(crate) const MAX_LOG_LINES: usize = 10_000;

/// PgUp / PgDn step inside the LogsView.
const LOGS_PAGE_LINES: usize = 10;

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

/// Max pane tabs with dedicated `Ctrl-b <digit>` jump keybind. Tabs past
/// this render as a `[+N]` overflow indicator; navigation past the 9th
/// pane still works via `Ctrl-b n`/`p`. The list-view overlay for more
/// than nine panes is explicitly deferred to 5e / v1.1 per
/// `docs/ISSUES.md#ctrl-b-w-pane-list-overlay-deferred`.
pub(crate) const MAX_TAB_DIGIT_SLOTS: usize = 9;

/// Which scope panel a tile hosts. Slice C1.5 shipped `Docker`; Slice
/// 4c adds `Ports` (one tile hosting both a Ports view and a Processes
/// toggle-view per Phase 4 Q1); Phases 5 / 9 extend further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKind {
    Docker,
    Ports,
    Fleet,
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
    /// A pending one-shot request (e.g. `DockerAction`) hit its
    /// deadline. The Tick sweep drives expiry in C3a — the runtime
    /// itself never constructs this variant today — but the variant
    /// stays in the event surface so a dedicated runtime-side sweeper
    /// (e.g. a timer wheel) can be added later without reshaping the
    /// API. Exercised in tests via direct construction.
    #[allow(dead_code)]
    PendingActionTimeout(u64),
    /// Slice 6.0: left-mouse-button press at 0-indexed terminal cell
    /// (x, y). The runtime extracts these from SGR mouse sequences
    /// on stdin via [`mouse::MouseParser`] before dispatch; tests
    /// can inject events directly. Right/middle/wheel presses are
    /// dropped at the parser — the clickable surface is left-only.
    MouseClick { x: u16, y: u16 },
    /// Slice 6.0: mouse pointer moved to cell (x, y). Emitted for
    /// both motion-only (hover) and drag; Slice 6.0 treats them
    /// identically, using the coordinate to light up tile border
    /// hover styling. 0-indexed.
    MouseHover { x: u16, y: u16 },
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

/// Severity / classification for [`AppAction::ShowToast`] and
/// entries in [`App::toasts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    /// Neutral notice — not produced by C3a (no path currently emits
    /// `Info`), but kept for future reuse so we don't have to reshape
    /// the enum when one arrives.
    #[allow(dead_code)]
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
    /// Which Docker-tile view is active. `List` is the container
    /// table (default); `Logs(...)` is a sub-state following one
    /// container's log stream. The sub-state lives *inside* the
    /// Docker tile — other tiles (PTY, placeholders) keep rendering
    /// and receiving input throughout (Decision #7, UX clarification
    /// #1 for C3b).
    pub(crate) view: DockerView,
    /// Index into the visible (filter-respecting) row set. Clamped on
    /// every `ContainerList` update. Ignored while `view == Logs`.
    pub(crate) selection: usize,
    pub(crate) filter: String,
    /// True while the filter bar has focus (user typed `/`). While
    /// active: chars append, backspace trims, Esc clears + deactivates,
    /// Enter deactivates but keeps the filter applied.
    pub(crate) filter_active: bool,
    /// Subscription id for `Subscribe(Docker)` (the always-on list
    /// subscription). Allocated once at [`App::new`] and never
    /// cleared. Distinct from a logs-view `sub_id`, which lives on
    /// [`LogsView`] and comes + goes with the sub-state.
    pub(crate) sub_id: u64,
    /// Inline confirm prompt for destructive actions (Kill / Remove).
    /// Rendered as a centered bordered box inside the Docker tile's
    /// Rect while set. Only reachable while `view == List`. Any key
    /// other than `y`/`Y` cancels (with `K`/`X` absorbed so a second
    /// press can't switch the target mid-prompt); focus moving away
    /// from the Docker tile cancels; a 10 s idle timeout cancels.
    pub(crate) pending_confirm: Option<PendingConfirm>,
    /// Phase 6 Slice 6c: current target for the Docker subscription +
    /// dispatched `DockerAction`s. `Local` (the default) mirrors
    /// pre-v11 behaviour; `Remote { alias }` routes through the
    /// Fleet-supervised agent connection. Swapped via the host-picker
    /// modal (click the tile-title's target suffix). Task E defines
    /// the picker state + retarget dispatch; this field is the
    /// source of truth for both the wire-level `Subscription::Docker
    /// { target }` value and the title-suffix render.
    pub(crate) target: tepegoz_proto::ScopeTarget,
}

/// Docker tile view state. C3a had a single implicit "list" view;
/// C3b adds the `Logs` sub-state.
#[derive(Debug)]
pub(crate) enum DockerView {
    /// Container list + filter + confirm-modal. The default.
    List,
    /// Following one container's log stream. Lives inside the Docker
    /// tile's `Rect` (not a modal overlay); other tiles continue to
    /// render and take input.
    Logs(LogsView),
}

/// Transcript state while a logs sub-state is active. Holds the
/// rolling buffer, the per-stream partial-line accumulators, the
/// scroll position, and the `at_tail` auto-follow flag.
#[derive(Debug)]
pub(crate) struct LogsView {
    /// Container id the sub is following. Renderer shows the
    /// display name; the id is kept as the authoritative identity
    /// for tests, diagnostics, and any future "reopen logs after
    /// reconnect" flow.
    #[allow(dead_code)]
    pub(crate) container_id: String,
    /// Display name captured on entry. Cached so renames or filter
    /// changes on the list don't retitle the logs view while we're
    /// inside it.
    pub(crate) container_name: String,
    /// Subscription id for the per-container `Subscribe(DockerLogs)`.
    /// Unsubscribed on exit.
    pub(crate) sub_id: u64,
    /// Assembled lines, newest at the back. Capped at
    /// [`MAX_LOG_LINES`]; oldest drops on append past the cap.
    pub(crate) lines: VecDeque<LogLine>,
    /// Partial-line byte accumulators. Log chunks may split
    /// mid-line; the bytes after the last `\n` in a chunk wait here
    /// until the rest of the line arrives. Per-stream so a stdout
    /// line in progress isn't corrupted by an interleaved stderr
    /// line.
    pub(crate) pending_stdout: Vec<u8>,
    pub(crate) pending_stderr: Vec<u8>,
    /// Number of lines above the buffer tail the visible top row
    /// sits. `0` = rendered at the tail (newest). Increments on
    /// scroll-up; decrements on scroll-down.
    pub(crate) scroll_offset: usize,
    /// Auto-follow flag. `true` when the user wants new lines to
    /// appear in the visible window as they arrive. Set `false` on
    /// any upward scroll; `G` resets to `true`. `DockerStreamEnded`
    /// also disables it so the final messages don't scroll off.
    pub(crate) at_tail: bool,
    /// Terminal reason from `DockerStreamEnded`, if the stream has
    /// ended. Rendered as a dimmed "— log stream ended: `<reason>` —"
    /// line at the tail.
    pub(crate) stream_ended: Option<String>,
}

/// One fully-assembled log line.
#[derive(Debug, Clone)]
pub(crate) struct LogLine {
    pub(crate) stream: LogStream,
    pub(crate) text: String,
}

impl DockerScope {
    fn new(sub_id: u64) -> Self {
        Self {
            // Subscribe is sent in initial_actions, so we open at
            // Connecting rather than Idle — there's no "haven't
            // subscribed yet" moment the user can observe.
            state: DockerScopeState::Connecting,
            view: DockerView::List,
            selection: 0,
            filter: String::new(),
            filter_active: false,
            sub_id,
            pending_confirm: None,
            // Default `Local`: pre-v11 behaviour. Target flips via
            // Task E's host-picker modal → `retarget_docker`
            // dispatch.
            target: tepegoz_proto::ScopeTarget::Local,
        }
    }

    /// True if the logs sub-state is active AND the given sub id
    /// matches it. Used to route `ContainerLog` / `DockerStreamEnded`
    /// events to the logs handler.
    pub(crate) fn is_current_logs_sub(&self, sub_id: u64) -> bool {
        matches!(&self.view, DockerView::Logs(l) if l.sub_id == sub_id)
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

    /// The currently selected (visible, filter-respecting) container,
    /// or `None` if we're not in `Available` or the list is empty.
    pub(crate) fn selected_container(&self) -> Option<&DockerContainer> {
        match &self.state {
            DockerScopeState::Available { containers, .. } => containers
                .iter()
                .filter(|c| self.matches_filter(c))
                .nth(self.selection),
            _ => None,
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

/// In-flight inline confirm prompt on the Docker tile.
#[derive(Debug, Clone)]
pub(crate) struct PendingConfirm {
    pub(crate) kind: DockerActionKind,
    pub(crate) container_id: String,
    /// Display name (first `names` entry with leading `/` stripped, or
    /// short id if the container had no names).
    pub(crate) container_name: String,
    /// Idle auto-cancel deadline.
    pub(crate) deadline: Instant,
}

impl LogsView {
    fn new(container_id: String, container_name: String, sub_id: u64) -> Self {
        Self {
            container_id,
            container_name,
            sub_id,
            lines: VecDeque::new(),
            pending_stdout: Vec::new(),
            pending_stderr: Vec::new(),
            scroll_offset: 0,
            at_tail: true,
            stream_ended: None,
        }
    }

    /// Absorb a `ContainerLog` chunk: append bytes to the per-stream
    /// pending buffer and flush every complete (`\n`-terminated) line
    /// into [`Self::lines`]. Tail bytes without a trailing `\n` wait
    /// in the pending buffer for the next chunk.
    pub(crate) fn ingest(&mut self, stream: LogStream, data: &[u8]) {
        // Drain complete lines into a local Vec first so the `pending`
        // borrow drops before we call `push_line` (which also borrows
        // `&mut self`).
        let mut completed: Vec<String> = Vec::new();
        {
            let pending = match stream {
                LogStream::Stdout => &mut self.pending_stdout,
                LogStream::Stderr => &mut self.pending_stderr,
            };
            pending.extend_from_slice(data);
            while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=nl).collect();
                // Strip the trailing `\n` (and a `\r` before it for
                // CRLF-style output from some Windows containers).
                let mut end = raw.len().saturating_sub(1);
                if end > 0 && raw[end - 1] == b'\r' {
                    end -= 1;
                }
                completed.push(String::from_utf8_lossy(&raw[..end]).into_owned());
            }
        }
        for text in completed {
            self.push_line(LogLine { stream, text });
        }
    }

    fn push_line(&mut self, line: LogLine) {
        if self.lines.len() >= MAX_LOG_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Move the visible top-of-window up by `n` lines (toward older
    /// history). Disables `at_tail` since the user chose to scroll
    /// away from the tail.
    pub(crate) fn scroll_up(&mut self, n: usize) {
        let max_offset = self.lines.len().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + n).min(max_offset);
        if self.scroll_offset > 0 {
            self.at_tail = false;
        }
    }

    /// Move the visible top-of-window down by `n` lines (toward
    /// newer content). When the offset reaches 0 the view is back at
    /// the tail, so `at_tail` flips true (auto-follow resumes).
    pub(crate) fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
        if self.scroll_offset == 0 {
            self.at_tail = true;
        }
    }

    /// Jump to the buffer tail + re-enable auto-follow. Bound to `G`
    /// / `End` / `Bottom` in the logs-view keybind map.
    pub(crate) fn jump_to_tail(&mut self) {
        self.scroll_offset = 0;
        self.at_tail = true;
    }

    /// Finalize the transcript on stream termination. Flushes any
    /// non-newline-terminated pending bytes as a last line, records
    /// the reason, and disables `at_tail` so the final context stays
    /// visible without being scrolled off by… nothing, but defensive
    /// anyway: a future "stream resumed" path would need to
    /// re-engage the tail explicitly.
    pub(crate) fn end_stream(&mut self, reason: String) {
        let stdout_tail = std::mem::take(&mut self.pending_stdout);
        if !stdout_tail.is_empty() {
            let text = String::from_utf8_lossy(&stdout_tail).into_owned();
            self.push_line(LogLine {
                stream: LogStream::Stdout,
                text,
            });
        }
        let stderr_tail = std::mem::take(&mut self.pending_stderr);
        if !stderr_tail.is_empty() {
            let text = String::from_utf8_lossy(&stderr_tail).into_owned();
            self.push_line(LogLine {
                stream: LogStream::Stderr,
                text,
            });
        }
        self.stream_ended = Some(reason);
        self.at_tail = false;
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

/// Phase 4 Slice 4c: Ports tile state. Hosts two coequal views in one
/// tile (Decision #7's god-view layout reserves space only for the
/// five headline scopes — Processes lives as a toggle-mode sub-view
/// inside the Ports tile per Phase 4 Q1). The user toggles between
/// them with `p`; both subscriptions stay live regardless of which is
/// rendered, so switching views never drops data.
#[derive(Debug)]
pub(crate) struct PortsScope {
    pub(crate) ports: PortsView,
    pub(crate) processes: ProcessesView,
    /// Which view the tile renders. Toggle with `p`.
    pub(crate) active: PortsActiveView,
    /// Subscription id for `Subscribe(Ports)`. Allocated once at
    /// `App::new`; lives for the session.
    pub(crate) ports_sub_id: u64,
    /// Subscription id for `Subscribe(Processes)`. Also allocated at
    /// `App::new`; both subs live concurrently.
    pub(crate) processes_sub_id: u64,
    /// Phase 6 Slice 6d-ii: current target for the Ports subscription.
    /// Mirrors the Docker tile's `target` field — both views in this
    /// tile retarget independently (a user can have local Ports +
    /// remote Processes simultaneously) so we keep two fields.
    pub(crate) ports_target: tepegoz_proto::ScopeTarget,
    /// Phase 6 Slice 6d-ii: current target for the Processes
    /// subscription. See `ports_target` for rationale.
    pub(crate) processes_target: tepegoz_proto::ScopeTarget,
}

/// Which view the Ports tile currently renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortsActiveView {
    Ports,
    Processes,
}

/// State for the Ports view (default / left side of the toggle).
#[derive(Debug)]
pub(crate) struct PortsView {
    pub(crate) state: PortsViewState,
    /// Index into the visible (filter-respecting) row set.
    pub(crate) selection: usize,
    pub(crate) filter: String,
    pub(crate) filter_active: bool,
}

/// Three-state lifecycle for the Ports view. Same shape as
/// `DockerScopeState` on purpose — the Phase 3 precedent.
#[derive(Debug)]
pub(crate) enum PortsViewState {
    Connecting,
    Available {
        rows: Vec<ProbePort>,
        source: String,
    },
    Unavailable {
        reason: String,
    },
}

/// State for the Processes view (right side of the toggle).
#[derive(Debug)]
pub(crate) struct ProcessesView {
    pub(crate) state: ProcessesViewState,
    pub(crate) selection: usize,
    pub(crate) filter: String,
    pub(crate) filter_active: bool,
}

#[derive(Debug)]
pub(crate) enum ProcessesViewState {
    Connecting,
    Available {
        rows: Vec<ProbeProcess>,
        source: String,
    },
    Unavailable {
        reason: String,
    },
}

/// Stable identity for Ports selection persistence across refreshes.
/// Listening ports are stable over minutes, but a refresh can still
/// reorder or drop rows — we re-anchor the selection to the same
/// `(protocol, local_port, pid)` tuple rather than trusting a positional
/// index that would silently re-target a different row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PortKey {
    pub(crate) protocol: String,
    pub(crate) local_port: u16,
    pub(crate) pid: u32,
}

impl PortKey {
    pub(crate) fn of(p: &ProbePort) -> Self {
        Self {
            protocol: p.protocol.clone(),
            local_port: p.local_port,
            pid: p.pid,
        }
    }
}

/// Stable identity for Processes selection. `(pid, start_time)` is
/// robust to pid reuse across short-lived processes — a new process
/// that reuses a pid gets a different `start_time`, so selection
/// doesn't silently retarget.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProcessKey {
    pub(crate) pid: u32,
    pub(crate) start_time_unix_secs: i64,
}

impl ProcessKey {
    pub(crate) fn of(p: &ProbeProcess) -> Self {
        Self {
            pid: p.pid,
            start_time_unix_secs: p.start_time_unix_secs,
        }
    }
}

impl PortsScope {
    fn new(ports_sub_id: u64, processes_sub_id: u64) -> Self {
        Self {
            ports: PortsView {
                state: PortsViewState::Connecting,
                selection: 0,
                filter: String::new(),
                filter_active: false,
            },
            processes: ProcessesView {
                state: ProcessesViewState::Connecting,
                selection: 0,
                filter: String::new(),
                filter_active: false,
            },
            active: PortsActiveView::Ports,
            ports_sub_id,
            processes_sub_id,
            ports_target: tepegoz_proto::ScopeTarget::Local,
            processes_target: tepegoz_proto::ScopeTarget::Local,
        }
    }
}

impl PortsView {
    pub(crate) fn matches_filter(&self, p: &ProbePort) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        p.process_name.to_lowercase().contains(&q)
            || p.local_ip.to_lowercase().contains(&q)
            || p.local_port.to_string().contains(&q)
            || p.container_id
                .as_deref()
                .is_some_and(|id| id.to_lowercase().contains(&q))
    }

    pub(crate) fn visible_count(&self) -> usize {
        match &self.state {
            PortsViewState::Available { rows, .. } => {
                rows.iter().filter(|p| self.matches_filter(p)).count()
            }
            _ => 0,
        }
    }

    pub(crate) fn selected_port(&self) -> Option<&ProbePort> {
        match &self.state {
            PortsViewState::Available { rows, .. } => rows
                .iter()
                .filter(|p| self.matches_filter(p))
                .nth(self.selection),
            _ => None,
        }
    }

    fn selected_key(&self) -> Option<PortKey> {
        self.selected_port().map(PortKey::of)
    }

    /// Re-anchor `selection` after a state change. If `old_key` still
    /// appears in the visible set, point `selection` at it. Otherwise
    /// clamp into `[0, visible_count)` so the selection lands on a
    /// real row (or collapses to 0 if the list emptied). Never panics
    /// when the list shrinks under a live cursor.
    fn reanchor_selection(&mut self, old_key: Option<PortKey>) {
        let PortsViewState::Available { rows, .. } = &self.state else {
            self.selection = 0;
            return;
        };
        let visible: Vec<&ProbePort> = rows.iter().filter(|p| self.matches_filter(p)).collect();
        if let Some(key) = old_key
            && let Some(idx) = visible.iter().position(|p| PortKey::of(p) == key)
        {
            self.selection = idx;
            return;
        }
        if visible.is_empty() {
            self.selection = 0;
        } else if self.selection >= visible.len() {
            self.selection = visible.len() - 1;
        }
    }
}

impl ProcessesView {
    pub(crate) fn matches_filter(&self, p: &ProbeProcess) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        p.command.to_lowercase().contains(&q) || p.pid.to_string().contains(&q)
    }

    pub(crate) fn visible_count(&self) -> usize {
        match &self.state {
            ProcessesViewState::Available { rows, .. } => {
                rows.iter().filter(|p| self.matches_filter(p)).count()
            }
            _ => 0,
        }
    }

    pub(crate) fn selected_process(&self) -> Option<&ProbeProcess> {
        match &self.state {
            ProcessesViewState::Available { rows, .. } => rows
                .iter()
                .filter(|p| self.matches_filter(p))
                .nth(self.selection),
            _ => None,
        }
    }

    fn selected_key(&self) -> Option<ProcessKey> {
        self.selected_process().map(ProcessKey::of)
    }

    fn reanchor_selection(&mut self, old_key: Option<ProcessKey>) {
        let ProcessesViewState::Available { rows, .. } = &self.state else {
            self.selection = 0;
            return;
        };
        let visible: Vec<&ProbeProcess> = rows.iter().filter(|p| self.matches_filter(p)).collect();
        if let Some(key) = old_key
            && let Some(idx) = visible.iter().position(|p| ProcessKey::of(p) == key)
        {
            self.selection = idx;
            return;
        }
        if visible.is_empty() {
            self.selection = 0;
        } else if self.selection >= visible.len() {
            self.selection = visible.len() - 1;
        }
    }
}

/// Phase 5 Slice 5b: SSH Fleet tile state. Hosts the list of configured
/// SSH hosts from `tepegoz-ssh::HostList::discover()` plus their per-
/// host connection state (all `Disconnected` in 5b — 5c's supervisor
/// drives real transitions). Single view (no toggle like Ports).
#[derive(Debug)]
pub(crate) struct FleetScope {
    pub(crate) state: FleetScopeState,
    pub(crate) selection: usize,
    pub(crate) filter: String,
    pub(crate) filter_active: bool,
    /// Subscription id for `Subscribe(Fleet)`. Allocated once at
    /// `App::new`; lives for the session.
    pub(crate) sub_id: u64,
}

/// Three-state lifecycle for the Fleet tile — mirrors Docker/Ports.
/// `Available` carries the full host list + per-alias state map +
/// source label. No `Unavailable` variant: a discovery failure still
/// produces an empty `HostList` with an error `source`, rendered as
/// Available-with-zero-hosts + the source string as the footer hint.
#[derive(Debug)]
pub(crate) enum FleetScopeState {
    Connecting,
    Available {
        hosts: Vec<HostEntry>,
        states: HashMap<String, HostState>,
        source: String,
    },
}

/// Phase 6 Slice 6c-iii: state for the host picker modal. Opens on
/// `Ctrl-b t` or a click on a target-capable tile's title bar.
///
/// `target_tile` names which scope owns the retarget — the modal
/// renders + commits against that tile's subscription. `selected` is
/// an index into [`App::host_picker_rows`] (index 0 = Local).
///
/// `required_capability` is the capability string the modal uses to
/// annotate unusable hosts (`"docker"` in 6c-iii; `"ports"` /
/// `"processes"` in 6d when those tiles gain retarget UX). In
/// 6c-iii the daemon → TUI wire doesn't carry per-agent capabilities
/// yet, so "usable" degrades to "host state is Connected" — a
/// Connected host without docker would still render as selectable and
/// fail gracefully on commit (DockerUnavailable renders in the tile).
#[derive(Debug, Clone)]
pub(crate) struct HostPickerModal {
    pub(crate) target_tile: HostPickerTargetTile,
    /// Forward-looking: stored per-picker so 6d's Ports / Processes
    /// tiles reuse the same modal with a different capability string.
    /// In 6c-iii we don't yet have a per-agent capability table in
    /// the TUI (the daemon has it; the wire doesn't propagate it), so
    /// this string is a render hint only — grey-out is driven by
    /// host state. 6d will consult it against a populated capability
    /// table.
    #[allow(dead_code)]
    pub(crate) required_capability: &'static str,
    pub(crate) selected: usize,
}

/// Which tile owns the retarget dispatch. 6c-iii shipped Docker; 6d-ii
/// adds Ports + Processes reusing this modal unchanged via the
/// `required_capability` parameter — `"ports"` / `"processes"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostPickerTargetTile {
    Docker,
    Ports,
    Processes,
}

impl HostPickerModal {
    pub(crate) fn target_tile_label(&self) -> &'static str {
        match self.target_tile {
            HostPickerTargetTile::Docker => "docker",
            HostPickerTargetTile::Ports => "ports",
            HostPickerTargetTile::Processes => "processes",
        }
    }
}

/// One row in the host picker modal. Index 0 is always Local; indices
/// 1..=N are Fleet hosts in discovery order.
///
/// `usable` is the authoritative "commit will work" signal — true
/// iff the host is `Connected` AND its agent advertises the
/// invoking tile's required capability (populated via
/// `Event::AgentCapabilities` since Slice 6d-i). `has_capability`
/// is surfaced separately so the annotation can distinguish
/// "(not connected)" from "(no <cap>)" — two different
/// remediations from the user's POV (reconnect vs. check the
/// remote docker / ports / processes service).
#[derive(Debug, Clone)]
pub(crate) enum HostPickerRow {
    Local,
    Remote {
        alias: String,
        state: tepegoz_proto::HostState,
        has_capability: bool,
        usable: bool,
    },
}

/// Stable identity for Fleet selection — the alias is unique per host
/// list so a single String suffices. Re-anchors across refreshes the
/// same way Ports/Processes do.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FleetKey(pub(crate) String);

impl FleetKey {
    pub(crate) fn of(h: &HostEntry) -> Self {
        Self(h.alias.clone())
    }
}

impl FleetScope {
    fn new(sub_id: u64) -> Self {
        Self {
            state: FleetScopeState::Connecting,
            selection: 0,
            filter: String::new(),
            filter_active: false,
            sub_id,
        }
    }

    pub(crate) fn matches_filter(&self, h: &HostEntry) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let q = self.filter.to_lowercase();
        h.alias.to_lowercase().contains(&q)
            || h.hostname.to_lowercase().contains(&q)
            || h.user.to_lowercase().contains(&q)
    }

    pub(crate) fn visible_count(&self) -> usize {
        match &self.state {
            FleetScopeState::Available { hosts, .. } => {
                hosts.iter().filter(|h| self.matches_filter(h)).count()
            }
            _ => 0,
        }
    }

    pub(crate) fn selected_host(&self) -> Option<&HostEntry> {
        match &self.state {
            FleetScopeState::Available { hosts, .. } => hosts
                .iter()
                .filter(|h| self.matches_filter(h))
                .nth(self.selection),
            _ => None,
        }
    }

    fn selected_key(&self) -> Option<FleetKey> {
        self.selected_host().map(FleetKey::of)
    }

    fn reanchor_selection(&mut self, old_key: Option<FleetKey>) {
        let FleetScopeState::Available { hosts, .. } = &self.state else {
            self.selection = 0;
            return;
        };
        let visible: Vec<&HostEntry> = hosts.iter().filter(|h| self.matches_filter(h)).collect();
        if let Some(key) = old_key
            && let Some(idx) = visible.iter().position(|h| FleetKey::of(h) == key)
        {
            self.selection = idx;
            return;
        }
        if visible.is_empty() {
            self.selection = 0;
        } else if self.selection >= visible.len() {
            self.selection = visible.len() - 1;
        }
    }
}

/// Pending one-shot request awaiting a response from the daemon. Keyed
/// by `request_id` in [`App::pending_actions`]; the id is mirrored back
/// in the `DockerActionResult` so the App can look up the description
/// (e.g. "Restart nginx") to include in the resulting toast.
#[derive(Debug)]
pub(crate) struct PendingAction {
    /// Absolute deadline. When `Instant::now()` exceeds this, the App
    /// declares the action lost and emits a timeout error toast.
    pub(crate) deadline: Instant,
    /// Human-readable description ("Restart nginx"). Used as the toast
    /// body prefix when the result (or a timeout) arrives.
    pub(crate) description: String,
}

/// A user-visible toast currently in the overlay strip. Stored newest-
/// to-oldest in [`App::toasts`] (new toasts `push_back`, oldest drops
/// off `pop_front` when the list exceeds [`MAX_TOASTS`]).
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub(crate) kind: ToastKind,
    pub(crate) message: String,
    /// Absolute deadline at which the Tick sweep drops this toast.
    pub(crate) expires_at: Instant,
}

/// Semantic key events parsed from raw stdin bytes when the Docker
/// tile is focused. C3a adds `Char` variants for `r`/`s`/`K`/`X`
/// lifecycle actions; C3b adds `PgUp` / `PgDn` for the logs-view
/// scroll and threads `Char(b'l')` through for entering the logs
/// sub-state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeKey {
    Up,
    Down,
    Top,
    Bottom,
    Home,
    End,
    PgUp,
    PgDn,
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
                        b"5" => out.push(ScopeKey::PgUp),
                        b"6" => out.push(ScopeKey::PgDn),
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

/// One entry in the pane stack. Holds everything the TUI needs to
/// multiplex bytes between multiple concurrent panes: the daemon's
/// pane id, the subscription id on which `PaneOutput` / `PaneSnapshot`
/// events arrive, a human-readable tab-strip label, and a per-pane
/// `vt100::Parser` so switching tabs restores the previous screen
/// without requiring a daemon re-subscribe.
pub(crate) struct PaneEntry {
    pub(crate) pane_id: PaneId,
    pub(crate) sub_id: u64,
    /// Short label for the tab strip: `"zsh"` for local shells (path
    /// trimmed to the final component) or `"ssh:<alias>"` for remote
    /// panes (the daemon's `remote_pane` module already formats it
    /// this way in `PaneInfo.shell`).
    pub(crate) label: String,
    pub(crate) parser: Parser,
}

/// In-flight `OpenPane` request awaiting a `PaneOpened` response. The
/// wire protocol has no per-request id for `OpenPane`; correlation is
/// FIFO, which works because the daemon's session loop processes
/// commands serially (one socket → one writer → in-order replies).
pub(crate) struct PendingOpen {
    /// Sub id pre-allocated at request time. The client uses this id
    /// in the subsequent `AttachPane { pane_id, subscription_id }`
    /// after the `PaneOpened` response arrives.
    pub(crate) sub_id: u64,
    /// Set for `Remote { alias }` opens — used to format a red toast
    /// when the open fails (unknown alias, SSH dial error, auth
    /// failure on first connect).
    pub(crate) alias: Option<String>,
}

/// The pure state machine.
pub(crate) struct App {
    pub(crate) view: View,
    /// Ordered stack of open panes. The pty tile always renders
    /// `pane_stack[active_pane]`; the tab strip renders all entries.
    /// Startup populates with a single root pane; `Ctrl-b Enter` on
    /// the Fleet tile pushes a remote pane; `Ctrl-b &` pops the
    /// active entry.
    pub(crate) pane_stack: Vec<PaneEntry>,
    pub(crate) active_pane: usize,
    /// FIFO queue of in-flight `OpenPane` requests. Popped when a
    /// `PaneOpened` response or an `Error` (for a failed open)
    /// arrives.
    pub(crate) pending_opens: VecDeque<PendingOpen>,
    pub(crate) docker: DockerScope,
    /// Phase 4 Slice 4c: Ports tile (with Processes toggle-view). Both
    /// the Ports and Processes subscriptions live for the session; the
    /// tile only renders one at a time, but neither drops data when
    /// toggled out of view.
    pub(crate) ports: PortsScope,
    /// Phase 5 Slice 5b: SSH Fleet tile. Subscribes to `Fleet` at
    /// startup; 5b carries only the initial `HostList` snapshot + one
    /// `HostStateChanged { Disconnected }` per host. 5c's supervisor
    /// drives real connection-state transitions.
    pub(crate) fleet: FleetScope,
    pub(crate) terminal_size: (u16, u16),
    /// Monotonic id allocator shared between subscription ids and
    /// DockerAction request ids. The daemon correlates each response by
    /// its embedded id, so collisions between namespaces don't matter —
    /// one counter keeps it simple.
    next_sub_id: u64,
    /// In-flight `DockerAction` requests keyed by `request_id`. Entries
    /// are removed when `DockerActionResult` arrives or the 30 s
    /// deadline passes (the latter emits a timeout toast).
    pub(crate) pending_actions: HashMap<u64, PendingAction>,
    /// Current toast overlay (newest at the back). Bounded to
    /// [`MAX_TOASTS`]; a fourth arrival drops the oldest silently.
    pub(crate) toasts: VecDeque<Toast>,
    /// Slice 6.0: `Ctrl-b ?` toggles the help overlay on; `Esc`
    /// (or a second `Ctrl-b ?`) dismisses it. When visible the
    /// overlay absorbs key input and blocks scope/pty routing.
    pub(crate) help_visible: bool,
    /// Phase 6 Slice 6c-iii: centered host picker modal. `Some`
    /// when `Ctrl-b t` (or a click on the Docker tile's title bar)
    /// opened the modal; `None` otherwise. While `Some`, the
    /// modal absorbs key input and mouse clicks the same way the
    /// help overlay does: any key other than navigation / Enter /
    /// Escape / Detach / Help dismisses + falls through; a click
    /// dismisses the modal (clicks on modal rows commit).
    pub(crate) host_picker: Option<HostPickerModal>,
    /// Phase 6 Slice 6d-i: per-alias agent capability list, sourced
    /// from `Event::AgentCapabilities` on the Fleet subscription.
    /// Empty vec = "agent registered but advertises no capabilities";
    /// missing entry = "no agent registered for this alias".
    /// Consumed by the host picker's `usable` determination so a
    /// Connected host that lacks the invoking tile's required
    /// capability greys out with `(no <cap>)` — the user sees up-
    /// front which retargets will and won't land.
    pub(crate) host_capabilities: HashMap<String, Vec<String>>,
    /// Slice 6.0: most-recent mouse-hover tile, used by the tile
    /// renderer to draw a distinct border style on the hovered tile.
    /// `None` means the cursor is outside every tile's Rect (or no
    /// hover event has been received yet).
    pub(crate) hovered_tile: Option<TileId>,
    /// Slice 6.0: most-recent scope-row left-click (tile, row index,
    /// instant) for double-click detection. Only scope row clicks
    /// update it — PTY tab clicks are single-click actions so they
    /// don't need dedup. A click whose (tile, row) + window match
    /// the stored state upgrades to a primary-action dispatch.
    pub(crate) last_click: Option<(TileId, usize, Instant)>,
    input_filter: InputFilter,
    scope_key_parser: ScopeKeyParser,
}

impl App {
    pub(crate) fn new(pane: PaneId, shell: String, terminal_size: (u16, u16)) -> Self {
        let (rows, cols) = terminal_size;
        let area = Rect::new(0, 0, cols, rows);
        let view = View::new(area);

        let (pty_rows, pty_cols) = pty_content_dims(&view.layout);
        let root_parser = Parser::new(pty_rows, pty_cols, VT100_SCROLLBACK_ROWS);

        let mut next_sub_id: u64 = 1;
        let pane_sub = next_sub_id;
        next_sub_id += 1;
        let docker_sub = next_sub_id;
        next_sub_id += 1;
        let ports_sub = next_sub_id;
        next_sub_id += 1;
        let processes_sub = next_sub_id;
        next_sub_id += 1;
        let fleet_sub = next_sub_id;
        next_sub_id += 1;

        let root_entry = PaneEntry {
            pane_id: pane,
            sub_id: pane_sub,
            label: pane_label_from_shell(&shell),
            parser: root_parser,
        };

        Self {
            view,
            pane_stack: vec![root_entry],
            active_pane: 0,
            pending_opens: VecDeque::new(),
            docker: DockerScope::new(docker_sub),
            ports: PortsScope::new(ports_sub, processes_sub),
            fleet: FleetScope::new(fleet_sub),
            terminal_size,
            next_sub_id,
            pending_actions: HashMap::new(),
            toasts: VecDeque::new(),
            help_visible: false,
            host_picker: None,
            host_capabilities: HashMap::new(),
            hovered_tile: None,
            last_click: None,
            input_filter: InputFilter::new(),
            scope_key_parser: ScopeKeyParser::default(),
        }
    }

    /// Currently-rendered pane id. Panics if the stack is empty — the
    /// invariant is maintained by `close_active_pane` (auto-reopens
    /// local root to keep the PTY tile from going blank) and by
    /// `handle_pane_event` on `PaneExit` (detaches when the last
    /// entry goes away).
    pub(crate) fn active_pane_id(&self) -> PaneId {
        self.pane_stack[self.active_pane].pane_id
    }

    /// Subscription id for the currently-rendered pane.
    pub(crate) fn active_pane_sub(&self) -> u64 {
        self.pane_stack[self.active_pane].sub_id
    }

    fn find_pane_by_sub(&self, sub_id: u64) -> Option<usize> {
        self.pane_stack.iter().position(|p| p.sub_id == sub_id)
    }

    /// Bootstrap actions for a fresh session: AttachPane, ResizePane
    /// (sized to the pty tile, not the whole terminal), Subscribe
    /// (Docker). All subscriptions are always-on for the life of the
    /// TUI; no mode switching.
    pub(crate) fn initial_actions(&mut self) -> Vec<AppAction> {
        let (pty_rows, pty_cols) = pty_content_dims(&self.view.layout);
        let pane_id = self.active_pane_id();
        let sub_id = self.active_pane_sub();
        vec![
            AppAction::SendEnvelope(envelope(Payload::AttachPane {
                pane_id,
                subscription_id: sub_id,
            })),
            AppAction::SendEnvelope(envelope(Payload::ResizePane {
                pane_id,
                rows: pty_rows,
                cols: pty_cols,
            })),
            AppAction::SendEnvelope(envelope(Payload::Subscribe(Subscription::Docker {
                id: self.docker.sub_id,
                target: self.docker.target.clone(),
            }))),
            AppAction::SendEnvelope(envelope(Payload::Subscribe(Subscription::Ports {
                id: self.ports.ports_sub_id,
                target: self.ports.ports_target.clone(),
            }))),
            AppAction::SendEnvelope(envelope(Payload::Subscribe(Subscription::Processes {
                id: self.ports.processes_sub_id,
                target: self.ports.processes_target.clone(),
            }))),
            AppAction::SendEnvelope(envelope(Payload::Subscribe(Subscription::Fleet {
                id: self.fleet.sub_id,
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
            AppEvent::Tick => {
                self.sweep_expired(Instant::now(), &mut actions);
                actions.push(AppAction::DrawFrame);
            }
            AppEvent::PendingActionTimeout(id) => {
                self.expire_pending_action(id, &mut actions);
            }
            AppEvent::MouseClick { x, y } => self.handle_mouse_click(x, y, &mut actions),
            AppEvent::MouseHover { x, y } => self.handle_mouse_hover(x, y, &mut actions),
        }
        actions
    }

    /// Left-mouse-button press at cell (x, y). Finds the tile
    /// containing the cell, focuses it, then dispatches tile-specific
    /// hit-testing:
    /// - PTY tile → tab-strip click switches the active pane;
    ///   `[×]` affordance closes it.
    /// - Scope tile → body-row click selects the row. A second click
    ///   on the same row within `DOUBLE_CLICK_WINDOW` fires the
    ///   tile's primary action (Fleet → open remote pane; Docker /
    ///   Ports have no primary action today).
    /// - Placeholder / TooSmall / border clicks → focus only.
    ///
    /// When the help overlay is visible, any click dismisses it and
    /// suppresses the underlying tile interaction — clicking "out of"
    /// the modal is the natural dismissal gesture.
    fn handle_mouse_click(&mut self, x: u16, y: u16, actions: &mut Vec<AppAction>) {
        self.handle_mouse_click_at(x, y, Instant::now(), actions);
    }

    /// Test-friendly variant: explicit `now` so unit tests can drive
    /// the double-click detection deterministically. Production path
    /// goes through [`App::handle_mouse_click`] which calls this with
    /// `Instant::now()`.
    pub(crate) fn handle_mouse_click_at(
        &mut self,
        x: u16,
        y: u16,
        now: Instant,
        actions: &mut Vec<AppAction>,
    ) {
        if self.help_visible {
            self.help_visible = false;
            actions.push(AppAction::DrawFrame);
            return;
        }

        // Phase 6 Slice 6c-iii: host picker absorbs clicks the same
        // way the help overlay does. A click anywhere dismisses —
        // commit is keyboard-driven (Enter). Click-to-commit-row
        // would require laying out + hit-testing modal rows; the
        // simpler "click = dismiss, keyboard = navigate/commit"
        // contract matches 6.0's help overlay precedent.
        if self.host_picker.is_some() {
            self.dismiss_host_picker(actions);
            return;
        }

        let pos = ratatui::layout::Position::new(x, y);
        let Some(tile) = self.view.layout.tiles.iter().find(|t| t.rect.contains(pos)) else {
            return;
        };
        let tile_id = tile.id;
        let rect = tile.rect;
        let kind = tile.kind.clone();

        self.move_focus_to(tile_id, actions);

        match kind {
            TileKind::Pty => self.dispatch_pty_click(rect, x, y, actions),
            TileKind::Scope(scope_kind) => {
                self.dispatch_scope_click(tile_id, scope_kind, rect, x, y, now, actions);
            }
            TileKind::Placeholder { .. } | TileKind::TooSmall => {
                // Focus already handled.
            }
        }
    }

    /// Tab-strip + close-affordance hit-testing for a click inside the
    /// PTY tile's rect. Non-strip clicks (border, vt100 content) are
    /// absorbed as focus-only.
    fn dispatch_pty_click(&mut self, rect: Rect, x: u16, y: u16, actions: &mut Vec<AppAction>) {
        let Some(inner) = inset_rect(rect) else {
            return;
        };
        // Tab strip is the top row of the inner area, height 1 when
        // `inner.height >= 2` (matches `pty_tile::render`). A zero-row
        // strip means the tile is too small to show tabs — treat the
        // click as focus-only.
        if inner.height < 2 || y != inner.y {
            return;
        }
        let strip = Rect::new(inner.x, inner.y, inner.width, 1);
        let Some(hit) = pty_tile::hit_test_tab_strip(&self.pane_stack, self.active_pane, strip, x)
        else {
            return;
        };
        match hit {
            pty_tile::TabStripHit::Tab(idx) => {
                if idx < self.pane_stack.len() && idx != self.active_pane {
                    self.active_pane = idx;
                    actions.push(AppAction::DrawFrame);
                }
            }
            pty_tile::TabStripHit::CloseActive => {
                self.close_active_pane(actions);
            }
        }
    }

    /// Scope-tile row-click dispatch + double-click primary-action
    /// detection. The body Y range sits between the status-bar row and
    /// the help-bar row, with an optional filter bar eating one more
    /// row at the top when the scope has an active or non-empty
    /// filter. Docker / Ports render a header row inside the body;
    /// Fleet does not — both branches are explicit below.
    fn dispatch_scope_click(
        &mut self,
        tile_id: TileId,
        kind: ScopeKind,
        rect: Rect,
        _x: u16,
        y: u16,
        now: Instant,
        actions: &mut Vec<AppAction>,
    ) {
        let Some(inner) = inset_rect(rect) else {
            return;
        };
        let filter_visible = self.scope_filter_visible(kind);
        let filter_offset: u16 = if filter_visible { 1 } else { 0 };

        // body_y_start (inclusive): first row after status bar (+
        // optional filter bar).
        let body_y_start = inner.y.saturating_add(1).saturating_add(filter_offset);
        // body_y_end (exclusive): help bar row.
        let body_y_end = inner.y.saturating_add(inner.height).saturating_sub(1);

        // Phase 6 Slice 6c-iii / 6d-ii: click on the tile's title
        // bar (y == rect.y, the border row) opens the host picker on
        // target-capable tiles. 6c-iii shipped Docker; 6d-ii adds
        // Ports (which retargets whichever view is active — Ports
        // or Processes). Other tiles / other Y positions fall
        // through to the standard scope-row dispatch.
        if y == rect.y && matches!(kind, ScopeKind::Docker | ScopeKind::Ports) {
            self.open_host_picker(actions);
            return;
        }

        if y < body_y_start || y >= body_y_end {
            return; // status / filter / help-bar click: focus only.
        }

        let row_idx = match kind {
            ScopeKind::Docker | ScopeKind::Ports => {
                // Header row at body_y_start; data rows start at
                // body_y_start + 1.
                if y <= body_y_start {
                    return;
                }
                (y - body_y_start - 1) as usize
            }
            ScopeKind::Fleet => {
                // No header row.
                (y - body_y_start) as usize
            }
        };

        let visible = match kind {
            ScopeKind::Docker => self.docker.visible_count(),
            ScopeKind::Ports => match self.ports.active {
                PortsActiveView::Ports => self.ports.ports.visible_count(),
                PortsActiveView::Processes => self.ports.processes.visible_count(),
            },
            ScopeKind::Fleet => self.fleet.visible_count(),
        };
        if row_idx >= visible {
            return;
        }

        // Apply the selection change.
        let prev_selection = self.scope_selection(kind);
        self.scope_set_selection(kind, row_idx);
        if self.scope_selection(kind) != prev_selection {
            actions.push(AppAction::DrawFrame);
        }

        // Double-click detection — same tile + same row + within the
        // Slice 6.0 double-click window. The window is 400 ms, tuned
        // slightly snappier than the usual 500 ms OS default because
        // TUI users click with intent more often than not.
        let is_double = matches!(
            self.last_click,
            Some((prev_tile, prev_row, prev_time))
                if prev_tile == tile_id
                    && prev_row == row_idx
                    && now.duration_since(prev_time) < DOUBLE_CLICK_WINDOW
        );
        self.last_click = Some((tile_id, row_idx, now));
        if is_double {
            match kind {
                ScopeKind::Fleet => self.dispatch_open_remote_pane(actions),
                // Docker / Ports have no row-level primary action
                // today; single-click + keyboard keybinds handle
                // their actions.
                ScopeKind::Docker | ScopeKind::Ports => {}
            }
        }
    }

    /// Returns true when the scope is currently rendering its filter
    /// bar — either it's actively being typed into, or a previously-
    /// committed filter string is still narrowing the list.
    fn scope_filter_visible(&self, kind: ScopeKind) -> bool {
        match kind {
            ScopeKind::Docker => self.docker.filter_active || !self.docker.filter.is_empty(),
            ScopeKind::Ports => match self.ports.active {
                PortsActiveView::Ports => {
                    self.ports.ports.filter_active || !self.ports.ports.filter.is_empty()
                }
                PortsActiveView::Processes => {
                    self.ports.processes.filter_active || !self.ports.processes.filter.is_empty()
                }
            },
            ScopeKind::Fleet => self.fleet.filter_active || !self.fleet.filter.is_empty(),
        }
    }

    fn scope_selection(&self, kind: ScopeKind) -> usize {
        match kind {
            ScopeKind::Docker => self.docker.selection,
            ScopeKind::Ports => match self.ports.active {
                PortsActiveView::Ports => self.ports.ports.selection,
                PortsActiveView::Processes => self.ports.processes.selection,
            },
            ScopeKind::Fleet => self.fleet.selection,
        }
    }

    fn scope_set_selection(&mut self, kind: ScopeKind, idx: usize) {
        match kind {
            ScopeKind::Docker => self.docker.selection = idx,
            ScopeKind::Ports => match self.ports.active {
                PortsActiveView::Ports => self.ports.ports.selection = idx,
                PortsActiveView::Processes => self.ports.processes.selection = idx,
            },
            ScopeKind::Fleet => self.fleet.selection = idx,
        }
    }

    /// Mouse motion at cell (x, y). Tracks which tile contains the
    /// pointer so the tile renderer can draw a distinct border style
    /// on hover. No-op when the coordinate falls outside every tile
    /// (e.g. the 1-column gutters that layout splits may create).
    fn handle_mouse_hover(&mut self, x: u16, y: u16, actions: &mut Vec<AppAction>) {
        let pos = ratatui::layout::Position::new(x, y);
        let hovered = self
            .view
            .layout
            .tiles
            .iter()
            .find(|t| t.rect.contains(pos))
            .map(|t| t.id);
        if hovered != self.hovered_tile {
            self.hovered_tile = hovered;
            actions.push(AppAction::DrawFrame);
        }
    }

    fn handle_stdin(&mut self, bytes: &[u8], actions: &mut Vec<AppAction>) {
        for input_action in self.input_filter.process(bytes) {
            // Slice 6.0: when the help overlay is visible, it absorbs
            // input as a modal — any keystroke (or any click, handled
            // in `handle_mouse_click`) dismisses it without reaching
            // the underlying tile. `Ctrl-b d` is preserved as an
            // escape hatch so the user can still detach from the
            // overlay; `Ctrl-b ?` is preserved as "toggle self."
            if self.help_visible {
                match input_action {
                    InputAction::Detach => {
                        actions.push(AppAction::Detach(DetachReason::User));
                        return;
                    }
                    InputAction::Help => {
                        self.handle_help_toggle(actions);
                    }
                    _ => {
                        self.help_visible = false;
                        actions.push(AppAction::DrawFrame);
                    }
                }
                continue;
            }

            // Phase 6 Slice 6c-iii: host picker absorbs input while
            // open. Navigation (arrows / j / k / Home / End) moves
            // selection; Enter commits; Esc / Ctrl-b t dismisses;
            // Ctrl-b d always detaches (escape hatch matching the
            // help overlay contract).
            if self.host_picker.is_some() {
                match input_action {
                    InputAction::Detach => {
                        actions.push(AppAction::Detach(DetachReason::User));
                        return;
                    }
                    InputAction::OpenHostPicker => {
                        self.dismiss_host_picker(actions);
                    }
                    InputAction::Help => {
                        // Help trumps picker — close picker + open help.
                        self.dismiss_host_picker(actions);
                        self.handle_help_toggle(actions);
                    }
                    InputAction::Forward(b) => {
                        for key in self.scope_key_parser.parse(&b) {
                            self.handle_host_picker_key(key, actions);
                            if self.host_picker.is_none() {
                                break;
                            }
                        }
                    }
                    _ => {
                        // Focus cycle / direction / PaneClose dismiss
                        // the modal without also triggering the
                        // underlying action — same logic as help's
                        // absorption. Keeps the modal predictable.
                        self.dismiss_host_picker(actions);
                    }
                }
                continue;
            }

            match input_action {
                InputAction::Forward(b) => self.handle_forward_bytes(b, actions),
                InputAction::Detach => {
                    actions.push(AppAction::Detach(DetachReason::User));
                    return;
                }
                InputAction::FocusDirection(dir) => self.handle_focus_direction(dir, actions),
                InputAction::FocusNext => self.handle_tab(true, actions),
                InputAction::FocusPrev => self.handle_tab(false, actions),
                InputAction::Help => self.handle_help_toggle(actions),
                InputAction::OpenHostPicker => self.open_host_picker(actions),
                InputAction::PaneClose => {
                    if self.view.layout.routes_to_pty(self.view.focused) {
                        self.close_active_pane(actions);
                    }
                }
            }
        }
    }

    fn handle_forward_bytes(&mut self, bytes: Vec<u8>, actions: &mut Vec<AppAction>) {
        if self.view.layout.routes_to_pty(self.view.focused) {
            let pane_id = self.active_pane_id();
            actions.push(AppAction::SendEnvelope(envelope(Payload::SendInput {
                pane_id,
                data: bytes,
            })));
            return;
        }
        match self.view.layout.routes_to_scope(self.view.focused) {
            Some(ScopeKind::Docker) => {
                for key in self.scope_key_parser.parse(&bytes) {
                    self.handle_scope_key(key, actions);
                }
            }
            Some(ScopeKind::Ports) => {
                for key in self.scope_key_parser.parse(&bytes) {
                    self.handle_ports_key(key, actions);
                }
            }
            Some(ScopeKind::Fleet) => {
                for key in self.scope_key_parser.parse(&bytes) {
                    self.handle_fleet_key(key, actions);
                }
            }
            None => {
                // Placeholder or TooSmall fall through: drop the bytes.
                // The tile renderer shows a "not yet implemented"
                // hint; no action needed here.
            }
        }
    }

    fn handle_focus_direction(&mut self, dir: FocusDir, actions: &mut Vec<AppAction>) {
        if let Some(next) = self.view.layout.next_focus(self.view.focused, dir) {
            self.move_focus_to(next, actions);
        }
    }

    /// `Tab` / `Shift-Tab` cycle focus through the fixed tile reading
    /// order. Scope-oblivious: the current focus is irrelevant except
    /// as the starting point. See `TileLayout::cycle_focus` for the
    /// order contract.
    fn handle_focus_cycle(&mut self, forward: bool, actions: &mut Vec<AppAction>) {
        if let Some(next) = self.view.layout.cycle_focus(self.view.focused, forward) {
            self.move_focus_to(next, actions);
        }
    }

    /// Slice 6.0.1 Tab-in-PTY carve-out. `Tab` / `Shift-Tab` cycle
    /// tile focus except when the PTY tile is focused, in which case
    /// the keystroke forwards to the pty byte stream so shell tab-
    /// completion (and any pty app that consumes Tab) works. PTY-
    /// focused Shift-Tab forwards the CSI Z sequence (`ESC [ Z`) —
    /// the same byte shape xterm sends on Shift-Tab — so readline /
    /// vim see what they'd see on a non-tepegoz terminal.
    fn handle_tab(&mut self, forward_cycle: bool, actions: &mut Vec<AppAction>) {
        if self.view.layout.routes_to_pty(self.view.focused) {
            let pane_id = self.active_pane_id();
            let data: Vec<u8> = if forward_cycle {
                vec![b'\t']
            } else {
                b"\x1b[Z".to_vec()
            };
            actions.push(AppAction::SendEnvelope(envelope(Payload::SendInput {
                pane_id,
                data,
            })));
            return;
        }
        self.handle_focus_cycle(forward_cycle, actions);
    }

    /// Shared focus-transition path used by directional, cycle, and
    /// click-based focus moves. Preserves the C3a UX clarification
    /// #3 invariant (focus-away from Docker cancels any pending
    /// confirm) and only emits frame updates on an actual change.
    fn move_focus_to(&mut self, next: TileId, actions: &mut Vec<AppAction>) {
        if next == self.view.focused {
            return;
        }
        if self.view.focused == TileId::Docker {
            self.docker.pending_confirm = None;
        }
        self.view.focused = next;
        actions.push(AppAction::FocusTile(next));
        actions.push(AppAction::DrawFrame);
    }

    /// `Ctrl-b ?` toggles the help overlay. Task E wires the actual
    /// overlay rendering + Esc dismissal; this stub keeps the input
    /// surface complete so the dispatch compiles.
    fn handle_help_toggle(&mut self, actions: &mut Vec<AppAction>) {
        self.help_visible = !self.help_visible;
        actions.push(AppAction::DrawFrame);
    }

    /// Build the picker's row list from the current Fleet state. Index
    /// 0 is always Local; indices 1..=N are each Fleet host in
    /// discovery order. Called by the render path + by commit /
    /// navigation key handlers that need to clamp against the row
    /// count.
    ///
    /// `required_capability` is the capability string the invoking
    /// tile requires (`"docker"` for the Docker tile; `"ports"` for
    /// Ports; `"processes"` for Processes). It's checked against the
    /// per-alias capability table populated from
    /// `Event::AgentCapabilities` — a host is `usable` iff its state
    /// is `Connected` AND its capability list contains the required
    /// string. Greyed rows distinguish "(not connected)" from
    /// "(no <cap>)" so the user sees which remediation applies.
    pub(crate) fn host_picker_rows(&self, required_capability: &str) -> Vec<HostPickerRow> {
        let mut rows = vec![HostPickerRow::Local];
        if let FleetScopeState::Available { hosts, states, .. } = &self.fleet.state {
            for h in hosts {
                let state = states
                    .get(&h.alias)
                    .copied()
                    .unwrap_or(tepegoz_proto::HostState::Disconnected);
                let has_capability = self
                    .host_capabilities
                    .get(&h.alias)
                    .map(|caps| caps.iter().any(|c| c == required_capability))
                    .unwrap_or(false);
                let usable = matches!(state, tepegoz_proto::HostState::Connected) && has_capability;
                rows.push(HostPickerRow::Remote {
                    alias: h.alias.clone(),
                    state,
                    has_capability,
                    usable,
                });
            }
        }
        rows
    }

    /// `Ctrl-b t` (or a click on the target-capable tile's title bar)
    /// opens the host picker modal. No-op if the focused tile isn't
    /// target-capable. 6c-iii shipped Docker; 6d-ii adds Ports +
    /// Processes — both reuse the same modal with their own
    /// required-capability string.
    ///
    /// Ports tile is dual-view (Ports + Processes via `p` toggle); the
    /// picker targets whichever view is currently active so a user
    /// reading processes can `Ctrl-b t` to retarget processes
    /// without having to think about the toggle state.
    ///
    /// Pre-selects the row matching the tile's current target so the
    /// modal opens with `▶` on "what's active" — the user can
    /// immediately press Enter to no-op-commit-to-same-target, which
    /// is sometimes the fastest way to reset the subscription.
    pub(crate) fn open_host_picker(&mut self, actions: &mut Vec<AppAction>) {
        let Some(scope) = self.view.layout.routes_to_scope(self.view.focused) else {
            return;
        };
        let (target_tile, required_capability, current_target) = match scope {
            ScopeKind::Docker => (
                HostPickerTargetTile::Docker,
                "docker",
                self.docker.target.clone(),
            ),
            ScopeKind::Ports => match self.ports.active {
                PortsActiveView::Ports => (
                    HostPickerTargetTile::Ports,
                    "ports",
                    self.ports.ports_target.clone(),
                ),
                PortsActiveView::Processes => (
                    HostPickerTargetTile::Processes,
                    "processes",
                    self.ports.processes_target.clone(),
                ),
            },
            // Fleet / placeholder tiles aren't target-capable.
            _ => return,
        };

        let rows = self.host_picker_rows(required_capability);
        let selected = rows
            .iter()
            .position(|r| match (r, &current_target) {
                (HostPickerRow::Local, tepegoz_proto::ScopeTarget::Local) => true,
                (
                    HostPickerRow::Remote { alias, .. },
                    tepegoz_proto::ScopeTarget::Remote { alias: t },
                ) => alias == t,
                _ => false,
            })
            .unwrap_or(0);

        self.host_picker = Some(HostPickerModal {
            target_tile,
            required_capability,
            selected,
        });
        actions.push(AppAction::DrawFrame);
    }

    /// Esc / click-outside / Ctrl-b t while open dismisses the picker.
    pub(crate) fn dismiss_host_picker(&mut self, actions: &mut Vec<AppAction>) {
        if self.host_picker.take().is_some() {
            actions.push(AppAction::DrawFrame);
        }
    }

    /// Navigation + Enter + Esc dispatch for an open picker. No-op if
    /// no picker is visible — caller checks beforehand and we return
    /// rather than panic on stale state.
    ///
    /// Mirrors the per-scope `Char(b'j')/Char(b'k') → Down/Up`
    /// translation the Docker / Ports / Fleet handlers do, so modal
    /// navigation feels consistent with the tile-focused navigation
    /// vocabulary the user already knows.
    pub(crate) fn handle_host_picker_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        let required = self
            .host_picker
            .as_ref()
            .map(|p| p.required_capability)
            .unwrap_or("");
        let rows_len = self.host_picker_rows(required).len();
        let Some(picker) = &mut self.host_picker else {
            return;
        };
        match key {
            ScopeKey::Up | ScopeKey::Char(b'k') => {
                if picker.selected > 0 {
                    picker.selected -= 1;
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::Down | ScopeKey::Char(b'j') => {
                if picker.selected + 1 < rows_len {
                    picker.selected += 1;
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::Home | ScopeKey::Top | ScopeKey::Char(b'g') => {
                if picker.selected != 0 {
                    picker.selected = 0;
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::End | ScopeKey::Bottom | ScopeKey::Char(b'G') => {
                if rows_len > 0 && picker.selected != rows_len - 1 {
                    picker.selected = rows_len - 1;
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::Enter => {
                self.commit_host_picker(actions);
            }
            ScopeKey::Escape => {
                self.dismiss_host_picker(actions);
            }
            _ => {
                // Absorb silently — typed letters, etc.
            }
        }
    }

    /// Commit the currently-selected picker row: dispatch a retarget
    /// against the picker's tile, then close the modal. No-op on a
    /// greyed-out row (host not in Connected state) — the modal stays
    /// open so the user can pick again.
    fn commit_host_picker(&mut self, actions: &mut Vec<AppAction>) {
        let required = self
            .host_picker
            .as_ref()
            .map(|p| p.required_capability)
            .unwrap_or("");
        let rows = self.host_picker_rows(required);
        let Some(picker) = self.host_picker.as_ref() else {
            return;
        };
        let Some(row) = rows.get(picker.selected) else {
            self.host_picker = None;
            actions.push(AppAction::DrawFrame);
            return;
        };
        let target_tile = picker.target_tile;
        let new_target = match row {
            HostPickerRow::Local => tepegoz_proto::ScopeTarget::Local,
            HostPickerRow::Remote {
                alias,
                usable: true,
                ..
            } => tepegoz_proto::ScopeTarget::Remote {
                alias: alias.clone(),
            },
            HostPickerRow::Remote { usable: false, .. } => {
                // Greyed-out row — don't commit. Leave the modal open
                // so the user can pick a different row.
                return;
            }
        };
        self.host_picker = None;
        match target_tile {
            HostPickerTargetTile::Docker => self.retarget_docker(new_target, actions),
            HostPickerTargetTile::Ports => self.retarget_ports(new_target, actions),
            HostPickerTargetTile::Processes => self.retarget_processes(new_target, actions),
        }
    }

    /// Apply a new target to the Docker tile: unsubscribe any active
    /// logs sub, unsubscribe the list sub, reset the view + state to
    /// Connecting, resubscribe with the new target. No-op if the
    /// target hasn't changed.
    ///
    /// We reset `selection` + `pending_confirm` on retarget because
    /// they're indexed into a container-list snapshot that's about to
    /// be replaced from a different host — preserving them across
    /// retarget would mean "row 3 of old host" stuck on "row 3 of new
    /// host" with no semantic continuity.
    fn retarget_docker(
        &mut self,
        new_target: tepegoz_proto::ScopeTarget,
        actions: &mut Vec<AppAction>,
    ) {
        if self.docker.target == new_target {
            actions.push(AppAction::DrawFrame);
            return;
        }

        // If in logs view, exit it + unsubscribe the logs sub before
        // retargeting the list. Otherwise the logs sub lingers bound
        // to the old target's container id.
        if let DockerView::Logs(logs) = &self.docker.view {
            actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
                id: logs.sub_id,
            })));
            self.docker.view = DockerView::List;
        }

        actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
            id: self.docker.sub_id,
        })));

        self.docker.state = DockerScopeState::Connecting;
        self.docker.selection = 0;
        self.docker.pending_confirm = None;
        self.docker.target = new_target.clone();

        actions.push(AppAction::SendEnvelope(envelope(Payload::Subscribe(
            Subscription::Docker {
                id: self.docker.sub_id,
                target: new_target,
            },
        ))));
        actions.push(AppAction::DrawFrame);
    }

    /// Phase 6 Slice 6d-ii: apply a new target to the Ports view.
    /// Same shape as `retarget_docker`: unsub old, reset state to
    /// Connecting + selection 0, resub with new target. Same-target
    /// commit is a no-op. Independent of the Processes view's target.
    fn retarget_ports(
        &mut self,
        new_target: tepegoz_proto::ScopeTarget,
        actions: &mut Vec<AppAction>,
    ) {
        if self.ports.ports_target == new_target {
            actions.push(AppAction::DrawFrame);
            return;
        }
        actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
            id: self.ports.ports_sub_id,
        })));
        self.ports.ports.state = PortsViewState::Connecting;
        self.ports.ports.selection = 0;
        self.ports.ports_target = new_target.clone();
        actions.push(AppAction::SendEnvelope(envelope(Payload::Subscribe(
            Subscription::Ports {
                id: self.ports.ports_sub_id,
                target: new_target,
            },
        ))));
        actions.push(AppAction::DrawFrame);
    }

    /// Phase 6 Slice 6d-ii: apply a new target to the Processes view.
    /// Mirrors `retarget_ports`. Independent of the Ports view's
    /// target.
    fn retarget_processes(
        &mut self,
        new_target: tepegoz_proto::ScopeTarget,
        actions: &mut Vec<AppAction>,
    ) {
        if self.ports.processes_target == new_target {
            actions.push(AppAction::DrawFrame);
            return;
        }
        actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
            id: self.ports.processes_sub_id,
        })));
        self.ports.processes.state = ProcessesViewState::Connecting;
        self.ports.processes.selection = 0;
        self.ports.processes_target = new_target.clone();
        actions.push(AppAction::SendEnvelope(envelope(Payload::Subscribe(
            Subscription::Processes {
                id: self.ports.processes_sub_id,
                target: new_target,
            },
        ))));
        actions.push(AppAction::DrawFrame);
    }

    fn handle_scope_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        // Logs sub-state has its own keybind map. Confirm modal is
        // unreachable while logs are showing (both live in the
        // Docker tile's Rect; logs has higher priority and
        // suppresses the list).
        if matches!(self.docker.view, DockerView::Logs(_)) {
            self.handle_logs_key(key, actions);
            return;
        }

        // Confirm modal takes priority: while visible, `y`/`Y`
        // confirms + dispatches; a repeat of the destructive keys
        // (`K` / `X`) is absorbed so the second press never silently
        // switches the modal's target mid-prompt; anything else
        // cancels (UX clarification #3).
        if let Some(pending) = self.docker.pending_confirm.clone() {
            match key {
                ScopeKey::Char(b'y') | ScopeKey::Char(b'Y') => {
                    self.docker.pending_confirm = None;
                    self.dispatch_docker_action(
                        pending.container_id,
                        pending.container_name,
                        pending.kind,
                        actions,
                    );
                }
                ScopeKey::Char(b'K') | ScopeKey::Char(b'X') => {
                    // Absorb — modal stays showing the original kind.
                    // No state change, no redraw needed.
                }
                _ => {
                    self.docker.pending_confirm = None;
                    actions.push(AppAction::DrawFrame);
                }
            }
            return;
        }

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
                | ScopeKey::PgUp
                | ScopeKey::PgDn
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
            // Lowercase-only for non-destructive actions — matches the
            // capital-only rule for destructive K/X and the
            // lowercase-only rule for navigation (h/j/k/l). One
            // consistent convention: capital = destructive, lowercase
            // = safe.
            ScopeKey::Char(b'r') => {
                self.issue_selected_docker_action(DockerActionKind::Restart, actions);
            }
            ScopeKey::Char(b's') => {
                self.issue_selected_docker_action(DockerActionKind::Stop, actions);
            }
            ScopeKey::Char(b'K') => self.begin_confirm(DockerActionKind::Kill, actions),
            ScopeKey::Char(b'X') => self.begin_confirm(DockerActionKind::Remove, actions),
            ScopeKey::Char(b'l') => self.try_enter_logs_view(actions),
            ScopeKey::Escape => {}
            ScopeKey::Enter => {} // Slice D uses this for DockerExec.
            ScopeKey::PgUp | ScopeKey::PgDn => {} // List view has no paging.
            ScopeKey::Backspace | ScopeKey::Char(_) => {}
        }
    }

    /// Ports tile key map. Routes to whichever of the two co-resident
    /// views is active (Ports | Processes). `p` toggles between them;
    /// each view keeps its own filter + selection independently.
    /// Selection persists across refreshes via stable keys —
    /// `(protocol, local_port, pid)` for Ports and `(pid, start_time)`
    /// for Processes (the latter guards against pid reuse on a
    /// short-lived process).
    fn handle_ports_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        // `p` toggles views at the outer scope — absorbed before
        // filter / nav dispatch so toggling while a filter is typed
        // doesn't swallow the `p` as a filter character. Uppercase
        // `P` is reserved (destructive-verb discipline) — no-op here.
        if matches!(key, ScopeKey::Char(b'p')) {
            // Never toggle while a filter input is active — `p`
            // should be a valid filter character in that state.
            let filter_active = match self.ports.active {
                PortsActiveView::Ports => self.ports.ports.filter_active,
                PortsActiveView::Processes => self.ports.processes.filter_active,
            };
            if !filter_active {
                self.ports.active = match self.ports.active {
                    PortsActiveView::Ports => PortsActiveView::Processes,
                    PortsActiveView::Processes => PortsActiveView::Ports,
                };
                actions.push(AppAction::DrawFrame);
                return;
            }
        }

        match self.ports.active {
            PortsActiveView::Ports => self.handle_ports_list_key(key, actions),
            PortsActiveView::Processes => self.handle_processes_list_key(key, actions),
        }
    }

    fn handle_fleet_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        if self.fleet.filter_active {
            match key {
                ScopeKey::Escape => {
                    self.fleet.filter.clear();
                    self.fleet.filter_active = false;
                    let old_key = self.fleet.selected_key();
                    self.fleet.reanchor_selection(old_key);
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Enter => {
                    self.fleet.filter_active = false;
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Backspace => {
                    if self.fleet.filter.pop().is_some() {
                        let old_key = self.fleet.selected_key();
                        self.fleet.reanchor_selection(old_key);
                        actions.push(AppAction::DrawFrame);
                    }
                }
                ScopeKey::Char(b) => {
                    if (0x20..=0x7e).contains(&b) {
                        self.fleet.filter.push(b as char);
                        let old_key = self.fleet.selected_key();
                        self.fleet.reanchor_selection(old_key);
                        actions.push(AppAction::DrawFrame);
                    }
                }
                _ => {}
            }
            return;
        }

        match key {
            ScopeKey::Up => {
                self.fleet.selection = self.fleet.selection.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Down => {
                let n = self.fleet.visible_count();
                if n > 0 && self.fleet.selection + 1 < n {
                    self.fleet.selection += 1;
                }
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Top | ScopeKey::Home => {
                self.fleet.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Bottom | ScopeKey::End => {
                let n = self.fleet.visible_count();
                self.fleet.selection = n.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::FilterStart => {
                self.fleet.filter_active = true;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Char(b'j') => self.handle_fleet_key(ScopeKey::Down, actions),
            ScopeKey::Char(b'k') => self.handle_fleet_key(ScopeKey::Up, actions),
            ScopeKey::Char(b'g') => self.handle_fleet_key(ScopeKey::Top, actions),
            ScopeKey::Char(b'G') => self.handle_fleet_key(ScopeKey::Bottom, actions),
            ScopeKey::Char(b'/') => self.handle_fleet_key(ScopeKey::FilterStart, actions),
            // `r` dispatches a FleetAction::Reconnect against the
            // selected host (lazy-connects that need user action, or
            // terminal states awaiting a retry). Lowercase per the
            // capital-discipline rule — reconnecting is safe / non-
            // destructive. Uppercase `R` is explicitly a no-op.
            ScopeKey::Char(b'r') => self.dispatch_fleet_reconnect(actions),
            // Slice 6.0: plain `Enter` on a focused Fleet tile is the
            // "primary action on the selected row" per the amended
            // Decision #7 — opens a remote pane for the selected
            // host. Supersedes the pre-6.0 `Ctrl-b Enter` keybind.
            ScopeKey::Enter => self.dispatch_open_remote_pane(actions),
            _ => {}
        }
    }

    /// Open a fresh remote pane for the Fleet tile's selected row.
    /// Called by plain `Enter` on the focused Fleet tile (or a
    /// double-click on the row in Slice 6.0+). Allocates
    /// a sub id, pushes a `PendingOpen` to correlate the response,
    /// and sends `OpenPane { target: Remote { alias } }`. The
    /// follow-up `AttachPane` + stack insertion happens when
    /// `PaneOpened` arrives in `handle_pane_opened`; a failure lands
    /// in `handle_daemon_envelope`'s `Error` arm via FIFO correlation.
    fn dispatch_open_remote_pane(&mut self, actions: &mut Vec<AppAction>) {
        let Some(host) = self.fleet.selected_host() else {
            return;
        };
        let alias = host.alias.clone();
        let (pty_rows, pty_cols) = pty_content_dims(&self.view.layout);
        let sub_id = self.alloc_sub_id();
        self.pending_opens.push_back(PendingOpen {
            sub_id,
            alias: Some(alias.clone()),
        });
        actions.push(AppAction::SendEnvelope(envelope(Payload::OpenPane(
            OpenPaneSpec {
                shell: None,
                cwd: None,
                env: Vec::new(),
                rows: pty_rows,
                cols: pty_cols,
                target: PaneTarget::Remote {
                    alias: alias.clone(),
                },
            },
        ))));
        self.push_toast(ToastKind::Info, format!("opening ssh:{alias}…"), actions);
    }

    /// Close the active pane. Sends `ClosePane` to the daemon,
    /// eagerly removes the entry from the stack, and opens a fresh
    /// local root pane if the stack would otherwise go empty so the
    /// PTY tile is never blank. Stale `PaneExit` / `PaneOutput`
    /// events that arrive after the close drop silently in
    /// `handle_daemon_envelope` via `find_pane_by_sub`.
    fn close_active_pane(&mut self, actions: &mut Vec<AppAction>) {
        if self.pane_stack.is_empty() {
            return;
        }
        let removed = self.pane_stack.remove(self.active_pane);
        actions.push(AppAction::SendEnvelope(envelope(Payload::ClosePane {
            pane_id: removed.pane_id,
        })));
        // Unsubscribe too — belt-and-suspenders. The daemon aborts
        // the forwarder on ClosePane, but the uniform subscription
        // shape means Unsubscribe works cleanly for pane subs as
        // well, and stale byte buffers flushing to a discarded sub
        // would otherwise surface in the toast path through the
        // FIFO Error correlation.
        actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
            id: removed.sub_id,
        })));
        if self.pane_stack.is_empty() {
            // Open a fresh local root so the PTY tile never goes
            // blank (pane `removed` might have been remote; local
            // is the safe default).
            let (pty_rows, pty_cols) = pty_content_dims(&self.view.layout);
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.into_os_string().into_string().ok());
            let sub_id = self.alloc_sub_id();
            self.pending_opens.push_back(PendingOpen {
                sub_id,
                alias: None,
            });
            actions.push(AppAction::SendEnvelope(envelope(Payload::OpenPane(
                OpenPaneSpec {
                    shell: None,
                    cwd,
                    env: Vec::new(),
                    rows: pty_rows,
                    cols: pty_cols,
                    target: PaneTarget::Local,
                },
            ))));
            self.active_pane = 0; // will be valid once PaneOpened pushes the entry
        } else {
            if self.active_pane >= self.pane_stack.len() {
                self.active_pane = self.pane_stack.len() - 1;
            }
            actions.push(AppAction::DrawFrame);
        }
    }

    /// Lookup the selected Fleet host + dispatch a
    /// `FleetAction::Reconnect`. Registers a pending action for toast
    /// correlation (same pattern as `dispatch_docker_action`).
    fn dispatch_fleet_reconnect(&mut self, actions: &mut Vec<AppAction>) {
        let Some(host) = self.fleet.selected_host() else {
            return;
        };
        let alias = host.alias.clone();
        let request_id = self.alloc_sub_id();
        self.pending_actions.insert(
            request_id,
            PendingAction {
                deadline: Instant::now() + PENDING_ACTION_TIMEOUT,
                description: format!("reconnect {alias}"),
            },
        );
        actions.push(AppAction::SendEnvelope(envelope(Payload::FleetAction(
            FleetActionRequest {
                request_id,
                alias,
                kind: FleetActionKind::Reconnect,
            },
        ))));
        actions.push(AppAction::DrawFrame);
    }

    fn handle_ports_list_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        if self.ports.ports.filter_active {
            match key {
                ScopeKey::Escape => {
                    self.ports.ports.filter.clear();
                    self.ports.ports.filter_active = false;
                    let old_key = self.ports.ports.selected_key();
                    self.ports.ports.reanchor_selection(old_key);
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Enter => {
                    self.ports.ports.filter_active = false;
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Backspace => {
                    if self.ports.ports.filter.pop().is_some() {
                        let old_key = self.ports.ports.selected_key();
                        self.ports.ports.reanchor_selection(old_key);
                        actions.push(AppAction::DrawFrame);
                    }
                }
                ScopeKey::Char(b) => {
                    if (0x20..=0x7e).contains(&b) {
                        self.ports.ports.filter.push(b as char);
                        let old_key = self.ports.ports.selected_key();
                        self.ports.ports.reanchor_selection(old_key);
                        actions.push(AppAction::DrawFrame);
                    }
                }
                _ => {}
            }
            return;
        }

        match key {
            ScopeKey::Up => {
                self.ports.ports.selection = self.ports.ports.selection.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Down => {
                let n = self.ports.ports.visible_count();
                if n > 0 && self.ports.ports.selection + 1 < n {
                    self.ports.ports.selection += 1;
                }
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Top | ScopeKey::Home => {
                self.ports.ports.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Bottom | ScopeKey::End => {
                let n = self.ports.ports.visible_count();
                self.ports.ports.selection = n.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::FilterStart => {
                self.ports.ports.filter_active = true;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Char(b'j') => self.handle_ports_list_key(ScopeKey::Down, actions),
            ScopeKey::Char(b'k') => self.handle_ports_list_key(ScopeKey::Up, actions),
            ScopeKey::Char(b'g') => self.handle_ports_list_key(ScopeKey::Top, actions),
            ScopeKey::Char(b'G') => self.handle_ports_list_key(ScopeKey::Bottom, actions),
            ScopeKey::Char(b'/') => self.handle_ports_list_key(ScopeKey::FilterStart, actions),
            _ => {}
        }
    }

    fn handle_processes_list_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        if self.ports.processes.filter_active {
            match key {
                ScopeKey::Escape => {
                    self.ports.processes.filter.clear();
                    self.ports.processes.filter_active = false;
                    let old_key = self.ports.processes.selected_key();
                    self.ports.processes.reanchor_selection(old_key);
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Enter => {
                    self.ports.processes.filter_active = false;
                    actions.push(AppAction::DrawFrame);
                }
                ScopeKey::Backspace => {
                    if self.ports.processes.filter.pop().is_some() {
                        let old_key = self.ports.processes.selected_key();
                        self.ports.processes.reanchor_selection(old_key);
                        actions.push(AppAction::DrawFrame);
                    }
                }
                ScopeKey::Char(b) => {
                    if (0x20..=0x7e).contains(&b) {
                        self.ports.processes.filter.push(b as char);
                        let old_key = self.ports.processes.selected_key();
                        self.ports.processes.reanchor_selection(old_key);
                        actions.push(AppAction::DrawFrame);
                    }
                }
                _ => {}
            }
            return;
        }

        match key {
            ScopeKey::Up => {
                self.ports.processes.selection = self.ports.processes.selection.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Down => {
                let n = self.ports.processes.visible_count();
                if n > 0 && self.ports.processes.selection + 1 < n {
                    self.ports.processes.selection += 1;
                }
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Top | ScopeKey::Home => {
                self.ports.processes.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Bottom | ScopeKey::End => {
                let n = self.ports.processes.visible_count();
                self.ports.processes.selection = n.saturating_sub(1);
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::FilterStart => {
                self.ports.processes.filter_active = true;
                actions.push(AppAction::DrawFrame);
            }
            ScopeKey::Char(b'j') => self.handle_processes_list_key(ScopeKey::Down, actions),
            ScopeKey::Char(b'k') => self.handle_processes_list_key(ScopeKey::Up, actions),
            ScopeKey::Char(b'g') => self.handle_processes_list_key(ScopeKey::Top, actions),
            ScopeKey::Char(b'G') => self.handle_processes_list_key(ScopeKey::Bottom, actions),
            ScopeKey::Char(b'/') => self.handle_processes_list_key(ScopeKey::FilterStart, actions),
            _ => {}
        }
    }

    /// Logs sub-state key map. Narrow — only scrolling + exit, since
    /// the Docker tile's logs view is a read-only transcript.
    fn handle_logs_key(&mut self, key: ScopeKey, actions: &mut Vec<AppAction>) {
        // Exit path must read the sub_id before mutating
        // `self.docker.view` (dropping the LogsView also drops its
        // fields), so pull the id up front for the Esc/q arms.
        let current_logs_sub = match &self.docker.view {
            DockerView::Logs(l) => Some(l.sub_id),
            _ => return,
        };

        match key {
            ScopeKey::Escape | ScopeKey::Char(b'q') => {
                if let Some(sub_id) = current_logs_sub {
                    self.docker.view = DockerView::List;
                    actions.push(AppAction::SendEnvelope(envelope(Payload::Unsubscribe {
                        id: sub_id,
                    })));
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::Up | ScopeKey::Char(b'k') => {
                if let DockerView::Logs(logs) = &mut self.docker.view {
                    logs.scroll_up(1);
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::Down | ScopeKey::Char(b'j') => {
                if let DockerView::Logs(logs) = &mut self.docker.view {
                    logs.scroll_down(1);
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::PgUp => {
                if let DockerView::Logs(logs) = &mut self.docker.view {
                    logs.scroll_up(LOGS_PAGE_LINES);
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::PgDn => {
                if let DockerView::Logs(logs) = &mut self.docker.view {
                    logs.scroll_down(LOGS_PAGE_LINES);
                    actions.push(AppAction::DrawFrame);
                }
            }
            ScopeKey::Char(b'G') | ScopeKey::Bottom | ScopeKey::End => {
                if let DockerView::Logs(logs) = &mut self.docker.view {
                    logs.jump_to_tail();
                    actions.push(AppAction::DrawFrame);
                }
            }
            // Everything else (filter slash, arrow keys not covered,
            // Enter, Backspace, any other char) is dropped — the
            // logs view is read-only.
            _ => {}
        }
    }

    /// `l` on the list view: if a container is selected, allocate a
    /// sub_id, send `Subscribe(DockerLogs { follow: true, tail_lines:
    /// 0 })`, and transition into the logs sub-state. No-op when
    /// nothing is selected (Unavailable or empty list).
    fn try_enter_logs_view(&mut self, actions: &mut Vec<AppAction>) {
        let Some(container) = self.docker.selected_container() else {
            return;
        };
        let container_id = container.id.clone();
        let container_name = display_name_for(container);
        let sub_id = self.alloc_sub_id();
        self.docker.view =
            DockerView::Logs(LogsView::new(container_id.clone(), container_name, sub_id));
        actions.push(AppAction::SendEnvelope(envelope(Payload::Subscribe(
            Subscription::DockerLogs {
                id: sub_id,
                container_id,
                follow: true,
                // "all history" per Slice B's wire contract. For
                // chatty long-lived containers this could dump
                // megabytes on entry; revisit with a bounded default
                // as a Phase-3 polish item.
                tail_lines: 0,
                // v11: logs follow the Docker tile's current target
                // so remote-Docker logs stream through the agent.
                target: self.docker.target.clone(),
            },
        ))));
        actions.push(AppAction::DrawFrame);
    }

    /// Dispatch a DockerAction against the currently selected
    /// container (if any). No-op when nothing is selected or the
    /// docker scope is not in `Available`.
    fn issue_selected_docker_action(
        &mut self,
        kind: DockerActionKind,
        actions: &mut Vec<AppAction>,
    ) {
        let Some(container) = self.docker.selected_container() else {
            return;
        };
        let container_id = container.id.clone();
        let display_name = display_name_for(container);
        self.dispatch_docker_action(container_id, display_name, kind, actions);
    }

    /// Register a PendingAction + emit the `DockerAction` envelope. The
    /// daemon replies with `DockerActionResult` carrying the same
    /// request_id; [`App::handle_daemon_envelope`] correlates.
    fn dispatch_docker_action(
        &mut self,
        container_id: String,
        display_name: String,
        kind: DockerActionKind,
        actions: &mut Vec<AppAction>,
    ) {
        let request_id = self.alloc_sub_id();
        let description = format!("{} {display_name}", action_verb(kind));
        self.pending_actions.insert(
            request_id,
            PendingAction {
                deadline: Instant::now() + PENDING_ACTION_TIMEOUT,
                description,
            },
        );
        actions.push(AppAction::SendEnvelope(envelope(Payload::DockerAction(
            DockerActionRequest {
                request_id,
                container_id,
                kind,
                // v11: action follows the Docker tile's current
                // target so remote containers get acted on via the
                // agent.
                target: self.docker.target.clone(),
            },
        ))));
        actions.push(AppAction::DrawFrame);
    }

    /// Begin a pending confirm prompt (K / X keybinds). No-op when no
    /// container is selected — we have nothing to ask about.
    fn begin_confirm(&mut self, kind: DockerActionKind, actions: &mut Vec<AppAction>) {
        let Some(container) = self.docker.selected_container() else {
            return;
        };
        let container_id = container.id.clone();
        let display_name = display_name_for(container);
        self.docker.pending_confirm = Some(PendingConfirm {
            kind,
            container_id,
            container_name: display_name,
            deadline: Instant::now() + PENDING_CONFIRM_TIMEOUT,
        });
        actions.push(AppAction::DrawFrame);
    }

    /// Push a toast onto the overlay, drop the oldest if the queue is
    /// already at [`MAX_TOASTS`]. Also emits `AppAction::ShowToast` so
    /// the runtime's `tui.log` carries a trace of every toast.
    ///
    /// `now` is taken explicitly so the expire-sweep can drive toast
    /// lifetimes relative to its synthetic clock in tests. Production
    /// callers pass `Instant::now()`.
    fn push_toast_at(
        &mut self,
        now: Instant,
        kind: ToastKind,
        message: String,
        actions: &mut Vec<AppAction>,
    ) {
        let duration = match kind {
            ToastKind::Success => TOAST_SUCCESS_DURATION,
            ToastKind::Error => TOAST_ERROR_DURATION,
            ToastKind::Info => TOAST_INFO_DURATION,
        };
        if self.toasts.len() >= MAX_TOASTS {
            self.toasts.pop_front();
        }
        self.toasts.push_back(Toast {
            kind,
            message: message.clone(),
            expires_at: now + duration,
        });
        actions.push(AppAction::ShowToast { kind, message });
        actions.push(AppAction::DrawFrame);
    }

    /// Convenience for non-sweep callers: uses `Instant::now()` as the
    /// clock reference.
    fn push_toast(&mut self, kind: ToastKind, message: String, actions: &mut Vec<AppAction>) {
        self.push_toast_at(Instant::now(), kind, message, actions);
    }

    /// Expire one specific pending action — called by both the Tick
    /// sweep and any runtime-driven `PendingActionTimeout` event.
    /// `now` is the clock reference used for the resulting timeout
    /// toast's `expires_at`.
    fn expire_pending_action_at(&mut self, now: Instant, id: u64, actions: &mut Vec<AppAction>) {
        if let Some(pa) = self.pending_actions.remove(&id) {
            self.push_toast_at(
                now,
                ToastKind::Error,
                format!("{} timed out — check engine", pa.description),
                actions,
            );
        }
    }

    fn expire_pending_action(&mut self, id: u64, actions: &mut Vec<AppAction>) {
        self.expire_pending_action_at(Instant::now(), id, actions);
    }

    /// Tick-driven sweep: expire pending confirms (10 s), pending
    /// actions (30 s, each emits a timeout toast), and stale toasts
    /// (per-kind duration).
    ///
    /// Factored out of [`App::handle_event`] so tests can drive expiry
    /// without waiting on wall-clock time — pass an `Instant::now() +
    /// Duration::from_secs(N)` to simulate the sweep at N seconds in
    /// the future. All toast lifetimes are computed against the same
    /// `now` so a freshly-pushed timeout toast isn't immediately
    /// evicted in the same pass.
    fn sweep_expired(&mut self, now: Instant, actions: &mut Vec<AppAction>) {
        let confirm_expired = self
            .docker
            .pending_confirm
            .as_ref()
            .is_some_and(|pc| now >= pc.deadline);
        if confirm_expired {
            self.docker.pending_confirm = None;
        }

        let expired: Vec<u64> = self
            .pending_actions
            .iter()
            .filter_map(|(&id, pa)| (now >= pa.deadline).then_some(id))
            .collect();
        for id in expired {
            self.expire_pending_action_at(now, id, actions);
        }

        self.toasts.retain(|t| now < t.expires_at);
    }

    fn handle_daemon_envelope(&mut self, env: Envelope, actions: &mut Vec<AppAction>) {
        match env.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event,
            }) => {
                if let Some(idx) = self.find_pane_by_sub(subscription_id) {
                    self.handle_pane_event(idx, event, actions);
                } else if subscription_id == self.docker.sub_id {
                    self.handle_docker_event(event, actions);
                } else if self.docker.is_current_logs_sub(subscription_id) {
                    self.handle_logs_event(event, actions);
                } else if subscription_id == self.ports.ports_sub_id {
                    self.handle_ports_event(event, actions);
                } else if subscription_id == self.ports.processes_sub_id {
                    self.handle_processes_event(event, actions);
                } else if subscription_id == self.fleet.sub_id {
                    self.handle_fleet_event(event, actions);
                }
                // Unknown sub id: stale event from a sub we've closed.
                // Drop silently.
            }
            Payload::PaneOpened(info) => {
                self.handle_pane_opened(info, actions);
            }
            Payload::Error(info) => {
                // FIFO correlation to an in-flight `OpenPane`: consume
                // the head of `pending_opens` ONLY when the daemon's
                // error message literally starts with one of the
                // OpenPane failure-path prefixes. Without this guard,
                // an unrelated Error (SendInput against an unknown
                // pane, AttachPane race with ClosePane) would
                // mis-consume a later OpenPane's slot while human-
                // paced interaction is typing through the pipe. The
                // prefixes are defined by the daemon at
                // `tepegoz-core::client`'s open-pane arms:
                //   `"open pane: ..."` (local failure)
                //   `"open remote pane (<alias>): ..."` (remote failure)
                // A proper fix lands in Phase 6 when the wire bump
                // for agent-backed panes adds per-request ids to
                // `OpenPane` (`docs/ISSUES.md#fifo-openpane-correlation-edge`).
                let is_open_failure = info.message.starts_with("open pane")
                    || info.message.starts_with("open remote pane");
                if is_open_failure && let Some(pending) = self.pending_opens.pop_front() {
                    let alias = pending.alias.as_deref().unwrap_or("local");
                    let message = format!("open pane {alias} failed: {}", info.message);
                    self.push_toast(ToastKind::Error, message, actions);
                    return;
                }
                let message = format!("daemon error {:?}: {}", info.kind, info.message);
                self.push_toast(ToastKind::Error, message, actions);
            }
            Payload::DockerActionResult(result) => {
                let pending = self.pending_actions.remove(&result.request_id);
                // Fallback description: the App never issued this
                // action (or its deadline fired first) — still surface
                // the outcome so the user isn't in the dark.
                let description = pending.map(|p| p.description).unwrap_or_else(|| {
                    format!("{} {}", action_verb(result.kind), result.container_id)
                });
                match result.outcome {
                    DockerActionOutcome::Success => {
                        self.push_toast(
                            ToastKind::Success,
                            format!("{description} — succeeded"),
                            actions,
                        );
                    }
                    DockerActionOutcome::Failure { reason } => {
                        self.push_toast(
                            ToastKind::Error,
                            format!("{description} failed: {reason}"),
                            actions,
                        );
                    }
                }
            }
            Payload::FleetActionResult(result) => {
                let pending = self.pending_actions.remove(&result.request_id);
                let description = pending.map(|p| p.description).unwrap_or_else(|| {
                    format!("{} {}", fleet_action_verb(result.kind), result.alias)
                });
                match result.outcome {
                    FleetActionOutcome::Success => {
                        // "Dispatched" is the success criterion —
                        // actual connection outcome arrives via
                        // HostStateChanged. Keep the toast short so
                        // it doesn't compete with the state-change
                        // toast that follows seconds later.
                        self.push_toast(
                            ToastKind::Info,
                            format!("{description} — dispatched"),
                            actions,
                        );
                    }
                    FleetActionOutcome::Failure { reason } => {
                        self.push_toast(
                            ToastKind::Error,
                            format!("{description} failed: {reason}"),
                            actions,
                        );
                    }
                }
            }
            // Welcome, Pong, PaneOpened, PaneList — consumed by the
            // handshake / ensure_pane reads, not the event loop.
            _ => {}
        }
    }

    fn handle_pane_event(&mut self, idx: usize, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::PaneSnapshot { scrollback, .. } => {
                if !scrollback.is_empty() {
                    self.pane_stack[idx].parser.process(&scrollback);
                    if idx == self.active_pane {
                        actions.push(AppAction::DrawFrame);
                    }
                }
            }
            Event::PaneOutput { data } => {
                self.pane_stack[idx].parser.process(&data);
                if idx == self.active_pane {
                    actions.push(AppAction::DrawFrame);
                }
            }
            Event::PaneExit { exit_code } => {
                // A pane's backing process exited (local shell `exit`,
                // remote SSH disconnect, `tepegoz doctor --ssh-forget`
                // followed by a TOFU rejection, etc.). Remove the
                // entry; if the stack empties, detach with the exit
                // code so `tepegoz connect <alias>`'s lone pane dying
                // returns the user to their outer shell. On a
                // multi-pane TUI, just drop the tab and toast —
                // neighboring panes keep running.
                let label = self.pane_stack[idx].label.clone();
                let was_active = idx == self.active_pane;
                self.pane_stack.remove(idx);
                if self.pane_stack.is_empty() {
                    actions.push(AppAction::Detach(DetachReason::PaneExited { exit_code }));
                    return;
                }
                if was_active || idx < self.active_pane {
                    // Shift active down if we removed something at
                    // or before it; clamp past-the-end.
                    if idx <= self.active_pane && self.active_pane > 0 {
                        self.active_pane -= 1;
                    }
                    if self.active_pane >= self.pane_stack.len() {
                        self.active_pane = self.pane_stack.len() - 1;
                    }
                }
                let detail = match exit_code {
                    Some(code) => format!("pane {label} exited (code {code})"),
                    None => format!("pane {label} exited"),
                };
                self.push_toast(ToastKind::Info, detail, actions);
                actions.push(AppAction::DrawFrame);
            }
            Event::PaneLagged { .. } => {
                // Visual lag indicator is future work; runtime logs
                // warn on the transport side.
            }
            _ => {}
        }
    }

    /// Correlate a `PaneOpened` response to the oldest in-flight
    /// `OpenPane` request (FIFO — daemon processes commands serially).
    /// Push a fresh entry onto the pane stack, make it active, focus
    /// the PTY tile, and emit the follow-up `AttachPane` + `ResizePane`
    /// needed to start the byte stream sized to the content area.
    fn handle_pane_opened(&mut self, info: PaneInfo, actions: &mut Vec<AppAction>) {
        let Some(pending) = self.pending_opens.pop_front() else {
            // PaneOpened with no in-flight request — only reachable if
            // another client on the same session opened a pane
            // concurrently. Drop silently; the user can open their
            // own panes via Ctrl-b Enter.
            return;
        };
        let (pty_rows, pty_cols) = pty_content_dims(&self.view.layout);
        let parser = Parser::new(pty_rows, pty_cols, VT100_SCROLLBACK_ROWS);
        let entry = PaneEntry {
            pane_id: info.id,
            sub_id: pending.sub_id,
            label: pane_label_from_shell(&info.shell),
            parser,
        };
        self.pane_stack.push(entry);
        self.active_pane = self.pane_stack.len() - 1;
        // Move focus back to the PTY tile — the user just opened a
        // pane from the Fleet tile and almost certainly wants to type
        // into the remote shell, not keep navigating the Fleet list.
        if self.view.focused != TileId::Pty && self.view.layout.tile(TileId::Pty).is_some() {
            self.view.focused = TileId::Pty;
            actions.push(AppAction::FocusTile(TileId::Pty));
        }
        actions.push(AppAction::SendEnvelope(envelope(Payload::AttachPane {
            pane_id: info.id,
            subscription_id: pending.sub_id,
        })));
        actions.push(AppAction::SendEnvelope(envelope(Payload::ResizePane {
            pane_id: info.id,
            rows: pty_rows,
            cols: pty_cols,
        })));
        actions.push(AppAction::DrawFrame);
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
                // Main Docker subscription — the daemon doesn't emit
                // DockerStreamEnded on the list sub, but if some
                // future version does, drop silently here. The
                // per-container logs/stats streams route to
                // handle_logs_event via is_current_logs_sub instead.
            }
            _ => {}
        }
    }

    fn handle_ports_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::PortList { ports, source } => {
                let old_key = self.ports.ports.selected_key();
                self.ports.ports.state = PortsViewState::Available {
                    rows: ports,
                    source,
                };
                self.ports.ports.reanchor_selection(old_key);
                actions.push(AppAction::DrawFrame);
            }
            Event::PortsUnavailable { reason } => {
                self.ports.ports.state = PortsViewState::Unavailable { reason };
                self.ports.ports.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            // Other event kinds are never delivered on the Ports sub id.
            _ => {}
        }
    }

    fn handle_processes_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::ProcessList { rows, source } => {
                let old_key = self.ports.processes.selected_key();
                self.ports.processes.state = ProcessesViewState::Available { rows, source };
                self.ports.processes.reanchor_selection(old_key);
                actions.push(AppAction::DrawFrame);
            }
            Event::ProcessesUnavailable { reason } => {
                self.ports.processes.state = ProcessesViewState::Unavailable { reason };
                self.ports.processes.selection = 0;
                actions.push(AppAction::DrawFrame);
            }
            _ => {}
        }
    }

    fn handle_fleet_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        match event {
            Event::HostList { hosts, source } => {
                let old_key = self.fleet.selected_key();
                // Seed per-alias state map from whatever was in the
                // prior Available state (if any) so a refreshed host
                // list doesn't blink connection markers back to
                // Disconnected. Missing aliases default to Disconnected
                // — they'll be corrected by a follow-up HostStateChanged.
                let mut states = HashMap::new();
                if let FleetScopeState::Available {
                    states: prev_states,
                    ..
                } = &self.fleet.state
                {
                    for h in &hosts {
                        if let Some(s) = prev_states.get(&h.alias) {
                            states.insert(h.alias.clone(), *s);
                        } else {
                            states.insert(h.alias.clone(), HostState::Disconnected);
                        }
                    }
                } else {
                    for h in &hosts {
                        states.insert(h.alias.clone(), HostState::Disconnected);
                    }
                }
                self.fleet.state = FleetScopeState::Available {
                    hosts,
                    states,
                    source,
                };
                self.fleet.reanchor_selection(old_key);
                actions.push(AppAction::DrawFrame);
            }
            Event::HostStateChanged {
                alias,
                state,
                reason,
            } => {
                if let FleetScopeState::Available { states, .. } = &mut self.fleet.state {
                    // Gate red-toast on the *transition* into a terminal
                    // state — duplicate HostStateChanged emits (e.g.
                    // two ProxyJump Reconnect attempts both landing on
                    // AuthFailed) must not re-toast the same outcome.
                    // State-only gating is honest v1 behavior; reason-
                    // diff retoasting can land as v1.1 polish if real-
                    // world usage surfaces the need.
                    let prev_state = states.get(&alias).copied();
                    let should_toast =
                        state.is_terminal() && reason.is_some() && (prev_state != Some(state));
                    states.insert(alias.clone(), state);
                    actions.push(AppAction::DrawFrame);
                    if should_toast {
                        let text = match state {
                            HostState::AuthFailed => format!(
                                "{alias}: auth failed — {}",
                                reason.as_deref().unwrap_or("<no reason>")
                            ),
                            HostState::HostKeyMismatch => format!(
                                "{alias}: host key rejected — {}",
                                reason.as_deref().unwrap_or("<no reason>")
                            ),
                            HostState::AgentNotDeployed => format!(
                                "{alias}: agent not deployed — {}",
                                reason.as_deref().unwrap_or("<no reason>")
                            ),
                            HostState::AgentVersionMismatch => format!(
                                "{alias}: agent version mismatch — {}",
                                reason.as_deref().unwrap_or("<no reason>")
                            ),
                            _ => return,
                        };
                        self.push_toast(ToastKind::Error, text, actions);
                    }
                }
                // Arriving before HostList — ignore; the supervisor
                // will re-emit once the tile transitions to Available.
            }
            Event::AgentCapabilities {
                alias,
                capabilities,
            } => {
                // Phase 6 Slice 6d-i: track per-alias agent
                // capabilities. Empty vec on agent disconnect — we
                // still insert (rather than remove) so a row that
                // briefly had a capability and lost it greys with
                // `(no <cap>)` instead of disappearing into a
                // missing-entry default. The map size is bounded by
                // the Fleet host count, so insertion-only is safe
                // memory-wise.
                self.host_capabilities.insert(alias, capabilities);
                actions.push(AppAction::DrawFrame);
            }
            _ => {}
        }
    }

    /// Route events arriving on the current `LogsView.sub_id`.
    fn handle_logs_event(&mut self, event: Event, actions: &mut Vec<AppAction>) {
        let DockerView::Logs(logs) = &mut self.docker.view else {
            return;
        };
        match event {
            Event::ContainerLog { stream, data } => {
                logs.ingest(stream, &data);
                actions.push(AppAction::DrawFrame);
            }
            Event::DockerStreamEnded { reason } => {
                logs.end_stream(reason);
                actions.push(AppAction::DrawFrame);
            }
            // ContainerList / DockerUnavailable / PaneSnapshot /
            // PaneOutput / PaneExit / PaneLagged / Status /
            // ContainerStats are never delivered on a DockerLogs sub
            // id — they go to their own subs. Drop silently if the
            // daemon ever misroutes.
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

        let (pty_rows, pty_cols) = pty_content_dims(&self.view.layout);
        for entry in &mut self.pane_stack {
            entry.parser.screen_mut().set_size(pty_rows, pty_cols);
            actions.push(AppAction::SendEnvelope(envelope(Payload::ResizePane {
                pane_id: entry.pane_id,
                rows: pty_rows,
                cols: pty_cols,
            })));
        }
        actions.push(AppAction::DrawFrame);
    }

    /// Allocate a fresh id for either a subscription or a DockerAction
    /// request. Monotonic; never reused.
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

/// The pty tile's Rect hosts a 1-row tab strip (chrome) plus the vt100
/// render area (content). The daemon-side pane and the client-side
/// `vt100::Parser` both need the content-area size so vim/less/top
/// render inside the box, not one row taller than it. This helper
/// carves the strip off `pty_tile_dims`; below the chrome minimum the
/// dims clamp to 1 row rather than zero (`vt100::Parser::new` panics
/// on zero).
pub(crate) fn pty_content_dims(layout: &TileLayout) -> (u16, u16) {
    let (tile_rows, tile_cols) = pty_tile_dims(layout);
    (tile_rows.saturating_sub(1).max(1), tile_cols)
}

fn envelope(payload: Payload) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload,
    }
}

/// Human display name for a container: first `/name` entry with the
/// leading slash stripped; short id prefix if the container had no
/// names. Used in toasts and the confirm prompt.
pub(crate) fn display_name_for(c: &DockerContainer) -> String {
    if let Some(raw) = c.names.first() {
        let trimmed = raw.trim_start_matches('/').to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let short = c.id.get(..12).unwrap_or(&c.id);
    short.to_string()
}

/// Past-tense-neutral verb form used in toast descriptions. Matches
/// `DockerActionKind` 1:1 so Slice D can reuse this when it adds Exec.
pub(crate) fn action_verb(kind: DockerActionKind) -> &'static str {
    match kind {
        DockerActionKind::Start => "Start",
        DockerActionKind::Stop => "Stop",
        DockerActionKind::Restart => "Restart",
        DockerActionKind::Kill => "Kill",
        DockerActionKind::Remove => "Remove",
    }
}

/// Human-readable verb for a `FleetActionKind`. Used only for toast
/// fallback when a `FleetActionResult` arrives with no matching
/// `PendingAction` (deadline already fired or stale result).
pub(crate) fn fleet_action_verb(kind: FleetActionKind) -> &'static str {
    match kind {
        FleetActionKind::Reconnect => "Reconnect",
        FleetActionKind::Disconnect => "Disconnect",
    }
}

/// Short pane-tab label from a shell string. Remote panes arrive as
/// `"ssh:<alias>"` from the daemon's `remote_pane` module and are
/// returned verbatim; local panes arrive as an absolute shell path
/// (`"/bin/zsh"`) and are trimmed to the final component. A 20-char
/// cap keeps the tab strip legible when aliases are long.
pub(crate) fn pane_label_from_shell(shell: &str) -> String {
    const MAX_LABEL_CHARS: usize = 20;
    let raw = if shell.starts_with("ssh:") || shell.is_empty() {
        shell.to_string()
    } else {
        let path = std::path::Path::new(shell);
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(shell)
            .to_string()
    };
    let trimmed = if raw.is_empty() {
        "pane".to_string()
    } else {
        raw
    };
    // Char-count, not byte-count — prevents a UTF-8 boundary panic if
    // a future user surfaces a non-ASCII alias.
    if trimmed.chars().count() > MAX_LABEL_CHARS {
        let prefix: String = trimmed.chars().take(MAX_LABEL_CHARS).collect();
        format!("{prefix}…")
    } else {
        trimmed
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
        App::new(7, "/bin/zsh".to_string(), (40, 120))
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
            7,
            "initial actions: AttachPane + ResizePane + Subscribe(Docker) + \
             Subscribe(Ports) + Subscribe(Processes) + Subscribe(Fleet) + DrawFrame"
        );

        // AttachPane with pane_sub.
        match &actions[0] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::AttachPane {
                    pane_id,
                    subscription_id,
                } => {
                    assert_eq!(*pane_id, 7);
                    assert_eq!(*subscription_id, app.active_pane_sub());
                }
                other => panic!("expected AttachPane, got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        // ResizePane with the pty tile's content rows (excluding the
        // tab strip) and full width — NOT the terminal dims. The
        // daemon-side pane is sized to the vt100 render area so vim et
        // al. render inside the box, not one row taller than it.
        let (expected_pty_rows, expected_pty_cols) = pty_content_dims(&app.view.layout);
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
                Payload::Subscribe(Subscription::Docker { id, .. }) => {
                    assert_eq!(*id, app.docker.sub_id);
                }
                other => panic!("expected Subscribe(Docker), got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        // Subscribe(Ports) with the ports sub_id.
        match &actions[3] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::Subscribe(Subscription::Ports { id, .. }) => {
                    assert_eq!(*id, app.ports.ports_sub_id);
                }
                other => panic!("expected Subscribe(Ports), got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        // Subscribe(Processes) with the processes sub_id.
        match &actions[4] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::Subscribe(Subscription::Processes { id, .. }) => {
                    assert_eq!(*id, app.ports.processes_sub_id);
                }
                other => panic!("expected Subscribe(Processes), got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        // Subscribe(Fleet) with the fleet sub_id (Phase 5 Slice 5b).
        match &actions[5] {
            AppAction::SendEnvelope(env) => match &env.payload {
                Payload::Subscribe(Subscription::Fleet { id }) => {
                    assert_eq!(*id, app.fleet.sub_id);
                }
                other => panic!("expected Subscribe(Fleet), got {other:?}"),
            },
            other => panic!("expected SendEnvelope, got {other:?}"),
        }

        assert!(matches!(actions[6], AppAction::DrawFrame));

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
    fn ctrl_b_question_toggles_help_overlay() {
        // Slice 6.0: Ctrl-b ? now toggles the help overlay state and
        // requests a redraw. Esc dismisses (covered by the help-
        // overlay Esc test below).
        let mut app = test_app();
        assert!(!app.help_visible);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(app.help_visible);
        assert!(
            actions.iter().any(|a| matches!(a, AppAction::DrawFrame)),
            "toggling the help overlay must trigger a redraw"
        );
        // Second press closes it.
        let _ = app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(!app.help_visible);
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
                target: tepegoz_proto::ScopeTarget::Local,
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
    fn docker_action_result_success_toasts_with_description() {
        // C3a: a Success result whose request_id matches a pending
        // action dequeues that pending action and emits a Success
        // toast whose message includes the action verb + container
        // name ("Restart nginx — succeeded").
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("nginx", "nginx", "running")]);
        // Press `r` to dispatch a Restart against the selected row.
        app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        let (request_id, _) = app
            .pending_actions
            .iter()
            .next()
            .map(|(&k, v)| (k, v.description.clone()))
            .expect("dispatch should have inserted a pending action");

        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerActionResult(DockerActionResult {
                request_id,
                container_id: "id-nginx".into(),
                kind: DockerActionKind::Restart,
                outcome: DockerActionOutcome::Success,
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));

        assert!(
            app.pending_actions.is_empty(),
            "matched result must clear the pending action"
        );
        let success = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast { kind: ToastKind::Success, message }
                    if message.contains("Restart") && message.contains("nginx") && message.contains("succeeded")
            )
        });
        assert_eq!(success, 1, "expected one Success toast; got {actions:?}");
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts.back().unwrap().kind, ToastKind::Success);
    }

    #[test]
    fn pane_output_feeds_vt100_parser_and_emits_drawframe() {
        let mut app = test_app();
        let sub_id = app.active_pane_sub();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
                event: Event::PaneOutput {
                    data: b"hello".to_vec(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        // vt100 received the bytes: first cell should now be 'h'.
        let cell = app.pane_stack[app.active_pane]
            .parser
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
        let sub_id = app.active_pane_sub();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
                event: Event::PaneSnapshot {
                    scrollback: b"replayed".to_vec(),
                    rows: 24,
                    cols: 80,
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        let cell = app.pane_stack[app.active_pane]
            .parser
            .screen()
            .cell(0, 0)
            .unwrap();
        assert_eq!(cell.contents(), "r");
        assert_eq!(count(&actions, |a| matches!(a, AppAction::DrawFrame)), 1);
    }

    #[test]
    fn pane_exit_event_emits_pane_exited_detach() {
        let mut app = test_app();
        let sub_id = app.active_pane_sub();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
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

        // ResizePane carries the NEW pty content dims (tile rows minus
        // the 1-row tab strip), not the terminal dims.
        let (expected_rows, expected_cols) = pty_content_dims(&app.view.layout);
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

    // ────────────────────── C3a — actions + toasts ──────────────────────

    /// Extract the single DockerAction envelope from a vec of actions,
    /// or fail the test. Used in multiple C3a tests.
    fn find_docker_action(actions: &[AppAction]) -> &DockerActionRequest {
        actions
            .iter()
            .find_map(|a| match a {
                AppAction::SendEnvelope(env) => match &env.payload {
                    Payload::DockerAction(req) => Some(req),
                    _ => None,
                },
                _ => None,
            })
            .expect("no DockerAction envelope in actions")
    }

    #[test]
    fn r_dispatches_restart_immediately_when_docker_focused() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        let actions = app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        let req = find_docker_action(&actions);
        assert_eq!(req.kind, DockerActionKind::Restart);
        assert_eq!(req.container_id, "id-web");
        assert!(
            app.pending_actions.contains_key(&req.request_id),
            "pending action must be recorded"
        );
        assert!(
            app.docker.pending_confirm.is_none(),
            "r is non-destructive — no confirm"
        );
    }

    #[test]
    fn s_dispatches_stop_immediately_when_docker_focused() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("db", "postgres", "running")]);
        let actions = app.handle_event(AppEvent::StdinChunk(b"s".to_vec()));
        let req = find_docker_action(&actions);
        assert_eq!(req.kind, DockerActionKind::Stop);
        assert_eq!(req.container_id, "id-db");
    }

    #[test]
    fn capital_k_enters_pending_confirm_kill_without_dispatching() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        let actions = app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        // No DockerAction emitted yet.
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(&env.payload, Payload::DockerAction(_))
            )),
            0,
            "K must enter confirm, not dispatch immediately"
        );
        let pc = app
            .docker
            .pending_confirm
            .as_ref()
            .expect("K must set pending_confirm");
        assert_eq!(pc.kind, DockerActionKind::Kill);
        assert_eq!(pc.container_id, "id-web");
        assert_eq!(pc.container_name, "web");
        assert!(
            pc.deadline > Instant::now(),
            "pending_confirm has a future deadline"
        );
    }

    #[test]
    fn capital_x_enters_pending_confirm_remove_without_dispatching() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"X".to_vec()));
        let pc = app.docker.pending_confirm.as_ref().unwrap();
        assert_eq!(pc.kind, DockerActionKind::Remove);
    }

    #[test]
    fn y_during_confirm_dispatches_docker_action_and_clears_pending() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        assert!(app.docker.pending_confirm.is_some());
        let actions = app.handle_event(AppEvent::StdinChunk(b"y".to_vec()));
        assert!(
            app.docker.pending_confirm.is_none(),
            "y must clear pending_confirm"
        );
        let req = find_docker_action(&actions);
        assert_eq!(req.kind, DockerActionKind::Kill);
        assert_eq!(req.container_id, "id-web");
        assert!(app.pending_actions.contains_key(&req.request_id));
    }

    #[test]
    fn n_during_confirm_cancels_without_dispatching() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        let actions = app.handle_event(AppEvent::StdinChunk(b"n".to_vec()));
        assert!(app.docker.pending_confirm.is_none());
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(&env.payload, Payload::DockerAction(_))
            )),
            0
        );
        assert!(app.pending_actions.is_empty());
    }

    #[test]
    fn esc_during_confirm_cancels() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        // Bare ESC byte (0x1b) on its own — the scope key parser emits
        // ScopeKey::Escape for a lone ESC at end of chunk.
        app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec()));
        assert!(app.docker.pending_confirm.is_none());
    }

    #[test]
    fn random_char_during_confirm_cancels() {
        // Any non-y key cancels. Pick 'z' arbitrarily.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"z".to_vec()));
        assert!(app.docker.pending_confirm.is_none());
    }

    #[test]
    fn focus_away_from_docker_cancels_pending_confirm() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        assert!(app.docker.pending_confirm.is_some());
        // Ctrl-b k → focus up (PTY).
        app.handle_event(AppEvent::StdinChunk(b"\x02k".to_vec()));
        assert_eq!(app.view.focused, TileId::Pty);
        assert!(
            app.docker.pending_confirm.is_none(),
            "leaving Docker must cancel the confirm"
        );
    }

    #[test]
    fn pending_confirm_10s_timeout_clears_state_without_dispatching() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        assert!(app.docker.pending_confirm.is_some());
        // Simulate the sweep 11 s in the future — past the 10 s
        // pending-confirm deadline.
        let mut actions: Vec<AppAction> = Vec::new();
        app.sweep_expired(Instant::now() + Duration::from_secs(11), &mut actions);
        assert!(
            app.docker.pending_confirm.is_none(),
            "sweep past deadline must drop pending_confirm"
        );
        // Silent auto-cancel — no DockerAction may leak.
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(&env.payload, Payload::DockerAction(_))
            )),
            0,
            "10 s auto-cancel must never dispatch the pending action"
        );
        assert!(
            app.pending_actions.is_empty(),
            "no pending_action should have been recorded by the auto-cancel path"
        );
    }

    #[test]
    fn second_k_while_kill_pending_is_absorbed_not_switched_or_cancelled() {
        // K → Kill modal. Another K while modal is open must absorb
        // (modal stays showing Kill with the same container). The
        // next `y` then confirms Kill as expected.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        let first_deadline = app.docker.pending_confirm.as_ref().unwrap().deadline;
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        let pc = app
            .docker
            .pending_confirm
            .as_ref()
            .expect("modal must stay open after repeat K");
        assert_eq!(
            pc.kind,
            DockerActionKind::Kill,
            "absorbed K must not switch the modal's target"
        );
        assert_eq!(
            pc.container_id, "id-web",
            "absorbed K must not switch container"
        );
        assert_eq!(
            pc.deadline, first_deadline,
            "absorbed K must not refresh the 10 s deadline"
        );
        let actions = app.handle_event(AppEvent::StdinChunk(b"y".to_vec()));
        let req = find_docker_action(&actions);
        assert_eq!(
            req.kind,
            DockerActionKind::Kill,
            "y after absorbed K must confirm Kill"
        );
    }

    #[test]
    fn x_while_kill_pending_is_absorbed_not_switched() {
        // K (Kill pending) → X must not switch the modal to Remove.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"X".to_vec()));
        let pc = app
            .docker
            .pending_confirm
            .as_ref()
            .expect("modal must stay open after X during Kill confirm");
        assert_eq!(pc.kind, DockerActionKind::Kill);
    }

    #[test]
    fn capital_r_is_noop_when_docker_focused_after_case_discipline_lock() {
        // Rule: capitals are reserved for destructive actions
        // (K / X). Lowercase is safe (r / s). Capital R must not
        // silently dispatch Restart — it falls through the match as
        // an unknown key and is dropped.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        let actions = app.handle_event(AppEvent::StdinChunk(b"R".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(&env.payload, Payload::DockerAction(_))
            )),
            0,
            "capital R must not dispatch — reserved pattern is 'caps = destructive'"
        );
        assert!(app.pending_actions.is_empty());
        assert!(
            app.docker.pending_confirm.is_none(),
            "capital R must not enter a confirm either"
        );
    }

    #[test]
    fn r_with_pty_focused_sends_input_not_docker_action() {
        // When PTY is focused, r is just a character the user typed
        // into their shell.
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(
                    &env.payload,
                    Payload::SendInput { data, .. } if data == b"r"
                )
            )),
            1
        );
        assert!(app.pending_actions.is_empty());
    }

    #[test]
    fn r_noop_when_docker_unavailable() {
        let mut app = test_app();
        // Put Docker into Unavailable.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.docker.sub_id,
                event: Event::DockerUnavailable {
                    reason: "no engine".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        let actions = app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(&env.payload, Payload::DockerAction(_))
            )),
            0
        );
        assert!(app.pending_actions.is_empty());
    }

    #[test]
    fn r_noop_when_container_list_empty() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, Vec::new());
        let actions = app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(&env.payload, Payload::DockerAction(_))
            )),
            0
        );
        assert!(app.pending_actions.is_empty());
        assert!(app.docker.pending_confirm.is_none());
    }

    #[test]
    fn docker_action_result_failure_toasts_with_description_and_reason() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        let request_id = *app.pending_actions.keys().next().unwrap();

        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerActionResult(DockerActionResult {
                request_id,
                container_id: "id-web".into(),
                kind: DockerActionKind::Restart,
                outcome: DockerActionOutcome::Failure {
                    reason: "container not running".into(),
                },
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));

        assert!(app.pending_actions.is_empty());
        let err = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast { kind: ToastKind::Error, message }
                    if message.contains("Restart")
                       && message.contains("web")
                       && message.contains("failed")
                       && message.contains("container not running")
            )
        });
        assert_eq!(err, 1, "expected one Error toast; got {actions:?}");
    }

    #[test]
    fn docker_action_result_without_pending_uses_fallback_description() {
        // Stale / unexpected result — no pending_action entry. We still
        // surface the outcome so the user isn't in the dark, but the
        // description falls back to the container id.
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerActionResult(DockerActionResult {
                request_id: 9_999,
                container_id: "ghost-id".into(),
                kind: DockerActionKind::Kill,
                outcome: DockerActionOutcome::Failure {
                    reason: "not found".into(),
                },
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        let err = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast { kind: ToastKind::Error, message }
                    if message.contains("Kill") && message.contains("ghost-id") && message.contains("not found")
            )
        });
        assert_eq!(err, 1);
    }

    #[test]
    fn pending_action_30s_timeout_emits_error_toast() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        assert_eq!(app.pending_actions.len(), 1);
        let mut actions = Vec::new();
        app.sweep_expired(Instant::now() + Duration::from_secs(31), &mut actions);
        assert!(
            app.pending_actions.is_empty(),
            "sweep past 30 s must drop the pending action"
        );
        let timeout_toast = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast { kind: ToastKind::Error, message }
                    if message.contains("timed out") && message.contains("Restart")
            )
        });
        assert_eq!(timeout_toast, 1);
        assert_eq!(
            app.toasts.back().map(|t| t.kind),
            Some(ToastKind::Error),
            "timeout toast must land in the overlay queue"
        );
    }

    #[test]
    fn pending_action_timeout_event_expires_that_action() {
        // The runtime may synthesize PendingActionTimeout(id) for a
        // single action (e.g. if it ever adds a dedicated sweeper). We
        // support that wire by treating it as equivalent to the Tick
        // sweep for that id.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"r".to_vec()));
        let request_id = *app.pending_actions.keys().next().unwrap();
        let actions = app.handle_event(AppEvent::PendingActionTimeout(request_id));
        assert!(app.pending_actions.is_empty());
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::ShowToast {
                    kind: ToastKind::Error,
                    ..
                }
            )),
            1
        );
    }

    #[test]
    fn pending_action_timeout_event_for_unknown_id_is_noop() {
        let mut app = test_app();
        let actions = app.handle_event(AppEvent::PendingActionTimeout(777));
        assert!(actions.is_empty(), "unknown id must silently no-op");
    }

    #[test]
    fn fourth_toast_drops_oldest_silently() {
        let mut app = test_app();
        let mut actions = Vec::new();
        for i in 0..4 {
            app.push_toast(ToastKind::Success, format!("msg-{i}"), &mut actions);
        }
        assert_eq!(app.toasts.len(), MAX_TOASTS);
        // Newest three remain (msg-1, msg-2, msg-3); oldest (msg-0) dropped.
        let messages: Vec<String> = app.toasts.iter().map(|t| t.message.clone()).collect();
        assert_eq!(messages, vec!["msg-1", "msg-2", "msg-3"]);
    }

    #[test]
    fn toast_sweep_drops_expired_toasts() {
        let mut app = test_app();
        let mut actions = Vec::new();
        app.push_toast(ToastKind::Success, "ok".into(), &mut actions);
        app.push_toast(ToastKind::Error, "err".into(), &mut actions);
        assert_eq!(app.toasts.len(), 2);

        // Success is 3 s; Error is 8 s. Sweep at 4 s: Success gone,
        // Error still present.
        let mut a = Vec::new();
        app.sweep_expired(Instant::now() + Duration::from_secs(4), &mut a);
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts.back().unwrap().kind, ToastKind::Error);

        // Sweep at 9 s: Error also gone.
        let mut a = Vec::new();
        app.sweep_expired(Instant::now() + Duration::from_secs(9), &mut a);
        assert!(app.toasts.is_empty());
    }

    #[test]
    fn daemon_error_also_lands_in_app_toasts() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Error(tepegoz_proto::ErrorInfo {
                kind: tepegoz_proto::ErrorKind::Internal,
                message: "disk full".into(),
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts.back().unwrap().kind, ToastKind::Error);
        assert!(app.toasts.back().unwrap().message.contains("disk full"));
        let show = count(&actions, |a| {
            matches!(
                a,
                AppAction::ShowToast { kind: ToastKind::Error, message }
                    if message.contains("disk full")
            )
        });
        assert_eq!(show, 1);
    }

    // ───────────────────── C3b — logs sub-state ─────────────────────

    /// Inject a `ContainerLog` on the currently-active logs sub id.
    /// Panics if no logs view is active — call after
    /// `try_enter_logs_view` succeeded.
    fn inject_container_log(app: &mut App, stream: LogStream, data: &[u8]) {
        let sub_id = match &app.docker.view {
            DockerView::Logs(l) => l.sub_id,
            _ => panic!("inject_container_log: no logs view active"),
        };
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
                event: Event::ContainerLog {
                    stream,
                    data: data.to_vec(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
    }

    fn inject_stream_ended(app: &mut App, reason: &str) {
        let sub_id = match &app.docker.view {
            DockerView::Logs(l) => l.sub_id,
            _ => panic!("inject_stream_ended: no logs view active"),
        };
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_id,
                event: Event::DockerStreamEnded {
                    reason: reason.into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
    }

    #[test]
    fn l_with_selected_container_enters_logs_view_and_subscribes() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        let actions = app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        let (sub_id, container_id_envelope) = actions
            .iter()
            .find_map(|a| match a {
                AppAction::SendEnvelope(env) => match &env.payload {
                    Payload::Subscribe(Subscription::DockerLogs {
                        id,
                        container_id,
                        follow,
                        tail_lines,
                        target: _,
                    }) => {
                        assert!(*follow, "logs must follow on entry");
                        assert_eq!(
                            *tail_lines, 0,
                            "C3b enters with full history (tail_lines=0)"
                        );
                        Some((*id, container_id.clone()))
                    }
                    _ => None,
                },
                _ => None,
            })
            .expect("l must send Subscribe(DockerLogs)");
        assert_eq!(container_id_envelope, "id-web");
        match &app.docker.view {
            DockerView::Logs(logs) => {
                assert_eq!(logs.sub_id, sub_id, "stored sub id must match envelope");
                assert_eq!(logs.container_id, "id-web");
                assert_eq!(logs.container_name, "web");
                assert!(logs.at_tail, "logs view opens tailing");
                assert_eq!(logs.scroll_offset, 0);
                assert!(logs.lines.is_empty());
                assert!(logs.stream_ended.is_none());
            }
            DockerView::List => panic!("l must transition to Logs view"),
        }
    }

    #[test]
    fn l_is_noop_when_no_container_selected() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, Vec::new());
        let actions = app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(
                    &env.payload,
                    Payload::Subscribe(Subscription::DockerLogs { .. })
                )
            )),
            0,
            "l must not subscribe when nothing is selected"
        );
        assert!(matches!(app.docker.view, DockerView::List));
    }

    #[test]
    fn l_is_noop_when_docker_unavailable() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.docker.sub_id,
                event: Event::DockerUnavailable {
                    reason: "no engine".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        let actions = app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(
                    &env.payload,
                    Payload::Subscribe(Subscription::DockerLogs { .. })
                )
            )),
            0
        );
        assert!(matches!(app.docker.view, DockerView::List));
    }

    #[test]
    fn esc_in_logs_view_unsubscribes_and_returns_to_list() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        let logs_sub_id = match &app.docker.view {
            DockerView::Logs(l) => l.sub_id,
            _ => unreachable!(),
        };
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec()));
        assert!(matches!(app.docker.view, DockerView::List));
        let unsub = count(&actions, |a| {
            matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(
                    &env.payload,
                    Payload::Unsubscribe { id } if *id == logs_sub_id
                )
            )
        });
        assert_eq!(unsub, 1, "Esc must Unsubscribe the logs sub");
    }

    #[test]
    fn q_in_logs_view_also_unsubscribes_and_returns_to_list() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        let logs_sub_id = match &app.docker.view {
            DockerView::Logs(l) => l.sub_id,
            _ => unreachable!(),
        };
        let actions = app.handle_event(AppEvent::StdinChunk(b"q".to_vec()));
        assert!(matches!(app.docker.view, DockerView::List));
        assert_eq!(
            count(&actions, |a| matches!(
                a,
                AppAction::SendEnvelope(env) if matches!(
                    &env.payload,
                    Payload::Unsubscribe { id } if *id == logs_sub_id
                )
            )),
            1
        );
    }

    #[test]
    fn container_log_chunks_assemble_into_lines_at_newlines() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));

        inject_container_log(&mut app, LogStream::Stdout, b"fo");
        inject_container_log(&mut app, LogStream::Stdout, b"o\nbar");
        // After this we expect one line "foo" and a pending "bar".
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert_eq!(logs.lines.len(), 1);
            assert_eq!(logs.lines[0].text, "foo");
            assert_eq!(logs.lines[0].stream, LogStream::Stdout);
        }

        inject_container_log(&mut app, LogStream::Stdout, b"\nbaz\n");
        let DockerView::Logs(logs) = &app.docker.view else {
            panic!();
        };
        let texts: Vec<&str> = logs.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn crlf_terminated_lines_strip_both_bytes() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        inject_container_log(&mut app, LogStream::Stdout, b"hello\r\nworld\n");
        let DockerView::Logs(logs) = &app.docker.view else {
            panic!();
        };
        let texts: Vec<&str> = logs.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["hello", "world"],
            "CRLF must strip both bytes; bare LF strips just the LF"
        );
    }

    #[test]
    fn stdout_and_stderr_pending_buffers_stay_separate() {
        // A half-line on stdout must not be corrupted by a complete
        // line arriving on stderr.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));

        inject_container_log(&mut app, LogStream::Stdout, b"foo");
        inject_container_log(&mut app, LogStream::Stderr, b"bar\n");
        // After: stderr produced one "bar" line; stdout still holds
        // pending "foo".
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert_eq!(logs.lines.len(), 1);
            assert_eq!(logs.lines[0].stream, LogStream::Stderr);
            assert_eq!(logs.lines[0].text, "bar");
        }

        inject_container_log(&mut app, LogStream::Stdout, b"baz\n");
        let DockerView::Logs(logs) = &app.docker.view else {
            panic!();
        };
        let lines: Vec<(LogStream, &str)> = logs
            .lines
            .iter()
            .map(|l| (l.stream, l.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            vec![(LogStream::Stderr, "bar"), (LogStream::Stdout, "foobaz"),],
            "cross-stream interleave must not mix bytes"
        );
    }

    #[test]
    fn j_k_pgup_pgdn_move_scroll_offset_and_toggle_at_tail() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        // Populate 30 lines so scrolling is meaningful.
        for i in 0..30 {
            inject_container_log(
                &mut app,
                LogStream::Stdout,
                format!("line-{i}\n").as_bytes(),
            );
        }
        // Fresh logs view opens at tail.
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert!(logs.at_tail);
            assert_eq!(logs.scroll_offset, 0);
        }

        // k (up) scrolls once, disables at_tail.
        app.handle_event(AppEvent::StdinChunk(b"k".to_vec()));
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert_eq!(logs.scroll_offset, 1);
            assert!(!logs.at_tail);
        }
        // PgUp adds LOGS_PAGE_LINES.
        app.handle_event(AppEvent::StdinChunk(b"\x1b[5~".to_vec()));
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert_eq!(logs.scroll_offset, 1 + LOGS_PAGE_LINES);
            assert!(!logs.at_tail);
        }
        // j (down) once.
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert_eq!(logs.scroll_offset, LOGS_PAGE_LINES);
            assert!(!logs.at_tail);
        }
        // PgDn drops all the way to 0 → at_tail flips back on.
        app.handle_event(AppEvent::StdinChunk(b"\x1b[6~".to_vec()));
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert_eq!(logs.scroll_offset, 0);
            assert!(logs.at_tail, "reaching 0 on scroll-down re-enables at_tail");
        }
    }

    #[test]
    fn capital_g_jumps_to_tail_and_resets_at_tail() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        for i in 0..20 {
            inject_container_log(&mut app, LogStream::Stdout, format!("l-{i}\n").as_bytes());
        }
        // Scroll far up.
        for _ in 0..10 {
            app.handle_event(AppEvent::StdinChunk(b"k".to_vec()));
        }
        {
            let DockerView::Logs(logs) = &app.docker.view else {
                panic!();
            };
            assert!(!logs.at_tail);
            assert_eq!(logs.scroll_offset, 10);
        }
        app.handle_event(AppEvent::StdinChunk(b"G".to_vec()));
        let DockerView::Logs(logs) = &app.docker.view else {
            panic!();
        };
        assert!(logs.at_tail);
        assert_eq!(logs.scroll_offset, 0);
    }

    #[test]
    fn docker_stream_ended_flushes_pending_sets_marker_and_disables_tail() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        inject_container_log(&mut app, LogStream::Stdout, b"final-line-without-newline");
        inject_stream_ended(&mut app, "container exited");

        let DockerView::Logs(logs) = &app.docker.view else {
            panic!();
        };
        assert_eq!(
            logs.lines.len(),
            1,
            "pending bytes must flush on stream end"
        );
        assert_eq!(logs.lines[0].text, "final-line-without-newline");
        assert_eq!(
            logs.stream_ended.as_deref(),
            Some("container exited"),
            "reason must be recorded verbatim"
        );
        assert!(!logs.at_tail, "at_tail disables on stream end");
    }

    #[test]
    fn max_log_lines_drops_oldest() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        // Push MAX_LOG_LINES + 5 lines; assert len caps and newest
        // survive.
        for i in 0..(MAX_LOG_LINES + 5) {
            inject_container_log(
                &mut app,
                LogStream::Stdout,
                format!("line-{i}\n").as_bytes(),
            );
        }
        let DockerView::Logs(logs) = &app.docker.view else {
            panic!();
        };
        assert_eq!(logs.lines.len(), MAX_LOG_LINES);
        // Oldest ("line-0" through "line-4") dropped.
        for i in 0..5 {
            assert!(
                logs.lines.iter().all(|l| l.text != format!("line-{i}")),
                "line-{i} must have been dropped"
            );
        }
        // Newest still present.
        assert_eq!(
            logs.lines.back().unwrap().text,
            format!("line-{}", MAX_LOG_LINES + 4)
        );
    }

    #[test]
    fn stale_logs_events_after_unsubscribe_are_dropped() {
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        let old_sub_id = match &app.docker.view {
            DockerView::Logs(l) => l.sub_id,
            _ => unreachable!(),
        };
        // Leave logs view.
        app.handle_event(AppEvent::StdinChunk(b"q".to_vec()));
        assert!(matches!(app.docker.view, DockerView::List));

        // A stale ContainerLog arrives on the now-unsubscribed id.
        // Must not panic and must not mutate DockerScope.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: old_sub_id,
                event: Event::ContainerLog {
                    stream: LogStream::Stdout,
                    data: b"ghost\n".to_vec(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert!(
            actions.is_empty(),
            "stale log chunks after Unsubscribe must drop silently"
        );
        assert!(matches!(app.docker.view, DockerView::List));
    }

    #[test]
    fn logs_view_ignores_r_s_k_x_and_l_keybinds() {
        // Read-only transcript: none of the list-view action keys
        // should do anything while logs are showing.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        for byte in [b'r', b's', b'K', b'X', b'l', b'/'] {
            let actions = app.handle_event(AppEvent::StdinChunk(vec![byte]));
            assert_eq!(
                count(&actions, |a| matches!(
                    a,
                    AppAction::SendEnvelope(env) if matches!(
                        &env.payload,
                        Payload::DockerAction(_) | Payload::Subscribe(_)
                    )
                )),
                0,
                "byte {byte:?} must be ignored in logs view"
            );
        }
        // Still in logs view, no pending confirm or action spawned.
        assert!(matches!(app.docker.view, DockerView::Logs(_)));
        assert!(app.docker.pending_confirm.is_none());
        assert!(app.pending_actions.is_empty());
    }

    #[test]
    fn focus_away_from_docker_does_not_cancel_logs_view() {
        // Unlike pending_confirm (which auto-cancels on focus-away),
        // the logs view must persist: the user can focus the pty tile
        // and come back to find the stream still live.
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("web", "nginx", "running")]);
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        assert!(matches!(app.docker.view, DockerView::Logs(_)));
        app.handle_event(AppEvent::StdinChunk(b"\x02k".to_vec())); // focus PTY
        assert_eq!(app.view.focused, TileId::Pty);
        assert!(
            matches!(app.docker.view, DockerView::Logs(_)),
            "logs view must persist across focus moves"
        );
    }

    // ---- Phase 4 Slice 4c: Ports tile (with Processes toggle) ----

    fn make_port(
        protocol: &str,
        port: u16,
        pid: u32,
        name: &str,
        container: Option<&str>,
    ) -> ProbePort {
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

    fn make_process(pid: u32, start_time: i64, command: &str) -> ProbeProcess {
        ProbeProcess {
            pid,
            parent_pid: 1,
            start_time_unix_secs: start_time,
            command: command.into(),
            cpu_percent: Some(1.5),
            mem_bytes: 4_194_304,
            partial: false,
        }
    }

    fn populate_ports_and_focus(app: &mut App, ports: Vec<ProbePort>) {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.ports_sub_id,
                event: Event::PortList {
                    ports,
                    source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        // Focus Ports tile: Ctrl-b j to Docker, then Ctrl-b l to Ports.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
    }

    fn populate_processes(app: &mut App, procs: Vec<ProbeProcess>) {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.processes_sub_id,
                event: Event::ProcessList {
                    rows: procs,
                    source: "sysinfo".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
    }

    #[test]
    fn port_list_event_transitions_ports_view_to_available_and_redraws() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.ports_sub_id,
                event: Event::PortList {
                    ports: vec![make_port("tcp", 8080, 100, "nginx", None)],
                    source: "linux-procfs".into(),
                },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert!(matches!(
            app.ports.ports.state,
            PortsViewState::Available { .. }
        ));
        assert_eq!(app.ports.ports.visible_count(), 1);
        assert!(actions.iter().any(|a| matches!(a, AppAction::DrawFrame)));
    }

    #[test]
    fn ports_unavailable_event_transitions_to_unavailable_and_clears_selection() {
        let mut app = test_app();
        populate_ports_and_focus(&mut app, vec![make_port("tcp", 8080, 100, "nginx", None)]);
        app.ports.ports.selection = 0;
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.ports_sub_id,
                event: Event::PortsUnavailable {
                    reason: "probe permission denied".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        assert!(matches!(
            app.ports.ports.state,
            PortsViewState::Unavailable { .. }
        ));
        assert_eq!(app.ports.ports.selection, 0);
    }

    #[test]
    fn process_list_event_transitions_processes_view_to_available() {
        let mut app = test_app();
        populate_processes(
            &mut app,
            vec![make_process(4242, 1_700_000_000, "nginx: worker")],
        );
        assert!(matches!(
            app.ports.processes.state,
            ProcessesViewState::Available { .. }
        ));
        assert_eq!(app.ports.processes.visible_count(), 1);
    }

    #[test]
    fn p_toggles_ports_and_processes_views_when_ports_tile_focused() {
        let mut app = test_app();
        populate_ports_and_focus(&mut app, vec![make_port("tcp", 8080, 100, "nginx", None)]);
        assert!(matches!(app.ports.active, PortsActiveView::Ports));
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec()));
        assert!(matches!(app.ports.active, PortsActiveView::Processes));
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec()));
        assert!(matches!(app.ports.active, PortsActiveView::Ports));
    }

    #[test]
    fn p_does_not_toggle_while_filter_is_active_on_ports_view() {
        let mut app = test_app();
        populate_ports_and_focus(&mut app, vec![make_port("tcp", 8080, 100, "nginx", None)]);
        app.handle_event(AppEvent::StdinChunk(b"/".to_vec())); // activate filter
        assert!(app.ports.ports.filter_active);
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec()));
        assert!(
            matches!(app.ports.active, PortsActiveView::Ports),
            "`p` while filter-typing must be a filter character, not toggle"
        );
        assert_eq!(app.ports.ports.filter, "p");
    }

    #[test]
    fn j_k_navigate_ports_view_independently_of_processes_selection() {
        let mut app = test_app();
        populate_ports_and_focus(
            &mut app,
            vec![
                make_port("tcp", 3000, 200, "web", None),
                make_port("tcp", 5432, 300, "postgres", None),
                make_port("tcp", 6379, 400, "redis", None),
            ],
        );
        populate_processes(
            &mut app,
            vec![
                make_process(200, 1_700_000_001, "web"),
                make_process(300, 1_700_000_002, "postgres"),
            ],
        );

        assert_eq!(app.ports.ports.selection, 0);
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.ports.ports.selection, 1);
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.ports.ports.selection, 2);

        // Toggle to Processes; its selection should still be at 0 (not
        // carried over from Ports' 2).
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec()));
        assert_eq!(app.ports.processes.selection, 0);

        // Move selection in Processes.
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.ports.processes.selection, 1);

        // Toggle back: Ports selection must still be 2 (not overwritten
        // by Processes' 1).
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec()));
        assert_eq!(app.ports.ports.selection, 2);
    }

    #[test]
    fn ports_selection_persists_across_refresh_by_protocol_port_pid_key() {
        let mut app = test_app();
        populate_ports_and_focus(
            &mut app,
            vec![
                make_port("tcp", 3000, 200, "web", None),
                make_port("tcp", 5432, 300, "postgres", None),
                make_port("tcp", 6379, 400, "redis", None),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // select postgres
        assert_eq!(app.ports.ports.selection, 1);

        // Refresh arrives with postgres REORDERED to index 0.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.ports_sub_id,
                event: Event::PortList {
                    ports: vec![
                        make_port("tcp", 5432, 300, "postgres", None),
                        make_port("tcp", 3000, 200, "web", None),
                        make_port("tcp", 6379, 400, "redis", None),
                    ],
                    source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));

        // Selection must follow postgres to its new index.
        assert_eq!(
            app.ports.ports.selection, 0,
            "selection must re-anchor to (protocol, port, pid) of postgres after reorder"
        );
    }

    #[test]
    fn ports_selection_moves_to_next_row_when_selected_row_disappears() {
        let mut app = test_app();
        populate_ports_and_focus(
            &mut app,
            vec![
                make_port("tcp", 3000, 200, "web", None),
                make_port("tcp", 5432, 300, "postgres", None),
                make_port("tcp", 6379, 400, "redis", None),
            ],
        );
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // select postgres
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // select redis
        assert_eq!(app.ports.ports.selection, 2);

        // postgres + redis vanish — only `web` left. Selection was
        // pointing at redis; since redis is gone, fall back to clamping
        // to the last valid index (0 = web).
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.ports_sub_id,
                event: Event::PortList {
                    ports: vec![make_port("tcp", 3000, 200, "web", None)],
                    source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            app.ports.ports.selection, 0,
            "disappeared-entity selection must clamp into the new visible range"
        );
    }

    #[test]
    fn processes_selection_persists_across_refresh_by_pid_start_time_key() {
        let mut app = test_app();
        populate_processes(
            &mut app,
            vec![
                make_process(200, 1_700_000_001, "web"),
                make_process(300, 1_700_000_002, "postgres"),
            ],
        );
        app.ports.active = PortsActiveView::Processes;
        app.view.focused = TileId::Ports;
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // select postgres
        assert_eq!(app.ports.processes.selection, 1);

        // Pid 200 gets reused by a *new* process (different
        // start_time). The selected postgres row (pid 300) is still
        // present but reordered. Selection must re-anchor to
        // (300, 1_700_000_002), not drift to the reused pid 200.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.ports.processes_sub_id,
                event: Event::ProcessList {
                    rows: vec![
                        make_process(300, 1_700_000_002, "postgres"),
                        make_process(200, 1_700_000_500, "DIFFERENT_BINARY"),
                    ],
                    source: "sysinfo".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            app.ports.processes.selection, 0,
            "selection must follow (pid, start_time) to postgres's new index"
        );
    }

    #[test]
    fn filter_active_typing_accumulates_to_ports_view_filter() {
        let mut app = test_app();
        populate_ports_and_focus(&mut app, vec![make_port("tcp", 8080, 100, "nginx", None)]);
        app.handle_event(AppEvent::StdinChunk(b"/".to_vec()));
        assert!(app.ports.ports.filter_active);
        app.handle_event(AppEvent::StdinChunk(b"ngi".to_vec()));
        assert_eq!(app.ports.ports.filter, "ngi");
        app.handle_event(AppEvent::StdinChunk(b"\x7f".to_vec())); // backspace
        assert_eq!(app.ports.ports.filter, "ng");
        // Enter commits; filter remains.
        app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        assert!(!app.ports.ports.filter_active);
        assert_eq!(app.ports.ports.filter, "ng");
    }

    #[test]
    fn esc_clears_ports_filter_and_deactivates() {
        let mut app = test_app();
        populate_ports_and_focus(&mut app, vec![make_port("tcp", 8080, 100, "nginx", None)]);
        app.handle_event(AppEvent::StdinChunk(b"/".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"xyz".to_vec()));
        assert_eq!(app.ports.ports.filter, "xyz");
        app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec())); // Esc
        assert!(!app.ports.ports.filter_active);
        assert_eq!(app.ports.ports.filter, "");
    }

    #[test]
    fn ports_filter_narrows_visible_rows_and_reanchors_selection() {
        let mut app = test_app();
        populate_ports_and_focus(
            &mut app,
            vec![
                make_port("tcp", 3000, 200, "web", None),
                make_port("tcp", 5432, 300, "postgres", None),
                make_port("tcp", 6379, 400, "redis", None),
            ],
        );
        assert_eq!(app.ports.ports.visible_count(), 3);
        app.handle_event(AppEvent::StdinChunk(b"/".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"post".to_vec()));
        assert_eq!(app.ports.ports.visible_count(), 1);
    }

    #[test]
    fn ports_focused_stdin_routes_to_ports_key_handler_not_send_input() {
        // Ports tile is now a real scope (ScopeKind::Ports), not a
        // placeholder. Focused stdin must route to handle_ports_key
        // and produce DrawFrame (from nav), not SendInput to the pty.
        let mut app = test_app();
        populate_ports_and_focus(
            &mut app,
            vec![
                make_port("tcp", 3000, 200, "web", None),
                make_port("tcp", 5432, 300, "postgres", None),
            ],
        );
        let actions = app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        let send_input_count = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    AppAction::SendEnvelope(env)
                        if matches!(&env.payload, Payload::SendInput { .. })
                )
            })
            .count();
        assert_eq!(send_input_count, 0, "j in Ports must not become SendInput");
        assert_eq!(app.ports.ports.selection, 1);
    }

    // ────────────────── Phase 5 Slice 5d-ii — pane stack ──────────────────

    /// Inject a `PaneOpened` as if the daemon replied to a client-
    /// initiated open. Registers a `PendingOpen` first so the FIFO
    /// correlation in `handle_pane_opened` consumes it (otherwise the
    /// handler drops the response as "another client's open").
    fn inject_pane_opened(app: &mut App, pane_id: PaneId, shell: &str) {
        let alias = shell.strip_prefix("ssh:").map(str::to_string);
        let sub_id = app.alloc_sub_id();
        app.pending_opens.push_back(PendingOpen { sub_id, alias });
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::PaneOpened(PaneInfo {
                id: pane_id,
                created_at_unix_millis: 0,
                rows: 20,
                cols: 80,
                shell: shell.into(),
                alive: true,
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
    }

    fn fleet_with_hosts(app: &mut App, aliases: &[&str]) {
        use std::collections::HashMap;
        let hosts: Vec<HostEntry> = aliases
            .iter()
            .map(|a| HostEntry {
                alias: (*a).into(),
                hostname: format!("{a}.example"),
                user: "alice".into(),
                port: 22,
                identity_files: vec![],
                proxy_jump: None,
            })
            .collect();
        let mut states = HashMap::new();
        for a in aliases {
            states.insert((*a).to_string(), HostState::Disconnected);
        }
        app.fleet.state = FleetScopeState::Available {
            hosts,
            states,
            source: "test".into(),
        };
    }

    #[test]
    fn root_pane_seeds_stack_with_local_label() {
        let app = test_app();
        assert_eq!(app.pane_stack.len(), 1, "starts with one root pane");
        assert_eq!(app.active_pane, 0);
        assert_eq!(
            app.pane_stack[0].label, "zsh",
            "local shell path trims to final component"
        );
    }

    #[test]
    fn tab_cycles_forward_through_fixed_tile_order() {
        // Slice 6.0.1 carve-out: Tab on PTY forwards to pty, so to
        // exercise the cycle we start from a non-PTY focus. Ctrl-b j
        // (undocumented spatial-focus alias) hops off PTY without
        // relying on Tab itself.
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        assert_eq!(app.view.focused, TileId::Docker);
        app.handle_event(AppEvent::StdinChunk(b"\t".to_vec()));
        assert_eq!(app.view.focused, TileId::Ports);
        app.handle_event(AppEvent::StdinChunk(b"\t".to_vec()));
        assert_eq!(app.view.focused, TileId::Fleet);
        app.handle_event(AppEvent::StdinChunk(b"\t".to_vec()));
        assert_eq!(app.view.focused, TileId::ClaudeCode);
        app.handle_event(AppEvent::StdinChunk(b"\t".to_vec()));
        assert_eq!(app.view.focused, TileId::Pty, "Tab wraps back to PTY");
    }

    #[test]
    fn shift_tab_cycles_backward_through_fixed_tile_order() {
        // As above — start from a non-PTY focus so Shift-Tab hits the
        // cycle branch (rather than the 6.0.1 PTY carve-out).
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        assert_eq!(app.view.focused, TileId::Docker);
        app.handle_event(AppEvent::StdinChunk(b"\x1b[Z".to_vec()));
        assert_eq!(app.view.focused, TileId::Pty);
        // Bouncing back through PTY — Shift-Tab on PTY forwards to
        // pty now, so use Ctrl-b j to return to Docker before the
        // next cycle step.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        assert_eq!(app.view.focused, TileId::Docker);
        // And continue cycling backward from Docker.
        app.handle_event(AppEvent::StdinChunk(b"\x1b[Z".to_vec()));
        assert_eq!(app.view.focused, TileId::Pty, "Docker ← Shift-Tab → Pty");
    }

    #[test]
    fn tab_on_pty_focus_forwards_tab_byte_to_pty_not_cycle() {
        // Slice 6.0.1 carve-out: shell tab-completion (or any pty app
        // that consumes Tab) must see the Tab byte while the PTY
        // tile owns focus. The tile stays focused; no cycle.
        let mut app = test_app();
        assert_eq!(app.view.focused, TileId::Pty);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\t".to_vec()));
        assert_eq!(
            app.view.focused,
            TileId::Pty,
            "Tab on PTY focus must not cycle tiles"
        );
        let sent_tab = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::SendInput { data, .. } if data == b"\t"
                    )
            )
        });
        assert!(
            sent_tab,
            "PTY-focused Tab must forward \\t as SendInput; got {actions:?}"
        );
    }

    #[test]
    fn shift_tab_on_pty_focus_forwards_csi_z_to_pty_not_cycle() {
        // Symmetric carve-out for Shift-Tab. Xterm sends `\x1b[Z` on
        // Shift-Tab; readline uses it for reverse menu-complete, vim
        // uses it for reverse tab-navigation in some plugins — we
        // hand that same byte shape to the pty so those paths work.
        let mut app = test_app();
        assert_eq!(app.view.focused, TileId::Pty);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x1b[Z".to_vec()));
        assert_eq!(
            app.view.focused,
            TileId::Pty,
            "Shift-Tab on PTY focus must not cycle tiles"
        );
        let sent_csi_z = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::SendInput { data, .. } if data == b"\x1b[Z"
                    )
            )
        });
        assert!(
            sent_csi_z,
            "PTY-focused Shift-Tab must forward \\x1b[Z as SendInput; got {actions:?}"
        );
    }

    #[test]
    fn tab_cycle_off_docker_tile_cancels_pending_confirm() {
        // Tab-cycling from Docker must preserve the C3a UX invariant
        // that focus-away cancels a pending confirm modal (same path
        // as Ctrl-b l directional focus).
        let mut app = test_app();
        populate_docker_and_focus(&mut app, vec![make_container("a", "a", "running")]);
        assert_eq!(app.view.focused, TileId::Docker);
        // Enter Kill-pending confirm.
        app.handle_event(AppEvent::StdinChunk(b"K".to_vec()));
        assert!(app.docker.pending_confirm.is_some());

        app.handle_event(AppEvent::StdinChunk(b"\t".to_vec()));
        assert_eq!(app.view.focused, TileId::Ports);
        assert!(
            app.docker.pending_confirm.is_none(),
            "Tab-cycle off Docker must cancel pending confirm"
        );
    }

    #[test]
    fn ctrl_b_ampersand_closes_active_pane_and_shifts_active() {
        let mut app = test_app();
        inject_pane_opened(&mut app, 100, "ssh:staging");
        inject_pane_opened(&mut app, 101, "ssh:dev");
        // active = 2 (ssh:dev). Close it; active should shift to 1
        // (ssh:staging).
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02&".to_vec()));
        assert_eq!(app.pane_stack.len(), 2);
        assert_eq!(app.active_pane, 1);
        // ClosePane envelope sent for the removed pane id.
        let closed = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::ClosePane { pane_id: 101 })
            )
        });
        assert!(
            closed,
            "ClosePane must carry the closed pane id; got {actions:?}"
        );
    }

    #[test]
    fn ctrl_b_ampersand_on_last_pane_dispatches_open_local() {
        let mut app = test_app();
        // Stack has one local root pane. Ctrl-b & should close it AND
        // request a fresh local root so the PTY tile never blanks.
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02&".to_vec()));
        // Stack cleared eagerly.
        assert!(app.pane_stack.is_empty());
        // OpenPane Local dispatched.
        let has_open = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::OpenPane(spec) if matches!(spec.target, PaneTarget::Local)
                    )
            )
        });
        assert!(
            has_open,
            "Ctrl-b & on last pane dispatches fresh local OpenPane; got {actions:?}"
        );
        assert_eq!(app.pending_opens.len(), 1);
    }

    #[test]
    fn ctrl_b_ampersand_is_noop_when_focus_is_not_pty() {
        let mut app = test_app();
        // Focus Docker tile so Ctrl-b & shouldn't fire the pane close.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        assert_eq!(app.view.focused, TileId::Docker);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02&".to_vec()));
        assert_eq!(app.pane_stack.len(), 1, "stack untouched from scope focus");
        let closed = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::ClosePane { .. })
            )
        });
        assert!(!closed, "no ClosePane envelope when focus is off PTY");
    }

    #[test]
    fn plain_enter_on_fleet_dispatches_open_pane_remote() {
        // Slice 6.0: plain Enter on a focused Fleet tile is the
        // "primary action on the selected row" per the amended
        // Decision #7. Pre-6.0 the keybind was Ctrl-b Enter.
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["staging", "dev"]);
        // Focus Fleet tile. Slice 6.0.1 carved Tab out of PTY focus,
        // so we use Ctrl-b j → l → l (undocumented spatial-focus
        // aliases) to hop PTY → Docker → Ports → Fleet without the
        // first Tab getting forwarded to the pty.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        assert_eq!(app.view.focused, TileId::Fleet);

        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        let open_remote_staging = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::OpenPane(spec)
                        if matches!(
                            &spec.target,
                            PaneTarget::Remote { alias }
                            if alias == "staging"
                        )
                    )
            )
        });
        assert!(
            open_remote_staging,
            "plain Enter on Fleet dispatches OpenPane Remote for selected alias; got {actions:?}"
        );
        assert_eq!(app.pending_opens.len(), 1);
        assert_eq!(
            app.pending_opens.front().unwrap().alias.as_deref(),
            Some("staging")
        );
    }

    #[test]
    fn plain_enter_on_pty_sends_send_input_not_open_pane() {
        // Slice 6.0: plain Enter on PTY flows through to the shell
        // as SendInput (`\r`) — the "primary action" unification
        // applies to scope tiles only; PTY keystrokes still forward.
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["staging"]);
        assert_eq!(app.view.focused, TileId::Pty);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        let has_open = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::OpenPane { .. })
            )
        });
        assert!(
            !has_open,
            "plain Enter on PTY must not dispatch OpenPane; got {actions:?}"
        );
        let has_send_input = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::SendInput { data, .. } if data == b"\r"
                    )
            )
        });
        assert!(
            has_send_input,
            "plain Enter on PTY forwards as SendInput \\r; got {actions:?}"
        );
    }

    #[test]
    fn pane_opened_response_pushes_entry_and_emits_attach_and_focus() {
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["staging"]);
        // Focus Fleet (via undocumented Ctrl-b spatial aliases so the
        // 6.0.1 Tab-in-PTY carve-out doesn't swallow the first step)
        // + plain-Enter dispatch OpenPane.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        let pending_sub = app.pending_opens.front().unwrap().sub_id;

        // Simulate the daemon's PaneOpened response.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::PaneOpened(PaneInfo {
                id: 42,
                created_at_unix_millis: 0,
                rows: 20,
                cols: 80,
                shell: "ssh:staging".into(),
                alive: true,
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));

        assert_eq!(app.pane_stack.len(), 2, "stack grew by one");
        assert_eq!(app.active_pane, 1);
        assert_eq!(app.pane_stack[1].pane_id, 42);
        assert_eq!(app.pane_stack[1].sub_id, pending_sub);
        assert_eq!(app.pane_stack[1].label, "ssh:staging");
        assert!(app.pending_opens.is_empty());

        // AttachPane sent for the new pane + sub.
        let attach = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::AttachPane { pane_id: 42, subscription_id }
                        if *subscription_id == pending_sub
                    )
            )
        });
        assert!(
            attach,
            "AttachPane must target the new pane id + sub; got {actions:?}"
        );

        // Focus moves back to PTY.
        assert_eq!(app.view.focused, TileId::Pty);
        let focus_pty = actions
            .iter()
            .any(|a| matches!(a, AppAction::FocusTile(TileId::Pty)));
        assert!(focus_pty, "focus moves back to PTY tile");
    }

    #[test]
    fn pane_exit_on_single_pane_detaches() {
        let mut app = test_app();
        let sub = app.active_pane_sub();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub,
                event: Event::PaneExit { exit_code: Some(0) },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        let detached = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::Detach(DetachReason::PaneExited { exit_code: Some(0) })
            )
        });
        assert!(detached, "last pane exit detaches the TUI; got {actions:?}");
    }

    #[test]
    fn pane_exit_on_non_active_pane_just_removes_it() {
        let mut app = test_app();
        inject_pane_opened(&mut app, 100, "ssh:staging");
        inject_pane_opened(&mut app, 101, "ssh:dev");
        // active = 2 (ssh:dev). PaneExit on pane 100 (index 1).
        let sub_100 = app.pane_stack[1].sub_id;
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: sub_100,
                event: Event::PaneExit { exit_code: None },
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(app.pane_stack.len(), 2);
        assert_eq!(
            app.active_pane, 1,
            "active (ssh:dev) shifts down to new index 1"
        );
        assert_eq!(app.pane_stack[1].pane_id, 101);
        // Detach NOT emitted; stack non-empty.
        let detached = actions.iter().any(|a| matches!(a, AppAction::Detach(_)));
        assert!(!detached, "non-terminal pane exit stays attached");
    }

    #[test]
    fn error_during_pending_open_attributes_failure_to_alias() {
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["staging"]);
        // Dispatch open against Fleet.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02\r".to_vec()));
        assert_eq!(app.pending_opens.len(), 1);

        // Daemon replies Error with the OpenPane failure-path prefix
        // (mirrors `tepegoz-core::client`'s
        // `error_envelope(..., "open remote pane ({alias}): {e}")`).
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Error(ErrorInfo {
                kind: ErrorKind::Internal,
                message: "open remote pane (staging): ssh dial failed: connection refused".into(),
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert!(app.pending_opens.is_empty(), "FIFO entry consumed");
        // Red toast references the alias + the reason verbatim.
        let error_toast = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::ShowToast {
                    kind: ToastKind::Error,
                    message,
                } if message.contains("staging") && message.contains("connection refused")
            )
        });
        assert!(
            error_toast,
            "Error during pending open surfaces as failure toast; got {actions:?}"
        );
    }

    #[test]
    fn unrelated_error_while_open_pending_does_not_consume_fifo() {
        // The FIFO consume must guard on the daemon's OpenPane
        // failure-path prefixes (`open pane` / `open remote pane`).
        // Without it, a SendInput-against-unknown-pane error (or any
        // other stray Error) would mis-attribute to the next pending
        // OpenPane and confuse the user with a red toast about the
        // wrong alias. Pinned so future refactors of the dispatcher
        // don't quietly regress the guard.
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["staging"]);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02\r".to_vec()));
        assert_eq!(app.pending_opens.len(), 1);

        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Error(ErrorInfo {
                kind: ErrorKind::UnknownPane,
                message: "send input: unknown pane 123".into(),
            }),
        };
        let actions = app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            app.pending_opens.len(),
            1,
            "unrelated Error must NOT consume the FIFO entry"
        );
        // Generic daemon-error toast still surfaces so the user sees it.
        let generic_toast = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::ShowToast {
                    kind: ToastKind::Error,
                    message,
                } if message.contains("daemon error") && message.contains("unknown pane")
            )
        });
        assert!(
            generic_toast,
            "Unrelated Error still toasted as generic daemon error; got {actions:?}"
        );
    }

    #[test]
    fn pane_label_from_shell_variants() {
        assert_eq!(pane_label_from_shell("/bin/zsh"), "zsh");
        assert_eq!(pane_label_from_shell("/usr/local/bin/fish"), "fish");
        assert_eq!(pane_label_from_shell("ssh:staging"), "ssh:staging");
        assert_eq!(pane_label_from_shell(""), "pane");
        // Long labels truncate with an ellipsis.
        let long = "ssh:this-is-a-really-long-alias-name-indeed";
        let trimmed = pane_label_from_shell(long);
        assert_eq!(trimmed.chars().count(), 21, "20-char prefix + one ellipsis");
        assert!(trimmed.ends_with('…'));
    }

    // ------------------------------------------------------------------
    // Slice 6.0: mouse + help-overlay state-machine tests.
    // ------------------------------------------------------------------

    /// Helper — returns the centre coordinate of the tile in the
    /// default 120×40 layout. Used to synthesize clicks without
    /// hardcoding coordinates that would break if the layout tweaks.
    fn tile_center(app: &App, id: TileId) -> (u16, u16) {
        let rect = app.view.layout.rect_of(id).expect("tile present");
        (rect.x + rect.width / 2, rect.y + rect.height / 2)
    }

    #[test]
    fn mouse_click_on_docker_tile_focuses_docker() {
        let mut app = test_app();
        assert_eq!(app.view.focused, TileId::Pty);
        let (x, y) = tile_center(&app, TileId::Docker);
        let actions = app.handle_event(AppEvent::MouseClick { x, y });
        assert_eq!(app.view.focused, TileId::Docker);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, AppAction::FocusTile(TileId::Docker))),
            "click on Docker tile emits FocusTile(Docker); got {actions:?}"
        );
    }

    #[test]
    fn mouse_click_outside_any_tile_is_ignored() {
        let mut app = test_app();
        assert_eq!(app.view.focused, TileId::Pty);
        // (9999, 9999) is well outside the 120×40 default layout.
        let actions = app.handle_event(AppEvent::MouseClick { x: 9999, y: 9999 });
        assert_eq!(app.view.focused, TileId::Pty, "focus untouched");
        assert!(
            actions.is_empty(),
            "clicks with no tile hit produce no actions; got {actions:?}"
        );
    }

    #[test]
    fn mouse_click_on_fleet_row_selects_that_row() {
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["a", "b", "c"]);
        // Fleet has no header row + no filter.
        let rect = app.view.layout.rect_of(TileId::Fleet).unwrap();
        let y = rect.y + 1 + 1 + 2; // border + status + third row
        let x = rect.x + 2;
        let _ = app.handle_event(AppEvent::MouseClick { x, y });
        assert_eq!(app.view.focused, TileId::Fleet);
        assert_eq!(app.fleet.selection, 2, "third row (idx 2) selected");
    }

    #[test]
    fn double_click_on_fleet_row_within_window_dispatches_open_pane_remote() {
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["primary", "secondary"]);
        let rect = app.view.layout.rect_of(TileId::Fleet).unwrap();
        let y = rect.y + 1 + 1; // first data row
        let x = rect.x + 2;
        let t0 = Instant::now();
        let mut actions = Vec::new();
        app.handle_mouse_click_at(x, y, t0, &mut actions);
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::OpenPane(_))
            )),
            "single click must not open a pane; got {actions:?}"
        );

        let mut actions2 = Vec::new();
        app.handle_mouse_click_at(x, y, t0 + Duration::from_millis(200), &mut actions2);
        let opened = actions2.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::OpenPane(spec)
                        if matches!(
                            &spec.target,
                            PaneTarget::Remote { alias } if alias == "primary"
                        )
                    )
            )
        });
        assert!(
            opened,
            "double-click on first Fleet row opens OpenPane Remote primary; got {actions2:?}"
        );
    }

    #[test]
    fn double_click_outside_window_is_treated_as_single_clicks() {
        let mut app = test_app();
        fleet_with_hosts(&mut app, &["a"]);
        let rect = app.view.layout.rect_of(TileId::Fleet).unwrap();
        let y = rect.y + 1 + 1;
        let x = rect.x + 2;
        let t0 = Instant::now();
        let mut actions = Vec::new();
        app.handle_mouse_click_at(x, y, t0, &mut actions);
        let mut actions2 = Vec::new();
        // 500 ms later — beyond the 400 ms window.
        app.handle_mouse_click_at(x, y, t0 + Duration::from_millis(500), &mut actions2);
        assert!(
            !actions2.iter().any(|a| matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::OpenPane(_))
            )),
            "clicks 500 ms apart must not fire primary action; got {actions2:?}"
        );
    }

    #[test]
    fn mouse_click_on_docker_header_row_selects_no_row() {
        let mut app = test_app();
        populate_docker_and_focus(
            &mut app,
            vec![
                make_container("a", "a", "running"),
                make_container("b", "b", "running"),
            ],
        );
        let rect = app.view.layout.rect_of(TileId::Docker).unwrap();
        // Header row sits at rect.y + 1 (border) + 1 (status) = rect.y + 2.
        // No filter active.
        let header_y = rect.y + 2;
        app.docker.selection = 0;
        let _ = app.handle_event(AppEvent::MouseClick {
            x: rect.x + 4,
            y: header_y,
        });
        assert_eq!(
            app.docker.selection, 0,
            "header click must not shift selection"
        );
    }

    #[test]
    fn mouse_click_on_pty_tab_strip_switches_active_pane() {
        let mut app = test_app();
        inject_pane_opened(&mut app, 100, "ssh:staging");
        inject_pane_opened(&mut app, 101, "ssh:dev");
        assert_eq!(app.active_pane, 2);
        // Tab strip row at rect.y + 1 (inside the border).
        // Tab 1 (`[1 …]`) starts at rect.x + 1, then comes tab 2, tab 3.
        // `[1 zsh]` (4 + label_len) = `[1 zsh]` label "zsh" = 7 cells.
        // Click on col rect.x + 1 hits tab 1.
        let rect = app.view.layout.rect_of(TileId::Pty).unwrap();
        let _ = app.handle_event(AppEvent::MouseClick {
            x: rect.x + 1,
            y: rect.y + 1,
        });
        assert_eq!(
            app.active_pane, 0,
            "click on first tab switches active pane to 0"
        );
    }

    #[test]
    fn mouse_click_on_pty_close_affordance_closes_active_pane() {
        let mut app = test_app();
        inject_pane_opened(&mut app, 100, "ssh:staging");
        assert_eq!(app.pane_stack.len(), 2);
        // Compute close-button position: after tab 1 + space + tab 2
        // + space. `[1 zsh]` (4+3=7 cells), space, `[2 ssh:staging*]`
        // (label len 11 + active chrome 5 = 16 cells), space, `[×]`.
        // The test doesn't assume exact column — find it by walking
        // tile_center instead: click near the far right of the strip.
        let rect = app.view.layout.rect_of(TileId::Pty).unwrap();
        let strip_y = rect.y + 1;
        // The full-width PTY tile is 118 cells wide at 120×40; close
        // affordance `[×]` sits past the tabs, before the right edge.
        // Find it by direct hit-test rather than hardcoding.
        let strip_rect = Rect::new(rect.x + 1, strip_y, rect.width - 2, 1);
        let mut close_x = None;
        for x in rect.x + 1..rect.x + rect.width - 1 {
            if let Some(pty_tile::TabStripHit::CloseActive) =
                pty_tile::hit_test_tab_strip(&app.pane_stack, app.active_pane, strip_rect, x)
            {
                close_x = Some(x);
                break;
            }
        }
        let close_x = close_x.expect("close affordance must be hit-testable somewhere");

        let actions = app.handle_event(AppEvent::MouseClick {
            x: close_x,
            y: strip_y,
        });
        assert_eq!(app.pane_stack.len(), 1, "close affordance closed one pane");
        assert!(
            actions.iter().any(|a| matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(&env.payload, Payload::ClosePane { .. })
            )),
            "close affordance dispatches ClosePane; got {actions:?}"
        );
    }

    #[test]
    fn mouse_hover_tracks_hovered_tile() {
        let mut app = test_app();
        assert_eq!(app.hovered_tile, None);
        let (x, y) = tile_center(&app, TileId::Docker);
        let actions = app.handle_event(AppEvent::MouseHover { x, y });
        assert_eq!(app.hovered_tile, Some(TileId::Docker));
        assert!(
            actions.iter().any(|a| matches!(a, AppAction::DrawFrame)),
            "hover change triggers a redraw"
        );
    }

    #[test]
    fn mouse_hover_to_same_tile_is_idempotent() {
        let mut app = test_app();
        let (x, y) = tile_center(&app, TileId::Ports);
        app.handle_event(AppEvent::MouseHover { x, y });
        let actions = app.handle_event(AppEvent::MouseHover { x: x + 1, y });
        // Same tile — no DrawFrame.
        assert!(
            !actions.iter().any(|a| matches!(a, AppAction::DrawFrame)),
            "hover within same tile skips the redraw; got {actions:?}"
        );
    }

    #[test]
    fn mouse_hover_off_all_tiles_clears_hovered_tile() {
        let mut app = test_app();
        let (x, y) = tile_center(&app, TileId::Fleet);
        app.handle_event(AppEvent::MouseHover { x, y });
        assert_eq!(app.hovered_tile, Some(TileId::Fleet));
        app.handle_event(AppEvent::MouseHover { x: 9999, y: 9999 });
        assert_eq!(app.hovered_tile, None);
    }

    #[test]
    fn help_overlay_starts_hidden() {
        let app = test_app();
        assert!(!app.help_visible);
    }

    #[test]
    fn esc_while_help_visible_dismisses_without_reaching_pty() {
        let mut app = test_app();
        // Open help via Ctrl-b ?.
        app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(app.help_visible);

        // Press bare Esc — should close the overlay and NOT forward
        // `\x1b` to the pty as SendInput.
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec()));
        assert!(!app.help_visible);
        let forwarded_esc = actions.iter().any(|a| {
            matches!(
                a,
                AppAction::SendEnvelope(env)
                    if matches!(
                        &env.payload,
                        Payload::SendInput { data, .. } if data == b"\x1b"
                    )
            )
        });
        assert!(
            !forwarded_esc,
            "help-visible Esc must not reach the pty; got {actions:?}"
        );
    }

    #[test]
    fn ctrl_b_d_while_help_visible_still_detaches() {
        // Escape hatch: even while the overlay absorbs input, the
        // user's explicit detach keybind must work.
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(app.help_visible);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02d".to_vec()));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, AppAction::Detach(DetachReason::User))),
            "Ctrl-b d must detach even with overlay visible; got {actions:?}"
        );
    }

    #[test]
    fn mouse_click_while_help_visible_dismisses_overlay() {
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(app.help_visible);
        let (x, y) = tile_center(&app, TileId::Docker);
        let _ = app.handle_event(AppEvent::MouseClick { x, y });
        assert!(!app.help_visible, "click dismissed overlay");
        // Focus should NOT have moved to Docker — the click is absorbed
        // by the overlay dismissal, not forwarded to tile focus.
        assert_eq!(app.view.focused, TileId::Pty);
    }

    #[test]
    fn ctrl_b_question_while_help_visible_toggles_off() {
        // Ctrl-b ? must round-trip the overlay — a second press
        // closes it rather than being absorbed as dismissal input.
        let mut app = test_app();
        app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(app.help_visible);
        app.handle_event(AppEvent::StdinChunk(b"\x02?".to_vec()));
        assert!(!app.help_visible);
    }

    /// End-to-end: feed SGR mouse bytes through MouseParser →
    /// synthesize AppEvent → drive App state machine. Ensures the
    /// production byte-flow path hooks up correctly without the
    /// session-level I/O scaffolding.
    #[test]
    fn sgr_stdin_bytes_drive_app_through_mouse_parser() {
        use crate::mouse::MouseParser;

        let mut app = test_app();
        fleet_with_hosts(&mut app, &["primary"]);

        // SGR mouse click at the Fleet tile's first data row. Compute
        // the 1-indexed SGR coords from the layout.
        let rect = app.view.layout.rect_of(TileId::Fleet).unwrap();
        let x = rect.x + 2;
        let y = rect.y + 1 + 1; // border + status → first row
        let bytes = format!("\x1b[<0;{};{}M", x + 1, y + 1);

        let mut parser = MouseParser::new();
        let (remaining, events) = parser.parse(bytes.as_bytes());
        assert!(remaining.is_empty());
        assert_eq!(events.len(), 1);
        for event in events {
            app.handle_event(event);
        }
        assert_eq!(app.view.focused, TileId::Fleet);
        assert_eq!(app.fleet.selection, 0);
    }

    // ---- Phase 6 Slice 6c-iii: host picker modal + Docker retarget ----

    fn populate_fleet_with_two_hosts(app: &mut App) {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::HostList {
                    hosts: vec![
                        tepegoz_proto::HostEntry {
                            alias: "alpha".into(),
                            hostname: "alpha.example".into(),
                            user: "test".into(),
                            port: 22,
                            identity_files: vec![],
                            proxy_jump: None,
                        },
                        tepegoz_proto::HostEntry {
                            alias: "bravo".into(),
                            hostname: "bravo.example".into(),
                            user: "test".into(),
                            port: 22,
                            identity_files: vec![],
                            proxy_jump: None,
                        },
                    ],
                    source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));

        for (alias, state) in [
            ("alpha", tepegoz_proto::HostState::Connected),
            ("bravo", tepegoz_proto::HostState::Disconnected),
        ] {
            let env = Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Event(EventFrame {
                    subscription_id: app.fleet.sub_id,
                    event: Event::HostStateChanged {
                        alias: alias.into(),
                        state,
                        reason: None,
                    },
                }),
            };
            app.handle_event(AppEvent::DaemonEnvelope(env));
        }

        // 6d-i: alpha is the Connected host that should be usable
        // for retarget — give it `"docker"` capability so the picker
        // greys it green. Bravo (Disconnected) doesn't need a cap
        // entry — its row greys based on state.
        let caps_env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "alpha".into(),
                    capabilities: vec!["docker".into()],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(caps_env));
    }

    #[test]
    fn ctrl_b_t_on_docker_tile_opens_picker_preselected_to_local() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        // Focus Docker.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        assert_eq!(app.view.focused, TileId::Docker);

        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        assert!(app.host_picker.is_some(), "Ctrl-b t opens picker");
        let picker = app.host_picker.as_ref().unwrap();
        assert_eq!(
            picker.target_tile,
            HostPickerTargetTile::Docker,
            "picker targets Docker tile (current focus)"
        );
        assert_eq!(
            picker.selected, 0,
            "picker pre-selects current target (Local → row 0)"
        );
    }

    #[test]
    fn ctrl_b_t_with_non_target_tile_focused_is_silent_noop() {
        let mut app = test_app();
        // Focus Fleet (not target-capable in 6c-iii). Layout is
        // PTY on top; Docker / Ports / Fleet across the middle; so
        // j → Docker, l → Ports, l → Fleet.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        assert_eq!(app.view.focused, TileId::Fleet);

        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        assert!(
            app.host_picker.is_none(),
            "Ctrl-b t on non-target-capable tile must not open picker"
        );
    }

    #[test]
    fn escape_dismisses_picker_without_changing_target() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        let prev_target = app.docker.target.clone();

        app.handle_event(AppEvent::StdinChunk(b"\x1b".to_vec())); // bare Esc
        assert!(app.host_picker.is_none(), "Esc dismisses picker");
        assert_eq!(
            app.docker.target, prev_target,
            "Esc doesn't commit a retarget"
        );
    }

    #[test]
    fn picker_navigation_arrows_and_jk_move_selection() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        assert_eq!(app.host_picker.as_ref().unwrap().selected, 0);

        // j → down.
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec()));
        assert_eq!(app.host_picker.as_ref().unwrap().selected, 1);

        // Arrow down (CSI B).
        app.handle_event(AppEvent::StdinChunk(b"\x1b[B".to_vec()));
        assert_eq!(app.host_picker.as_ref().unwrap().selected, 2);

        // k → up.
        app.handle_event(AppEvent::StdinChunk(b"k".to_vec()));
        assert_eq!(app.host_picker.as_ref().unwrap().selected, 1);

        // Clamping: down past end doesn't go past last row.
        app.handle_event(AppEvent::StdinChunk(b"jjjj".to_vec()));
        assert_eq!(
            app.host_picker.as_ref().unwrap().selected,
            2,
            "clamped at last row (Local + 2 hosts = index 2)"
        );
    }

    #[test]
    fn enter_on_connected_host_commits_retarget_and_reshapes_subs() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // → alpha (connected)

        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        assert!(app.host_picker.is_none(), "Enter dismisses picker");
        assert_eq!(
            app.docker.target,
            tepegoz_proto::ScopeTarget::Remote {
                alias: "alpha".into()
            },
            "target is now Remote{{alpha}}"
        );

        // Must have sent Unsubscribe(docker_sub_id) + Subscribe(Docker,
        // target: Remote{alpha}) in that order.
        let subs: Vec<&AppAction> = actions
            .iter()
            .filter(|a| matches!(a, AppAction::SendEnvelope(_)))
            .collect();
        assert!(
            subs.len() >= 2,
            "retarget must emit at least Unsub + Sub envelopes"
        );
        let mut saw_unsub = false;
        let mut saw_resub = false;
        for a in &subs {
            if let AppAction::SendEnvelope(env) = a {
                match &env.payload {
                    Payload::Unsubscribe { id } if *id == app.docker.sub_id => saw_unsub = true,
                    Payload::Subscribe(Subscription::Docker { id, target })
                        if *id == app.docker.sub_id =>
                    {
                        assert_eq!(
                            *target,
                            tepegoz_proto::ScopeTarget::Remote {
                                alias: "alpha".into()
                            },
                            "resubscribe must carry the new target"
                        );
                        saw_resub = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(
            saw_unsub && saw_resub,
            "retarget must emit Unsubscribe(old) + Subscribe(new)"
        );

        // State resets: Connecting + selection 0 + pending_confirm cleared.
        assert!(matches!(app.docker.state, DockerScopeState::Connecting));
        assert_eq!(app.docker.selection, 0);
        assert!(app.docker.pending_confirm.is_none());
    }

    #[test]
    fn enter_on_greyed_out_host_is_noop_picker_stays_open() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        // Navigate to bravo (row 2, Disconnected → greyed-out).
        app.handle_event(AppEvent::StdinChunk(b"jj".to_vec()));
        assert_eq!(app.host_picker.as_ref().unwrap().selected, 2);

        let prev_target = app.docker.target.clone();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        assert!(
            app.host_picker.is_some(),
            "Enter on greyed row must leave picker open for another choice"
        );
        assert_eq!(
            app.docker.target, prev_target,
            "Enter on greyed row must not commit a retarget"
        );
        let wire_actions = actions
            .iter()
            .filter(|a| matches!(a, AppAction::SendEnvelope(_)))
            .count();
        assert_eq!(
            wire_actions, 0,
            "Enter on greyed row sends no Subscribe / Unsubscribe envelopes"
        );
    }

    #[test]
    fn click_on_docker_title_bar_opens_picker() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        let rect = app.view.layout.rect_of(TileId::Docker).unwrap();

        // Docker tile top-border row = rect.y. Click anywhere on it.
        let actions = app.handle_event(AppEvent::MouseClick {
            x: rect.x + 2,
            y: rect.y,
        });
        assert!(
            app.host_picker.is_some(),
            "click on Docker title bar opens picker"
        );
        assert!(
            actions.iter().any(|a| matches!(a, AppAction::DrawFrame)),
            "retarget open emits DrawFrame"
        );
    }

    #[test]
    fn click_while_picker_visible_dismisses_it() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        assert!(app.host_picker.is_some());

        // Any click dismisses — same gesture as help overlay.
        app.handle_event(AppEvent::MouseClick { x: 5, y: 5 });
        assert!(
            app.host_picker.is_none(),
            "click while picker open must dismiss"
        );
    }

    #[test]
    fn ctrl_b_t_while_picker_open_dismisses_it() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        assert!(app.host_picker.is_some());

        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        assert!(
            app.host_picker.is_none(),
            "second Ctrl-b t must dismiss the open picker (toggle semantics)"
        );
    }

    #[test]
    fn ctrl_b_d_while_picker_open_detaches_escape_hatch() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));

        let actions = app.handle_event(AppEvent::StdinChunk(b"\x02d".to_vec()));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, AppAction::Detach(DetachReason::User))),
            "Ctrl-b d is escape hatch — detaches even from picker"
        );
    }

    #[test]
    fn retarget_to_same_target_is_noop_no_envelopes() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        // selected=0 is Local; current target is Local; pressing Enter
        // should commit to the same target — no Unsubscribe/Subscribe
        // should fire.
        assert_eq!(app.docker.target, tepegoz_proto::ScopeTarget::Local);
        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        assert!(app.host_picker.is_none(), "picker dismissed");
        let wire_actions = actions
            .iter()
            .filter(|a| matches!(a, AppAction::SendEnvelope(_)))
            .count();
        assert_eq!(
            wire_actions, 0,
            "same-target commit sends no envelopes (noop shortcut in retarget_docker)"
        );
    }

    #[test]
    fn retarget_from_logs_view_exits_logs_and_unsubscribes_both_subs() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        populate_docker_and_focus(&mut app, vec![make_container("c1", "img", "running")]);
        // Enter logs view on c1.
        app.handle_event(AppEvent::StdinChunk(b"l".to_vec()));
        let logs_sub = match &app.docker.view {
            DockerView::Logs(l) => l.sub_id,
            _ => panic!("expected Logs view after pressing l"),
        };

        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // → alpha
        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));

        assert!(
            matches!(app.docker.view, DockerView::List),
            "retarget must exit Logs view"
        );
        let unsub_ids: Vec<u64> = actions
            .iter()
            .filter_map(|a| match a {
                AppAction::SendEnvelope(e) => match e.payload {
                    Payload::Unsubscribe { id } => Some(id),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            unsub_ids.contains(&logs_sub),
            "retarget must unsubscribe the logs sub ({logs_sub})"
        );
        assert!(
            unsub_ids.contains(&app.docker.sub_id),
            "retarget must unsubscribe the list sub"
        );
    }

    // ---- Phase 6 Slice 6d-i: AgentCapabilities event handling ----

    #[test]
    fn agent_capabilities_event_populates_host_capabilities_map() {
        let mut app = test_app();
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "demo".into(),
                    capabilities: vec!["docker".into(), "ports".into()],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        assert_eq!(
            app.host_capabilities.get("demo"),
            Some(&vec!["docker".to_string(), "ports".into()]),
            "AgentCapabilities event must populate the per-alias map"
        );
    }

    #[test]
    fn agent_capabilities_with_empty_vec_clears_capabilities() {
        let mut app = test_app();
        // Seed populated.
        let seed = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "host".into(),
                    capabilities: vec!["docker".into()],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(seed));
        // Disconnect re-emits with empty caps.
        let teardown = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "host".into(),
                    capabilities: vec![],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(teardown));
        assert_eq!(
            app.host_capabilities.get("host"),
            Some(&Vec::<String>::new()),
            "empty capabilities vec must overwrite (not delete) the entry — \
             a Connected host with no caps still needs a `(no docker)` row"
        );
    }

    #[test]
    fn picker_greys_connected_host_without_required_capability() {
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);

        // alpha is Connected and has "docker" (set in
        // populate_fleet_with_two_hosts). Add a third host that's
        // Connected but advertises no capabilities.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::HostList {
                    hosts: vec![
                        tepegoz_proto::HostEntry {
                            alias: "alpha".into(),
                            hostname: "alpha.example".into(),
                            user: "test".into(),
                            port: 22,
                            identity_files: vec![],
                            proxy_jump: None,
                        },
                        tepegoz_proto::HostEntry {
                            alias: "noprobe".into(),
                            hostname: "noprobe.example".into(),
                            user: "test".into(),
                            port: 22,
                            identity_files: vec![],
                            proxy_jump: None,
                        },
                    ],
                    source: "test".into(),
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
        let connected_no_caps = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::HostStateChanged {
                    alias: "noprobe".into(),
                    state: tepegoz_proto::HostState::Connected,
                    reason: None,
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(connected_no_caps));
        let empty_caps = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "noprobe".into(),
                    capabilities: vec![],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(empty_caps));

        // Both alpha and noprobe are Connected. Picker rows for
        // "docker" should mark alpha usable, noprobe NOT usable.
        let rows = app.host_picker_rows("docker");
        let alpha_row = rows
            .iter()
            .find(|r| matches!(r, HostPickerRow::Remote { alias, .. } if alias == "alpha"))
            .expect("alpha row");
        let noprobe_row = rows
            .iter()
            .find(|r| matches!(r, HostPickerRow::Remote { alias, .. } if alias == "noprobe"))
            .expect("noprobe row");

        match alpha_row {
            HostPickerRow::Remote {
                usable,
                has_capability,
                ..
            } => {
                assert!(*usable, "alpha (Connected + has docker) must be usable");
                assert!(*has_capability);
            }
            _ => unreachable!(),
        }
        match noprobe_row {
            HostPickerRow::Remote {
                usable,
                has_capability,
                ..
            } => {
                assert!(
                    !*usable,
                    "noprobe (Connected but missing docker cap) must NOT be usable"
                );
                assert!(!*has_capability);
            }
            _ => unreachable!(),
        }
    }

    // ---- Phase 6 Slice 6d-ii: Ports/Processes retarget reuse ----

    fn populate_fleet_with_ports_processes_caps(app: &mut App) {
        populate_fleet_with_two_hosts(app);
        // Add ports + processes to alpha's capability list (it
        // already has docker from populate_fleet_with_two_hosts).
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "alpha".into(),
                    capabilities: vec!["docker".into(), "ports".into(), "processes".into()],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(env));
    }

    #[test]
    fn ctrl_b_t_on_ports_view_opens_picker_with_ports_capability() {
        let mut app = test_app();
        populate_fleet_with_ports_processes_caps(&mut app);
        // Focus PTY → Docker → Ports. PTY default → j → Docker → l → Ports.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        assert_eq!(app.view.focused, TileId::Ports);
        // Default Ports view is the Ports view (not Processes).
        assert!(matches!(app.ports.active, PortsActiveView::Ports));

        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        let picker = app.host_picker.as_ref().expect("picker opened");
        assert_eq!(picker.target_tile, HostPickerTargetTile::Ports);
        assert_eq!(picker.required_capability, "ports");
    }

    #[test]
    fn ctrl_b_t_on_processes_view_uses_processes_capability() {
        let mut app = test_app();
        populate_fleet_with_ports_processes_caps(&mut app);
        // Focus Ports tile and toggle to Processes view.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec())); // toggle
        assert!(matches!(app.ports.active, PortsActiveView::Processes));

        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        let picker = app.host_picker.as_ref().expect("picker opened");
        assert_eq!(picker.target_tile, HostPickerTargetTile::Processes);
        assert_eq!(picker.required_capability, "processes");
    }

    #[test]
    fn enter_commits_ports_retarget_and_resubscribes() {
        let mut app = test_app();
        populate_fleet_with_ports_processes_caps(&mut app);
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec())); // focus Ports
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // → alpha

        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        assert_eq!(
            app.ports.ports_target,
            tepegoz_proto::ScopeTarget::Remote {
                alias: "alpha".into()
            }
        );
        // Processes target unchanged — independent retargets.
        assert_eq!(
            app.ports.processes_target,
            tepegoz_proto::ScopeTarget::Local
        );

        let mut saw_unsub = false;
        let mut saw_resub = false;
        for a in &actions {
            if let AppAction::SendEnvelope(env) = a {
                match &env.payload {
                    Payload::Unsubscribe { id } if *id == app.ports.ports_sub_id => {
                        saw_unsub = true;
                    }
                    Payload::Subscribe(Subscription::Ports { id, target })
                        if *id == app.ports.ports_sub_id =>
                    {
                        assert_eq!(
                            *target,
                            tepegoz_proto::ScopeTarget::Remote {
                                alias: "alpha".into()
                            }
                        );
                        saw_resub = true;
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_unsub && saw_resub);
    }

    #[test]
    fn ports_and_processes_targets_are_independent() {
        // Retarget Ports to alpha, retarget Processes to local
        // (separately) — neither should affect the other's target
        // field. This locks the "two independent fields" design.
        let mut app = test_app();
        populate_fleet_with_ports_processes_caps(&mut app);

        // Retarget Ports → alpha.
        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02l".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"j\r".to_vec()));

        assert_eq!(
            app.ports.ports_target,
            tepegoz_proto::ScopeTarget::Remote {
                alias: "alpha".into()
            }
        );
        assert_eq!(
            app.ports.processes_target,
            tepegoz_proto::ScopeTarget::Local
        );

        // Toggle to Processes view + retarget Processes → alpha too.
        app.handle_event(AppEvent::StdinChunk(b"p".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec()));
        app.handle_event(AppEvent::StdinChunk(b"j\r".to_vec()));

        assert_eq!(
            app.ports.processes_target,
            tepegoz_proto::ScopeTarget::Remote {
                alias: "alpha".into()
            }
        );
        // Ports target still Remote{alpha} from the first retarget.
        assert_eq!(
            app.ports.ports_target,
            tepegoz_proto::ScopeTarget::Remote {
                alias: "alpha".into()
            }
        );
        // Docker target still Local (untouched by Ports/Processes retargets).
        assert_eq!(app.docker.target, tepegoz_proto::ScopeTarget::Local);
    }

    #[test]
    fn enter_on_connected_no_cap_host_is_noop_picker_stays_open() {
        // 6d-i: capability greying is the gate — Enter on a Connected
        // host that lacks the required cap must NOT commit a retarget.
        let mut app = test_app();
        populate_fleet_with_two_hosts(&mut app);
        // Replace alpha's capabilities with empty.
        let empty_caps = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: app.fleet.sub_id,
                event: Event::AgentCapabilities {
                    alias: "alpha".into(),
                    capabilities: vec![],
                },
            }),
        };
        app.handle_event(AppEvent::DaemonEnvelope(empty_caps));

        app.handle_event(AppEvent::StdinChunk(b"\x02j".to_vec())); // focus Docker
        app.handle_event(AppEvent::StdinChunk(b"\x02t".to_vec())); // open picker
        app.handle_event(AppEvent::StdinChunk(b"j".to_vec())); // → alpha (now greyed)

        let prev_target = app.docker.target.clone();
        let actions = app.handle_event(AppEvent::StdinChunk(b"\r".to_vec()));
        assert!(
            app.host_picker.is_some(),
            "Enter on Connected-no-cap row leaves picker open"
        );
        assert_eq!(
            app.docker.target, prev_target,
            "Enter on Connected-no-cap row must not commit a retarget"
        );
        let wire_actions = actions
            .iter()
            .filter(|a| matches!(a, AppAction::SendEnvelope(_)))
            .count();
        assert_eq!(
            wire_actions, 0,
            "Connected-no-cap commit sends no envelopes"
        );
    }
}
