# Tepegöz v1 roadmap

Target: **0.1.0 release** at end of Phase 10. Rough budget: 15–20 weeks full-time.

Status key: ✅ complete · 🟡 code+tests green, user acceptance pending · 🟠 in progress · ⚪ not started.

Per-phase: goal, delivered (or scope), acceptance test, explicit non-goals, risks.

---

## Phase 0 — Scaffold · ✅ · `81c7731`

**Goal.** A buildable, linted, CI-green Cargo workspace with stubbed subcommands.

**Delivered.**
- Workspace `Cargo.toml` with 12 crate members + `xtask`.
- `rust-toolchain.toml` pinned `stable`; `mise.toml` pinned to `1.94.1`.
- `.cargo/config.toml` with `cargo xtask` alias.
- `.github/workflows/ci.yml`: fmt, clippy `-D warnings`, native tests on ubuntu-latest + macos-latest, cross-build matrix (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`) via `cargo-zigbuild`.
- `tepegoz` binary with clap-derived CLI stubs for `daemon`, `tui`, `connect`, `agent`, `doctor`.
- `tracing_subscriber` wired with `RUST_LOG` + `--log-level` fallback.

**Acceptance.** Local `cargo build && cargo test && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` all pass; CI green on both OSes.

**Not in scope.** Any runtime behavior beyond `--help` text.

---

## Phase 1 — Proto + daemon + TUI round-trip · ✅ · `3715bf9`

**Goal.** Prove the daemon-as-source-of-truth pattern. Daemon holds state; TUI is a passive viewer; state survives client disconnect.

**Delivered.**
- `tepegoz-proto`: rkyv 0.8 `Envelope { version, payload }` with 4-byte big-endian length prefix and bytecheck validation on every read.
- Messages (v1): `Hello`, `Welcome`, `Ping`, `Pong`, `Subscribe(Status)`, `Unsubscribe`, `Event(EventFrame)`, `Error(ErrorInfo)`.
- `tepegoz-proto::socket::default_socket_path()` → `$XDG_RUNTIME_DIR/tepegoz-<uid>/daemon.sock` (fallback `$TMPDIR`, then `/tmp`).
- `tepegoz-core`: Unix socket listener (0700 parent when default, 0600 socket); stale-socket eviction; graceful SIGINT shutdown; `StatusSnapshot` streamed at 1 Hz to subscribers.
- Original `tepegoz-tui`: ratatui status panel (daemon pid, uptime, client counts).
- Atomic counters in `SharedState` — no lock contention on sampling.
- Single writer task per client draining an mpsc — outbound frames serialized without per-write locks.

**Acceptance test.** `crates/tepegoz-core/tests/daemon_persistence.rs` — client #1 connects, snapshots `clients_total=1`, disconnects. Client #2 reconnects, snapshots `clients_total=2`; `uptime_seconds` monotonic; `daemon_pid` identical — proving state survived.

**Not in scope.** PTYs. Scope panels. Authentication. Any subscription kind other than `Status`.

---

## Phase 2 — Local pty multiplex + persistence · 🟡

**Status.** Code + tests green at `f12d194`; one user-visible bug under diagnosis (immediate detach on attach) — see `docs/ISSUES.md#active`.

**Relevant commits.** `eab274c` (scrollback/broadcast race fix), `321ed5e` (cwd + `TEPEGOZ_PANE_ID` fixes), `f12d194` (diagnostic tracing).

**Goal.** "Kill the TUI mid-command, reopen, see where I left off, keep going." Daemon owns the shell; TUI is a window.

**Delivered (code).**
- `tepegoz-pty`: `PtyManager` owns `HashMap<PaneId, Arc<Pane>>`. Each `Pane` wraps a portable-pty master, a blocking reader thread (appends to a 2 MiB `VecDeque<Bytes>` ring buffer, broadcasts on a tokio channel), a waiter thread (records exit code, publishes `PaneUpdate::Exit`), and a size cell.
- Wire protocol v2: `OpenPane`, `AttachPane`, `ClosePane`, `ListPanes`, `SendInput`, `ResizePane`; responses `PaneOpened`, `PaneList`; events `PaneSnapshot`, `PaneOutput`, `PaneExit`, `PaneLagged`.
- Daemon client session: per-`AttachPane` forwarder task translates broadcast events into protocol events keyed by subscription id.
- TUI rewrite: raw-passthrough attacher — raw mode + alt screen, stdin → `SendInput`, `PaneOutput` → stdout, `SIGWINCH` → `ResizePane`. Detach via `Ctrl-b d` or `Ctrl-b q` (InputFilter state machine).
- Daemon stamps `TEPEGOZ_PANE_ID=<id>` into pty env; TUI refuses to run if its own env has that var (blocks recursive attach).
- Shell starts in TUI's `current_dir()` (portable-pty otherwise defaults to `$HOME`).

**Acceptance tests.**
- `crates/tepegoz-core/tests/pty_persistence.rs` — client opens pane, sends `echo MARKER\n`, verifies output; drops; second client reconnects, re-attaches, `PaneSnapshot` contains `MARKER` from the ring buffer.
- `tepegoz-pty::tests::subscribe_does_not_duplicate_bytes` — drives 50 markers mid-stream; asserts each appears exactly once across snapshot + live (regression for the scrollback/broadcast race).
- `tepegoz-pty::tests::pane_honors_cwd_and_exposes_pane_id_env` — `pwd` output contains requested cwd; `$TEPEGOZ_PANE_ID` matches pane id.

**Pending to mark ✅.** Resolve the active immediate-detach bug per `docs/ISSUES.md`.

**Not in scope.** Tiled layout. Multi-pane navigation. VT100 emulation for overlay chrome. (MVP is deliberately single-pane raw passthrough; chrome waits until we actually need it.)

**Risks.** Because the TUI is raw passthrough, anything the terminal or the shell does that coincidentally matches the detach prefix (`Ctrl-b d/q`) triggers detach. The active bug is suspected to live here.

---

## Phase 3 — Docker scope panel · 🟠 (Slices A + B + C1 + C2 landed; C1.5 tiling foundation in progress — Decision #7 supersedes the C1 `View::{Pane, Scope}` mode enum; C2 Docker renderer being re-plumbed into a tile `Rect`)

**Goal.** First scope panel. Lists containers; tails logs; execs into container (opens a new pane); lifecycle actions. Sets the UX template for Ports/Processes/Logs in Phase 4.

Phase 3 is large enough to land in slices. Each slice is independently green and tests its own behavior end-to-end.

### Slice A — Foundation: socket discovery + container list subscription · ✅

**Delivered (code).**
- `tepegoz-docker`: socket discovery walks `$DOCKER_HOST` env > Docker Desktop (`~/.docker/run/docker.sock`) > Colima (`~/.colima/default/docker.sock`) > Rancher Desktop (`~/.rd/docker.sock`) > native Linux (`/var/run/docker.sock`). `Engine::connect` returns the first candidate that pings inside a 5 s probe budget, or a structured `ConnectError` listing every attempt with its reason. `Engine::list_containers` returns wire-typed `Vec<DockerContainer>`. `bollard::models::ContainerSummary` → `tepegoz_proto::DockerContainer` translation handles missing optional fields with safe defaults; labels come out sorted; empty/unset state collapses to `"unknown"`.
- Wire protocol bumped to **v3**: `Subscription::Docker { id }`; `Event::ContainerList { containers, engine_source }`; `Event::DockerUnavailable { reason }`; supporting types `DockerContainer`, `DockerPort`, `KeyValue`.
- Daemon: per-`Subscribe(Docker)` poll task tracked in `HashMap<id, AbortHandle>` so `Unsubscribe { id }` cancels just that subscription. Refresh interval 2 s; reconnect interval 5 s. Emits `DockerUnavailable` only on availability *transitions* (not on every retry) to avoid spamming clients.

**Acceptance tests.**
- `crates/tepegoz-core/tests/docker_scope.rs::docker_subscription_emits_either_container_list_or_unavailable` — spawns daemon, sends `Subscribe(Docker)`, asserts the first event is one of `ContainerList | DockerUnavailable` within a 30 s budget. Both paths green; `engine_source` and `reason` are non-empty.
- `crates/tepegoz-core/tests/docker_scope.rs::docker_subscription_returns_container_list_when_engine_is_running` — opt-in via `TEPEGOZ_DOCKER_TEST=1`. Insists on the Available path; meant for CI/local runs that provision docker beforehand.
- `tepegoz-docker::tests::into_wire_translates_bollard_summary` + `into_wire_handles_empty_state` — translation correctness in the absence of a real engine.
- `tepegoz-docker::socket::tests::*` — socket discovery order is stable.
- `tepegoz-proto::codec::tests::{subscribe_docker_roundtrip, docker_container_list_event_roundtrip, docker_unavailable_event_roundtrip}` — wire roundtrip for the new variants.

**Not in this slice.** Lifecycle actions, logs streaming, container stats, TUI scope view. See B/C/D below.

### Slice B — Lifecycle actions + logs streaming + container stats · ✅

**Delivered (code).**
- Wire protocol bumped to **v4**. Commands: `Payload::DockerAction(DockerActionRequest { request_id, container_id, kind: DockerActionKind })`. Responses: `Payload::DockerActionResult(DockerActionResult { request_id, container_id, kind, outcome: Success | Failure { reason } })`. New subscriptions: `Subscription::DockerLogs { id, container_id, follow, tail_lines }` and `Subscription::DockerStats { id, container_id }`. New events: `Event::ContainerLog { stream, data }`, `Event::ContainerStats(DockerStats { cpu_percent, mem_bytes, mem_limit_bytes })`, `Event::DockerStreamEnded { reason }`. Supporting types `LogStream { Stdout, Stderr }` and `DockerStats`.
- `tepegoz-docker`: `Engine::action(container_id, DockerActionKind)` translates to bollard `start_container` / `stop_container` / `restart_container` / `kill_container` / `remove_container` (force-remove for the last). `Engine::logs_stream(container_id, follow, tail_lines)` returns a boxed `Stream<Item = anyhow::Result<(LogStream, Vec<u8>)>>`; bollard's `LogOutput::{StdOut, StdErr, Console}` map to wire types (`Console` flows as `Stdout`; `StdIn` is dropped). `Engine::stats_stream(container_id)` returns `Stream<Item = anyhow::Result<DockerStats>>`. CPU% computed from `cpu_stats` vs `precpu_stats` deltas using the standard docker-stats-CLI formula; `0.0` when the delta can't be calculated (first sample, missing precpu, sys_delta=0).
- `tepegoz-core`: per-subscription forwarder tasks for `DockerLogs` and `DockerStats`, tracked in the same `HashMap<id, AbortHandle>` as `Subscribe(Docker)` so `Unsubscribe { id }` cancels uniformly. Both forwarders always emit a terminal `Event::DockerStreamEnded { reason }` (even on connect failure or empty stream) so the client knows the stream is done. `DockerAction` runs in a spawned task — slow dockerd doesn't stall the session loop; engine-unavailable and bollard errors both surface as `DockerActionResult::Failure { reason }`.

**Acceptance tests.**
- `tepegoz-proto::codec` — roundtrip for `DockerAction`, `DockerActionResult` (including `Failure` reason), `Subscribe(DockerLogs)`, `Event::ContainerLog`, `Event::ContainerStats`.
- `tepegoz-docker::tests` — `stats_to_wire_computes_cpu_percent` (synthetic CPU delta → 80%), `stats_to_wire_returns_zero_cpu_when_delta_is_unavailable` (no precpu, sys_delta=0), `stats_to_wire_handles_missing_memory_section`.
- `tepegoz-core/tests/docker_scope.rs` adds three always-on unreachable-engine tests (`DockerAction` returns `Failure` with reason; `Subscribe(DockerLogs)` and `Subscribe(DockerStats)` both terminate with `DockerStreamEnded`) plus an opt-in `TEPEGOZ_DOCKER_TEST=1` end-to-end test that provisions an `alpine:latest` container, observes a stdout marker through `Subscribe(DockerLogs)`, observes a stats sample with `mem_bytes > 0` through `Subscribe(DockerStats)`, and asserts `DockerAction(Restart)` returns `Success`. The container is force-removed via `Drop` cleanup so a panic mid-test doesn't leak it.

### Slice C — TUI scope view + scope/pty switch

Slice C is the heaviest TUI refactor in v1 — it replaces the pure-passthrough attach loop we had through Phase 2 with the god-view tiled layout per Decision #7. The architecture commitment Phases 4 (Ports/Processes), 5 (SSH remote pty), 7 (port scanner), and 9 (Claude Code) inherit lives here. Per CTO sign-off, lands as four sub-slices: **C1** (pure-state-machine bus + event-driven skeleton), **C1.5** (tiling foundation — god-view layout, vt100 pty tile, focus nav), **C2** (Docker scope rendering + subscription lifecycle), **C3** (action keybinds + toasts + logs panel).

A prior revision of this slice shipped C1 as a `View::{Pane, Scope}` two-mode app. That model was the drift caught by the product-vision review and is superseded by Decision #7; the C1.5 sub-slice corrects it before any further rendering work lands. C1's `AppEvent`/`AppAction` bus, Runtime↔App split, and daemon subscription-uniformity fix survive the correction. See each sub-slice below for the precise salvage list.

#### Slice C1 — TUI skeleton + view enum + event bus · ✅ (superseded in part by C1.5)

**Superseded in part by C1.5 (Decision #7).** The pure-state-machine `App`, `AppEvent`/`AppAction` bus, `InputFilter` split-across-chunks handling, Runtime↔App split, daemon subscription-uniformity fix, and scope-renderer plumbing in C2 all survive. What is removed in C1.5: `View::{Pane, Scope(ScopeKind)}`, `switch_to_scope` / `switch_to_pane`, the synthetic re-attach sequence, `AppAction::{EnterPaneMode, EnterScopeMode}`, `InputAction::{SwitchToScope, SwitchToPane}`, and any test asserting mode-switch semantics. `View` is redefined as `{ layout: TileLayout, focused: TileId }`. This section preserves the C1 record as landed; the new shape is documented under Slice C1.5 below.

**Delivered (code).**
- `tepegoz-tui/src/app.rs`: pure-state-machine `App` with `View::{Pane, Scope(ScopeKind::Docker)}`. Single mutator `App::handle_event(AppEvent) -> Vec<AppAction>` — no I/O. `AppEvent::{StdinChunk, DaemonEnvelope, Resize, Tick, PendingActionTimeout}` covers every external happening; `AppAction::{SendEnvelope, WriteStdout, EnterPaneMode, EnterScopeMode, DrawScope, Detach(DetachReason::{User, PaneExited{exit_code}})}` enumerates every side effect. `DockerScope` state (Idle/Connecting/Available/Unavailable) is defined with placeholder fields (selection, filter, sub_id) so C2 doesn't have to grow the struct shape.
- `tepegoz-tui/src/input.rs`: `InputFilter` extended with `SwitchToScope` (Ctrl-b s), `SwitchToPane` (Ctrl-b a), `Help` (Ctrl-b ?). All control sequences split the byte stream cleanly (any pre-control bytes get forwarded as `Forward(Vec<u8>)` first, then the control action; bytes after the control are dropped).
- `tepegoz-tui/src/session.rs`: thin `AppRuntime` owns the I/O glue — daemon socket halves, writer mpsc, stdin reader, SIGWINCH stream, ratatui Terminal — and executes whatever `AppAction`s the App emits. Mode-conditional rendering: in Pane mode, raw stdout passthrough (no ratatui draw cycle); in Scope mode, ratatui takes over with a 30 Hz coalesced redraw tick that's gated off in pane mode (no CPU cost when not used).
- `tepegoz-tui/src/scope/docker.rs`: stub renderer for the docker scope view ("Slice C1 ships only the bus + view switch; C2 wires the container table"). Status bar shows the active `DockerScopeState` discriminant.
- New deps: `ratatui` 0.30 (default features; pulls `ratatui-crossterm` 0.1 + transitive crossterm 0.29; harmless to coexist with our existing crossterm 0.28 since both share the same OS-level termios state).
- View switch mechanics: Pane → Scope clears the screen and starts the ratatui draw cycle. Scope → Pane clears the screen, cancels the previous `AttachPane` subscription, and sends a fresh `AttachPane` so the daemon replays current scrollback as `PaneSnapshot`. **TODO(phase-5):** scrollback re-transfer cost will matter over SSH; revisit if SSH bandwidth becomes a concern.

**Acceptance tests.**
- `tepegoz-tui::input::tests` (12) — every InputFilter behavior including the new control sequences and their split-across-chunks variants.
- `tepegoz-tui::app::tests` (14) — App state machine drives event sequences without any I/O: initial_actions allocates AttachPane + ResizePane; Ctrl-b d emits user detach; pane keystrokes forward as SendInput; Ctrl-b s switches to scope (EnterScopeMode + DrawScope); double-switch is idempotent; Ctrl-b a returns to pane with synthetic re-attach (Unsubscribe + fresh AttachPane); PaneOutput in Pane mode emits WriteStdout; PaneSnapshot likewise; PaneOutput in Scope mode is dropped (the synthetic re-attach replays from the daemon's ring); PaneExit propagates exit_code via DetachReason::PaneExited; events for unknown subscription ids are silently dropped; Resize forwards to daemon and only redraws in scope; Tick is a no-op in Pane and emits DrawScope in Scope.
- **Manual demo: NOT performed in C1 implementation environment** (no interactive terminal available). C2 must run the full demo sequence per Slice C "Demonstrable, not simulated" below.

**Not in this slice.** Scope rendering (table widget, three-state lifecycle visuals), `Subscribe(Docker)` wiring, navigation/filter, action keybinds. All in C2.

#### Slice C1.5 — Tiling foundation · 🟠 (in progress)

**Goal.** Replace the C1 mode-switch `View` model with the tiled-layout substrate per Decision #7. The god-view default layout renders on first run; scopes not yet implemented show labeled placeholder tiles.

**Delivered (plan).**
- `vt100` crate added to `tepegoz-tui`. Pty tile renders via vt100 parser + ratatui widget.
- `View` redefined: `{ layout: TileLayout, focused: TileId }`; `TileKind` enum: `Pty | Scope(ScopeKind) | Placeholder { label, eta_phase }`.
- All subscriptions live concurrently: `AttachPane` on startup, `Subscribe(Docker)` on startup, placeholders for Ports/Fleet/Claude.
- Focus navigation: `Ctrl-b h/j/k/l` + arrows; focus-respecting input routing.
- Default layout constructor renders the god-view mockup from README on first run with no config.
- The C2c2 Docker renderer is re-plumbed to draw into a tile `Rect` rather than owning the full frame. Three-state lifecycle, navigation, filter unchanged.

**Acceptance tests.**
- Headless render test via `TestBackend`: default layout renders with pty tile, docker tile, three placeholder tiles, all at expected rects for a 120×40 terminal.
- State-machine tests for focus movement, focus-respecting input routing, resize re-layout.
- `vt100` integration test: a pty that emits cursor positioning + alt-screen entry renders correctly within a 40×20 tile `Rect` without smearing into neighbors.
- Manual demo (gated by CTO/user): `tepegoz tui` → god view visible, focus navigation feels right, Docker tile (real) and placeholder tiles (labeled) coexist cleanly, vim in the pty tile preserves correctly across focus moves and detach/reattach.

**Non-goals.** Layout configurability. Resizable splits. Tile show/hide. User-defined keybinds for focus. All deferred past v1.

**Risks.** `vt100` integration edge cases (wide chars, emoji, alt-screen stacking). Budget for a character-width bug.

##### C1.5a — Docs-only commit

- Decision #7 added to `docs/DECISIONS.md`.
- This Slice C1.5 section inserted in `docs/ROADMAP.md`; C1 / C2 banners note what survives.
- `docs/ARCHITECTURE.md` TUI section updated to reflect tile layout + vt100 integration + scope-renderer `(state, Frame, Rect)` contract.
- `docs/STATUS.md` updated: C1.5 in progress; C2 renderer re-plumbing is C1.5b scope; bus fixes + daemon fixes survive.
- README.md: mockup unchanged (already the correct spec); first-run-layout note added beneath it.

**Gate.** CTO reads Decision #7 + this roadmap slice in tree. No C1.5b code until sign-off.

##### C1.5b — Tiling-foundation code

Per plan above. Starts after C1.5a sign-off. Ping when green on both-OS CI and pushed.

##### C1.5c — Manual demo gate

User (Emin) runs `tepegoz tui` in a real terminal and confirms: default god-view layout renders on first launch with no config — PTY top, Docker bottom-left, placeholder tiles bottom-middle/right/wide-strip; focus navigation (`Ctrl-b h/j/k/l` + arrows) feels natural; focused tile is visually distinct; pty tile works (`vim /tmp/foo`, type text, focus away + back, vim screen intact); Docker tile populates within ~2 s with live container list; navigation + filter work inside the Docker tile while pty tile continues updating; placeholder tiles are clearly labeled and non-interactive (no crashes on focus); detach + reattach preserves all state (Phase 2 invariant). No C3 code starts until user signs off.

#### Slice C2 — Docker scope rendering + subscription lifecycle · 🟠 (gate landed)

##### C2 gate (first commit) — vim-preservation gate + daemon Unsubscribe fix · ✅ (daemon fix survives; vim-preservation rationale repurposed)

**C1.5 salvage.** Daemon `pane_subs: HashMap<u64, AbortHandle>` fix (`43b28eb`) is decoupled from TUI shape and survives untouched. `pane_unsubscribe.rs` stays as the daemon-side regression test. `vim_preservation.rs`'s original rationale (verifying the synthetic re-attach preserves vim state) goes away with the mode-switch model; in C1.5b the file is repurposed as a `vt100` reconstruction test — same pty harness, same marker emission, but the assertion becomes "the vt100 screen buffer contains the marker at the expected cell after feeding the pty bytes through the parser."

**Delivered.**
- **Bug fix:** Through Slice C1, daemon's `pane_subs` was `JoinSet<()>` with no per-id key — `Payload::Unsubscribe { id }` only touched `status_sub` and `docker_subs`, so the C1 TUI's synthetic re-attach was leaking one zombie pane forwarder per Scope→Pane mode switch (daemon CPU + writer-mpsc bandwidth burnt indefinitely; pane bytes sent over the socket twice). Refactored `pane_subs` to `HashMap<u64, AbortHandle>` mirroring `docker_subs`, wired `Unsubscribe` to cancel pane forwarders, and made `AttachPane` on an existing sub_id replace + abort the previous (defensive). On session end, both maps drain + abort.
- **Regression test** `crates/tepegoz-core/tests/pane_unsubscribe.rs` — pins the invariant: after `Unsubscribe(sub_1)`, no further envelopes arrive with `subscription_id == sub_1`. New input is observable on the new sub.
- **Vim-preservation byte-level proxy** `crates/tepegoz-core/tests/vim_preservation.rs` — drives a real `/bin/sh` pane, emits vim-style escape sequences (alt-screen entry `ESC[?1049h`, cursor positioning `ESC[5;10H`, marker text) via `printf`, then exercises the C1 synthetic re-attach pattern (Unsubscribe(sub_1) + AttachPane(sub_2)) and asserts the new `PaneSnapshot` contains all three byte markers. **This is the strongest automated proxy for the vim demo; eyeball confirmation in a real terminal is still required** before C2 commit 2 (rendering work) lands. Per CTO §3, fallback options if eyeball reveals problems are documented at `app.rs::switch_to_pane`.

**NOT yet done — C2 commit 2 (rendering) is unblocked but still pending:**
- Container table widget, three-state lifecycle visuals, navigation, filter (see C2 commit 2 scope below).
- The 3 small test gaps (per CTO C2 first-commit list): `second_switch_to_pane_is_idempotent`, `help_in_pane_mode_is_dropped`, and the new `AppAction::ShowToast` variant for `Payload::Error` + `DockerActionResult::Failure` routing.

##### C2 commit 2 — rendering work · ✅ (content survives; re-plumbed to a tile `Rect` in C1.5b)

**C1.5 salvage.** `DockerScope` state struct (Idle/Connecting/Available/Unavailable), three-state lifecycle rendering, navigation (j/k/arrows/g/G/Home/End), filter (`/` activate, `Enter` commit, `Esc` clear, `Backspace` edit, case-insensitive substring over name + image), selection clamping, `AppAction::ShowToast { kind, message }` wire, and `Payload::Error` / `DockerActionResult::Failure` routing to `ToastKind::Error` all survive. The renderer signature changes from owning the full `Frame` to `(state, Frame, Rect)`; the 7 headless render cases adjust to draw into a sub-`Rect` rather than the full frame. The mode-switch-specific App tests (`switch_to_scope` / `switch_to_pane` allocation, `pane_output_in_scope_mode_is_dropped` in its current framing, the idempotent-double-switch pair) are replaced in C1.5b with focus-navigation equivalents; the Docker subscription-lifecycle tests, ShowToast-routing tests, and CTO test-gap cases all survive.

**Delivered.**
- `tepegoz-tui/src/app.rs` rewritten:
  - `switch_to_scope` allocates a docker sub_id, sends `Subscribe(Docker)`, sets state to `Connecting` (immediate visual feedback — no blank-spinner window).
  - `switch_to_pane` sends `Unsubscribe(docker_sub_id)`, resets docker state to `Idle`, clears filter + selection, then the existing synthetic pane re-attach.
  - `handle_docker_event`: `ContainerList` → `Available { containers, engine_source }` + `clamp_selection`; `DockerUnavailable { reason }` → `Unavailable { reason }`; `DockerStreamEnded` → no-op (C3 logs/stats consumer).
  - `AppAction::ShowToast { kind: Info|Success|Error, message }` variant. `handle_daemon_envelope` routes `Payload::Error` and `DockerActionResult::Failure` to `ToastKind::Error`; `Success` deferred to C3.
  - `ScopeKeyParser` state machine parses ESC CSI sequences (arrows, `Home`, `End`) plus `ESC ESC` → standalone `Escape`; flushes pending lone `ESC` at end of chunk (terminal reads deliver full `ESC [ A` in one go). Navigation: ↑↓ / `j` `k` / `g` `G` / `Home` `End`. Filter: `/` activates input, typed chars append, `Backspace` trims, `Enter` commits (keeps filter), `Esc` clears (both filter text and active mode). Filter matches name + image substring, case-insensitive.
  - `DockerScope::{ matches_filter, visible_count, clamp_selection }` helpers so the renderer doesn't duplicate logic.
- `tepegoz-tui/src/scope/docker.rs` rewritten as the real renderer:
  - Three-state lifecycle with distinct visuals (Connecting is yellow; Available is green-status with `▶` selection marker; Unavailable is red-bordered with verbatim reason).
  - Empty-list state (Available but 0 visible) renders "No containers" or "No containers match filter" — explicitly distinct from Unavailable.
  - Filter bar (top) with `filter: <text>_` caret when active.
  - Help bar (bottom) context-aware (different hints when filter is active vs browsing).
  - Status bar shows `visible/total container(s)` + engine source + filter note.
  - Port column shows public mappings first, truncates to 3 + "+N" overflow.
- `tepegoz-tui/src/session.rs`: `AppAction::ShowToast` stubbed as `tracing::warn!`/`info!` depending on severity (C3 implements the actual overlay).

**Acceptance tests.**
- 27 `tepegoz-tui::app::tests` (up from 14) including the 3 CTO-requested gaps (`second_switch_to_pane_is_idempotent`, `ctrl_b_question_in_pane_mode_is_dropped`, and the `DockerActionResult::Success does not toast yet` + `Payload::Error routes to ShowToast` pair for the ShowToast wire) plus navigation (j/k/arrows/g/G/Home/End), filter (narrow/commit/clear/backspace), and Docker subscription lifecycle (subscribe-on-enter, Unsubscribe-on-leave, state transitions).
- 7 `tepegoz-tui::scope::docker::tests` headless render tests using `ratatui::backend::TestBackend(120×30)`: Available state renders container table with names/images/states + `▶` marker; Connecting message; Unavailable with verbatim reason; Available-but-empty shows distinct "No containers"; filter matching nothing; filter-bar caret when active; ports column renders public + internal mappings (uses 180-wide TestBackend for the port test since port strings overflow 120 cols after the fixed NAME/IMAGE/STATUS columns consume their share).

##### C2 commit 3 — end-to-end test + eyeball demo · 🟡 (latency pin survives; eyeball demo replaced by C1.5c)

**C1.5 salvage.** The `crates/tepegoz-core/tests/docker_scope.rs::docker_scope_lists_provisioned_container_within_2s` latency pin is daemon-side and unaffected — stays green unchanged. The eyeball demo originally gated on C2c3 (vim-preservation across Scope→Pane re-attach + CTO §7 engine-unavailable-mid-session) is moot: the synthetic re-attach goes away, and the new user-facing gate is C1.5c (god-view rendering + focus nav + vim-in-pty-tile + tile-coexistence + detach/reattach). The CTO §7 engine-unavailable check moves into C1.5c since the Docker tile is always-subscribed in the tiled layout.

**Delivered (automated).**
- `crates/tepegoz-core/tests/docker_scope.rs::docker_scope_lists_provisioned_container_within_2s` — opt-in `TEPEGOZ_DOCKER_TEST=1`. Provisions a unique-per-PID `alpine:latest` container (`sleep 120`), subscribes to Docker, asserts the first `ContainerList` arrives in <2 s *and* contains the provisioned container by name. Force-removes on `Drop` so panics don't leak. The 2-second threshold pins the "feels responsive on Ctrl-b s" UX contract; a slip there would make scope view feel broken.
- **Navigation / filter** assertions kept at the App-state-machine layer (`tepegoz-tui::app::tests`) rather than duplicated against a real daemon. The behavior under test is the App's — an end-to-end version wouldn't catch anything more than the existing unit tests, and would inflate CI time. Listed here for transparency: arrows / j / k / g / G / Home / End navigation, filter narrow / commit / clear / backspace, subscription lifecycle (subscribe on enter, unsubscribe on leave).

**Eyeball demo — pending user run.** The CI automation cannot drive an interactive TUI. The manual demo is the first real-terminal check for:
1. **Vim-preservation across Scope→Pane synthetic re-attach** (Step 1 in `docs/OPERATIONS.md`). If this fails, apply the fallback mitigation from `docs/ISSUES.md` before proceeding with C3.
2. Scope rendering / navigation / filter (Step 2).
3. **CTO §7 Step 10**: kill docker daemon mid-session, verify scope view transitions to Unavailable within ~5 s without crashing the TUI; restart docker, verify scope view recovers.
4. Detach + reattach (Phase 2 invariant, Step 4).

Full script in `docs/OPERATIONS.md` "Slice C manual demo prep".

**Acceptance tests.**
- Headless render test using `ratatui::backend::TestBackend(120, 30)`: build an `App`, populate `DockerScope::state` with three fake containers, drive `DrawScope`, assert names/states/ports appear in the rendered buffer at the expected cell positions, including the selected-row highlight.
- Add to `crates/tepegoz-core/tests/docker_scope.rs` a TUI-driver test that spawns the daemon, runs a scripted `App` (no terminal) through "subscribe → receive ContainerList → press r → receive DockerActionResult". Bypasses crossterm but exercises the entire wire path.
- **Manual demo (per CTO §7 sign-off, including new Step 10):** start daemon + TUI; switch to scope (`Ctrl-b s`); see container table; navigate (j/k); filter (`/`); switch back to pane (`Ctrl-b a`); verify vim-preservation; detach + reattach (`Ctrl-b d`, `tepegoz tui`); **kill the docker daemon, verify scope view transitions to Unavailable within ~5 s without crashing the TUI; restart docker, verify scope view recovers**. Standing victim-container snippet in `docs/OPERATIONS.md`.

#### Slice C3 — Action keybinds + toasts + logs panel · ⚪

**Scope.**
- Scope view keybinds (per CTO §4 sign-off):
  - `r` restart (immediate; recoverable)
  - `s` stop (immediate; recoverable)
  - `K` (capital) kill — **requires `y` confirmation modal** (n/Esc/anything-else cancels)
  - `X` (capital) remove — **requires `y` confirmation modal**
  - `l` open logs panel (`Subscribe(DockerLogs)` for selected container)
  - `Enter` exec into container (Slice D — opens new pane)
  - `?` help overlay
- Action results render as toasts (overlay near bottom of scope view) — "✓ Restarted nginx" / "✗ Stop failed: container not running".
- **Pending-action timeout** (per CTO §5 sign-off): each in-flight `DockerAction` carries a `deadline: Instant`; a 30 s sweep timer in the runtime emits `AppEvent::PendingActionTimeout(request_id)` for any expired entry; App emits a "Action timed out — check engine" toast. Cheap to add now; expensive to retrofit.

**Acceptance tests.**
- App state-machine tests: `r`/`s`/`K`/`X` emit the right `DockerAction` envelope (with the right `kind`); destructive actions (`K`/`X`) require `y` confirm before emitting; `n`/`Esc`/other key cancels. Pending-action timeout fires the toast.
- End-to-end test: spawn daemon, drive scripted App through Restart of a real container (opt-in via `TEPEGOZ_DOCKER_TEST=1`).
- **Manual demo (Slice C overall acceptance — see §7 in the C1 commit history for the full sequence with Step 10).**

### Slice D — `DockerExec` → new pty pane · ⚪

**Scope.**
- Command: `DockerExec { container_id, cmd, env, rows, cols }`. Daemon spawns a docker exec session, wraps it as a `Pane` in `PtyManager`, returns `PaneOpened(PaneInfo)`. From the client's perspective it looks identical to opening a local shell pane.
- TUI's `RequestOpenPane(PaneRequestKind::DockerExec { ... })` (the C1 placeholder variant) gets wired: `Enter` in scope view sends the command, awaits `PaneOpened`, then transitions `View → Pane(new_pane_id)`.

**Acceptance.** Provision a known container; exec into it; send `pwd\n`; verify expected output in pane scrollback.

### Slice D — `DockerExec` → new pty pane · ⚪

**Scope.**
- Command: `DockerExec { container_id, cmd, env, rows, cols }`. Daemon spawns a docker exec session, wraps it as a `Pane` in `PtyManager`, returns `PaneOpened(PaneInfo)`. From the client's perspective it looks identical to opening a local shell pane.

**Acceptance.** Provision a known container; exec into it; send `pwd\n`; verify expected output in pane scrollback.

**Not in scope (Phase 3 overall).** Docker Compose, swarm, multi-host. Cross-container networking visualization (Phase 4+).

**Risks.** Socket discovery across Docker runtimes was the main engineering risk; Slice A's structured-error connect with a transparent reason field shoulders that. Logs and exec streaming may surface backpressure scenarios that didn't appear in pty work — broadcast capacity may need tuning per-subscription kind.

---

## Phase 4 — Ports + processes panels (local) · ⚪

**Goal.** Two more scope panels backed by native per-OS probes.

**Scope.**
- `tepegoz-probe`: `trait Probe` + per-OS implementations:
  - **Linux**: `procfs` for processes; netlink `NETLINK_SOCK_DIAG` for listening sockets (no parsing overhead).
  - **macOS**: `libproc-rs` for processes (`PROC_ALL_PIDS` + `PROC_PIDTASKALLINFO`); `libproc` `PROC_PIDFDSOCKETINFO` per pid for sockets.
  - Cross-OS fallback via `sysinfo` for CPU/mem/disk.
- Wire protocol: `Subscribe(Ports)`, `Subscribe(Processes)`; events for list + deltas.
- TUI panels: tabular views with sort, filter (by port, pid, process name), signal actions on processes.

**Acceptance.** Start a known process and bind a known port; see both in the panels. Kill the process; see it disappear.

**Not in scope.** Remote probes (that's Phase 6 with the agent).

**Risks.** netlink is fast but unfamiliar; libproc is older and less documented. Keep `sysinfo` as fallback.

---

## Phase 5 — SSH transport + remote pty · ⚪

**Goal.** `tepegoz connect user@host` opens a remote pty pane — same UX as local.

**Scope.**
- `tepegoz-ssh`: `russh` client with channel multiplexing. Host key TOFU (trust on first use) with warn on change.
- Wire protocol extension: pty commands/events gain a `host` qualifier (or the daemon tracks per-connection host association).
- Daemon: per-host connection pool; persistent channel carries protocol frames.
- TUI: host:pane identification in the UI.

**Acceptance.** Integration test with a test SSH server (via testcontainers or similar): open remote pane, send input, read output, disconnect, reattach, verify scrollback.

**Not in scope.** QUIC (Phase 10). Multi-host agent coordination (Phase 6).

**Risks.** Pure-SSH latency may feel sluggish for live telemetry; QUIC in Phase 10 is the relief valve. Acceptable for Phase 5 since the killer app here is remote pty.

---

## Phase 6 — Agent binary + remote scope panels · ⚪

**Goal.** Deploy a lightweight agent to remote hosts; the same scope panels work against remote as against local.

**Scope.**
- `tepegoz-agent` subcommand runs a stdio-framed protocol server. Targets: static musl Linux (x86_64 + aarch64), universal macOS. <5 MB per target.
- `xtask build-agents` cross-compiles all four targets into `target/agents/`.
- Controller `build.rs` reads `target/agents/` and `include_bytes!`s each arch.
- Daemon: detect remote OS + arch over SSH → `scp` the matching agent binary to `~/.cache/tepegoz/agent-<version>` → verify SHA256 → exec over SSH with stdio carrying the protocol.
- Remote scope panels: Docker, Ports, Processes — same wire protocol, agent-backed.

**Acceptance.** Full fleet test: deploy agent to a test VM via SSH, open a remote pane, verify docker panel works against remote docker, verify port scan finds a known open port on remote host.

**Not in scope.** Agent auto-update (agents are redeployed per controller version). Multi-user agents.

**Risks.** Cross-compiling the agent for 4 targets is real work. Protocol/library version drift between controller and embedded agents must be caught by CI.

---

## Phase 7 — Port scanner · ⚪

**Goal.** Port scanning as a first-class capability. TCP-connect in v1; SYN deferred to v1.1 (Linux first).

**Scope.**
- `tepegoz-scan`: port existing `pscan` tool logic into the crate. TCP-connect scan via `socket2` (SO_LINGER zero-timeout + RST close to skip TIME_WAIT on localhost sweeps); bounded concurrent fanout via tokio semaphore (default ~500).
- Wire protocol: `ScanTargets { targets, port_range, concurrency }` command; `ScanResult { target, port, state, banner }` event.
- Same code runs locally or on an agent host.

**Acceptance.** Scan `127.0.0.1` with a known listener on a known port; find it. Scan a known-dead port; see it closed.

**Not in scope.** SYN scan (v1.1 Linux-first; macOS BPF is a separate effort).

**Risks.** Some networks rate-limit outbound connections; default concurrency may need tuning.

---

## Phase 8 — Recording + replay · ⚪

**Goal.** Every pane keystroke/output is recorded, encrypted at rest, replayable offline.

**Scope.**
- `tepegoz-record`: per-pane append-only log at `~/.local/share/tepegoz/recordings/<pane-id>/<ts>.tpgr` (macOS equivalent under `~/Library/Application Support/...`).
- Encryption: `age` wrapper. Per-session key, wrapped with a root key from OS keychain (`keyring` crate), with fallback `TEPEGOZ_ROOT_KEY` inline env or `TEPEGOZ_ROOT_KEY_FILE=/path/` per `docs/DECISIONS.md#2`.
- Precedence: env > file > keychain. When any override is set, the daemon does **not** write back to the keychain.
- `tepegoz replay <pane-id>` subcommand: time-scrubbing playback with speed control + regex search.

**Acceptance.** Run a pane, produce known output, close pane. Replay; bytes match original.

**Not in scope.** Sharing recordings across machines. Multi-user access control.

**Risks.** Encryption/decryption throughput on a high-traffic pane — benchmark and tune.

---

## Phase 9 — Claude Code pane awareness · ⚪

**Goal.** Parse `~/.claude/projects/` state to augment pty pane metadata. TUI status line shows `● claude: editing foo.rs (42s)` without interrupting the agent.

**Scope.**
- Structural signature of `~/.claude/projects/` layout (set of dirnames + top-level JSON fields), compared against known-tested signatures baked into the binary.
- On unknown signature: yellow status notice, feature disabled; `tepegoz doctor --claude-layout` dumps the detected signature for bug reports. **Never crash the daemon.**
- Pure observation — no LLM calls.

**Acceptance.** Start 3 Claude Code sessions in distinct pty panes; status line for each correctly reflects current tool use and file touched.

**Not in scope.** Interaction with Claude sessions (that's v3).

**Risks.** Claude Code's state layout changes — structural signature tolerates benign additions; hard breakage requires a new signature.

---

## Phase 10 — QUIC hot path + release 0.1.0 · ⚪

**Goal.** Ship.

**Scope.**
- QUIC via `quinn` over SSH port-forward for hot-path streams (logs, high-volume pane output). Roaming survives wifi flap in ms.
- Release pipeline in `xtask package`:
  - Cross-compiled binaries (mac x86_64/arm64, linux x86_64/aarch64 musl).
  - SHA256 + **minisign signatures** on every artifact (checksums catch corruption; signatures catch tampering).
  - Homebrew tap `emincb/tap/tepegoz`.
  - `curl | sh` install script.
  - Optional `cargo install tepegoz` via crates.io.
- Agent embedding: `include_bytes!` all four arches into controller (~15 MB total), per `docs/DECISIONS.md#3`.
- Size/perf tuning: `opt-level="z"`, LTO, strip, feature-gate. Target controller <20 MB, agent <5 MB.

**Acceptance.** Download binary from GH releases, run it, full fleet demo works. Minisign signature validates against the published pubkey.

**Not in scope.** Team features. Web UI (v2). AI (v3). Auto-update.

**Risks.** minisign key management — lose the signing key and future releases can't be signed without rotation. Publish the pubkey in README + tap formula + docs.

---

## After v1 (not in this document's scope)

- **v2**: phone/web client over WSS + mTLS speaking the same wire protocol.
- **v3**: AI "god query" orchestrator layered on the v1 event stream.
