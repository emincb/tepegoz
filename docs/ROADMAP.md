# Tepegöz v1 roadmap

Target: **0.1.0 release** at end of Phase 10. Rough budget: 15–20 weeks full-time.

Status key: ✅ complete · 🟡 code+tests green, user acceptance pending · 🟠 in progress · ⚪ not started · 🔵 deferred to a future release.

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

## Phase 3 — Docker scope panel · ✅ (2026-04-14)

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

#### Slice C1.5 — Tiling foundation · 🟡 (C1.5a + C1.5b landed; C1.5c manual demo pending user)

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

##### C1.5b — Tiling-foundation code · ✅

**Delivered.**
- `vt100` 0.16 added to `tepegoz-tui` deps; also as dev-dep to `tepegoz-core` for the repurposed vim_preservation test.
- New `tile` module: `TileId { Pty, Docker, Ports, Fleet, ClaudeCode, TooSmall }`, `TileKind { Pty, Scope(ScopeKind), Placeholder { label, eta_phase }, TooSmall }`, `FocusDir`, `TileLayout` with `default_for(area)` producing the god-view Rect arrangement + `MIN_COLS × MIN_ROWS` (80×24) tiny-terminal fallback. Spatial adjacency via `next_focus(from, dir)` with left-align tiebreak so `Ctrl-b j` from full-width PTY lands on Docker (live) rather than Ports (placeholder).
- Rewritten `app.rs`: `View { layout, focused }`; `pty_parser: vt100::Parser` sized to the pty tile; `pane_sub` + `docker.sub_id` both stable u64s allocated once in `App::new` (no more Option-nullable transitions); `initial_actions` emits AttachPane + ResizePane (pty-tile dims, NOT terminal dims) + Subscribe(Docker) + DrawFrame; `handle_forward_bytes` routes by focused tile (Pty → SendInput; Scope(Docker) → scope key parser; Placeholder/TooSmall → drop); `handle_focus_direction` moves focus via `layout.next_focus`; `handle_resize` recomputes layout + resizes vt100 parser + sends ResizePane with new pty-tile dims. Docker opens at `DockerScopeState::Connecting` (Subscribe is already in-flight); `Idle` variant kept for completeness but unreachable. Deleted: `View::{Pane, Scope}`, `switch_to_scope`/`switch_to_pane`, the synthetic re-attach, `AppAction::{EnterPaneMode, EnterScopeMode, WriteStdout}`, `InputAction::{SwitchToScope, SwitchToPane}`. Renamed: `AppAction::DrawScope` → `DrawFrame`. Added: `AppAction::FocusTile(TileId)` (observational; runtime logs at debug).
- Rewritten `input.rs`: `InputAction::FocusDirection(FocusDir)` replaces `SwitchToScope`/`SwitchToPane`. State-machine filter recognizes `Ctrl-b` + `h/j/k/l` AND `Ctrl-b` + CSI arrow sequences (`ESC [ A/B/C/D`), with split-across-chunks resilience. `Ctrl-b ESC X` where `X` isn't `[` forwards the raw bytes rather than swallowing the ESC.
- New `pty_tile.rs`: `render(parser, frame, area, focused)` projects `vt100::Screen` cells into ratatui, translating fg/bg/bold/italic/underline/reverse attrs. Cursor rendered as reversed cell only when focused (unfocused tiles show buffer without a misleading caret). Bordered block with focus-aware style (bright cyan when focused; dim gray otherwise).
- New `scope::placeholder::render(label, eta_phase, frame, area, focused)`: bordered block with dimmed border, centered label ("Ports — Phase 4" style), + focused "Phase N — not yet implemented" hint.
- Re-plumbed `scope::docker::render(state, frame, area, focused)`: content preserved from C2c2 (three-state lifecycle, `▶` selection marker, filter bar, help bar, port column formatting), signature adjusted to draw into a sub-`Rect`. Help bar updated to reference `Ctrl-b h/j/k/l` focus keys. Focus-aware border color.
- Rewritten `session.rs` runtime: always-on ratatui (no mode gating); 30 Hz tick always active; `render_tiles(app, frame)` walks the tile layout and dispatches per `TileKind`; single-tile fallback for the too-small layout.
- Repurposed `crates/tepegoz-core/tests/vim_preservation.rs` as a vt100 reconstruction test: spawns daemon, opens `/bin/sh`, shell emits alt-screen entry + cursor positioning + marker via printf, accumulated PaneSnapshot/PaneOutput bytes are fed to a `vt100::Parser`, assertion is `parser.screen().cell(row, col)` contains the marker at the expected position plus the full marker string reads correctly across a row substring. This is the automated proxy for "vim will render correctly inside the pty tile."
- Kept unchanged: daemon-side `pane_subs` HashMap fix (`43b28eb`), handshake version-mismatch guard (`e4d2113`), printf hotfix (`56a8a4f`), C2c3 latency-pin test, `pane_unsubscribe.rs`. All decoupled from TUI shape.

**Acceptance tests.** 114 workspace-wide (+34 from C2c3): `tile` 13, `input` 22 (up from 12), `app` 29 (up from 27), `pty_tile` 3 new, `scope::placeholder` 3 new, `scope::docker` 8 (up from 7), `vim_preservation` 1 (repurposed). `cargo fmt --all` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean.

**Gate.** CI green on macOS + ubuntu-latest. Then user runs C1.5c.

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

#### Slice C3 — Action keybinds + toasts + logs panel · ✅ (`8a9176c`, `fc5ded4`, `4dd1208`, _close commit_)

Lands as three sub-commits per CTO sign-off: **C3a** (actions + confirm modal + toasts + timeout sweep), **C3b** (logs panel sub-state inside the Docker tile + C3a style-cleanup: R/S aliases removed + K/X absorption behavior), **C3c** (end-to-end Restart round-trip test + 9-scenario manual demo script).

**C3a scope (delivered).**
- `r` restart (immediate; recoverable) and `s` stop (immediate; recoverable) dispatch `DockerAction` against the selected container on the focused Docker tile. Both also bind `R` / `S` so caps-lock doesn't silently steal the action.
- `K` (capital) kill and `X` (capital) remove enter an inline confirm modal inside the Docker tile's `Rect` (not full-screen — per C3a UX clarification #3). `y`/`Y` confirms + dispatches; any other key cancels. Focus moving away from the Docker tile cancels. 10 s idle auto-cancel.
- Toast overlay renders as a 1-line-per-toast strip directly above the Claude Code tile (per C3a UX clarification #2). Max 3 visible; a 4th arrival drops the oldest silently. Auto-dismiss: Success ~3 s, Error ~8 s, Info ~4 s. Never blocks keystrokes.
- Pending-action 30 s timeout sweep runs on every Tick: expired entries emit an "`<verb> <name>` timed out — check engine" error toast. The `AppEvent::PendingActionTimeout(id)` wire is kept on the input surface so a future dedicated sweeper (timer wheel) can feed it without reshaping the event API.
- `DockerActionResult::Success` emits a green "`<verb> <name>` — succeeded" toast matched against the pending action description; `Failure { reason }` emits a red "`<verb> <name>` failed: `<reason>`" toast. Stale results (no matching pending action) fall back to `<verb> <container_id>` so the user still sees the outcome.
- `Payload::Error` from the daemon also lands in the toast overlay queue (previously logged only).

**C3b scope (delivered).** Lands as one commit starting with a head cleanup per CTO push-back on C3a, then the logs-panel body. Head cleanup: R/S aliases removed (lowercase-only `r`/`s`, matching the case-discipline rule — caps = destructive, lowercase = safe/navigation); K/X during a pending confirm now *absorb* rather than cancel (protects users from accidentally switching the modal's target mid-prompt); 10 s auto-cancel test strengthened to also assert no `DockerAction` leaks on silent expiry; new test pins capital `R` as a no-op. Body: `DockerScope.view: DockerView::{List, Logs(LogsView)}` enum; `l` on list with a selected container allocates a fresh sub id, sends `Subscribe(DockerLogs { id, container_id, follow: true, tail_lines: 0 })`, and transitions to `Logs(LogsView { container_id, container_name, sub_id, lines: VecDeque<LogLine>, pending_stdout/_stderr: Vec<u8>, scroll_offset, at_tail, stream_ended })`. `LogsView::ingest(stream, data)` appends to per-stream pending buffer and flushes complete `\n`-terminated lines into the capped `VecDeque` (`MAX_LOG_LINES = 10_000`, drop-oldest on overflow); CRLF is stripped. `DockerStreamEnded { reason }` flushes pending bytes as a final line, records the reason, and disables `at_tail`; renderer paints a dimmed "— log stream ended: `<reason>` —" line. Scroll keys: `j`/`k`/Down/Up by 1; PgUp/PgDn by `LOGS_PAGE_LINES = 10`; `G`/End/Bottom jump-to-tail + re-enable auto-follow; scrolling up disables `at_tail`; reaching offset 0 via scroll-down re-enables it. `Esc`/`q` Unsubscribe and return to List. Logs view persists across focus moves; action keybinds (`r`/`s`/`K`/`X`/`l`/filter) are all ignored while logs are showing (read-only transcript). Stale events on an unsubscribed logs sub drop silently via `DockerScope::is_current_logs_sub`.

**C3c scope (pending).** End-to-end integration test in `crates/tepegoz-core/tests/`: provision alpine container, drive `Restart` through the App + wire, assert `DockerActionResult::Success` arrives and the follow-up `ContainerList` reflects the change. Opt-in `TEPEGOZ_DOCKER_TEST=1`. Plus a manual-demo script addition in `docs/OPERATIONS.md` for the user's eyeball check.

**C3a acceptance tests (143 workspace-wide, +29 from C1.5b's 114).**
- `tepegoz-tui::app::tests` 51 (up from 29): all C3a state transitions — r/s immediate dispatch, K/X enter confirm, y/n/Esc/random-char/focus-away cancel paths, 10 s confirm timeout, 30 s action timeout toast, `PendingActionTimeout(id)` event expiry, unknown-id no-op, Success/Failure toasts with description, fallback description for stale results, no-op when PTY focused, no-op when Docker Unavailable, no-op when list empty, toast overflow drop-oldest, toast sweep per-kind cadence, daemon error lands in toast queue.
- `tepegoz-tui::scope::docker::tests` 11 (up from 8): confirm modal renders with container name + prompt; confirm absent when no pending; help bar shows action keybinds in idle state.
- `tepegoz-tui::toast::tests` 5 new: empty list paints nothing; single Error toast lands directly above Claude Code strip; three toasts stack on three lines; cap at `MAX_TOASTS` drops oldest; too-small fallback layout no-ops rather than panicking.
- Combined: `cargo fmt --all` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` 143 passed / 0 failed.

**C3b acceptance tests (164 workspace-wide, +21 from C3a's 143).**
- `tepegoz-tui::app::tests` 69 (up from 51): C3b-head strengthens + adds the R/S deletion + K/X absorption tests (10 s auto-cancel asserts no DockerAction dispatched; second K during Kill pending is absorbed; X during Kill pending is absorbed; capital R is a no-op); C3b-body adds the logs state-machine suite (`l` enters logs + subscribes; `l` no-op without selection; `l` no-op when Docker Unavailable; Esc / q Unsubscribe + return to List; ContainerLog chunks assemble on `\n`; CRLF strips both bytes; stdout/stderr pending stay separate under interleave; j/k/PgUp/PgDn move scroll + toggle at_tail; G jumps to tail + re-enables at_tail; DockerStreamEnded flushes + sets marker + disables at_tail; MAX_LOG_LINES drops oldest; stale events post-Unsubscribe drop silently; action keys ignored in logs view; focus-away does not cancel logs view).
- `tepegoz-tui::scope::docker::tests` 14 (up from 11): logs view renders status + transcript + logs-mode help bar; stream-ended marker renders at the tail with the reason; confirm modal is suppressed while logs view is active.
- Combined: `cargo fmt --all` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` 164 passed / 0 failed.

**C3c acceptance tests (delivered).**
- `crates/tepegoz-core/tests/docker_scope.rs::restart_propagates_to_follow_up_container_list` — opt-in `TEPEGOZ_DOCKER_TEST=1`. Provisions a unique-per-PID alpine container, subscribes to Docker, snapshots pre-restart `state`/`status`, sleeps 2 s to let "Up N seconds" advance so the post-restart reset is observably different, sends `DockerAction::Restart` with a known `request_id`, asserts matching `DockerActionResult::Success` (panics with the engine's reason on `Failure`), then asserts a subsequent `ContainerList` (post-Success only — pre-Success lists are "pre" and any shift there is spurious) shows `state != pre_state || status != pre_status`. Force-removes on `Drop` so panics don't leak. Verified locally against Docker Desktop (~6 s run time). This is the full round-trip pin: client → daemon socket → DockerAction → engine → DockerActionResult::Success → next daemon poll → ContainerList reflects the restart. If the daemon's `Subscribe(Docker)` poller didn't repoll after an action, or if `request_id` correlation broke, this test fails where the unit tests pass.
- **Manual demo** in `docs/OPERATIONS.md` "Slice C3 manual demo prep": 9 scenarios with a pass/fail matrix covering `r`/`s` dispatch + Success toast, capital-`R` no-op (case-discipline lock), `K`/`X` confirm flow including K→K absorption, Failure toast verbatim reason, 30 s pending-action timeout, toast stacking + drop-oldest, logs panel entry/tail/scroll/exit, `DockerStreamEnded` marker, and a tile-sized logs sanity check. Scenarios 1–8 gate Phase 3 close; scenario 9 is observational (any gotchas → `docs/ISSUES.md` as Phase-3-polish, does NOT block Phase 3 close).
- Combined: `cargo fmt --all` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` 165 passed / 0 failed.

**Gate.** CI green on macOS + ubuntu-latest, then CTO reviews the integration-test shape + demo script. User runs the manual demo in a real terminal with the pass/fail matrix. If scenarios 1–8 sign off, Phase 3 row in `docs/STATUS.md` goes ✅.

### Slice D — `DockerExec` → new pty pane · 🔵 Deferred to v1.1

**Deferred to v1.1.** Docker's exec API (bollard inherits from the engine API) ends the exec session when the hijacked connection closes — there is no server-side "detach and keep running." This means a `DockerExec` pane cannot preserve Phase 2's detach/reattach invariant without a custom in-container agent, which is out of scope for v1.

The "scope view triggers new pane" pattern also generalizes to Phase 5 (SSH Fleet → open remote pty) and Phase 6 (remote Docker → exec into remote container). Designing the mechanism for DockerExec in isolation would lock in a shape that may not fit those phases. Revisit after Phase 5's concrete requirements force the generalized design.

Users retain `docker exec -it <container> sh` in their local pty tile as the v1 escape hatch.

**Original scope (retained for v1.1 reference).**
- Command: `DockerExec { container_id, cmd, env, rows, cols }`. Daemon spawns a docker exec session, wraps it as a `Pane` in `PtyManager`, returns `PaneOpened(PaneInfo)`. From the client's perspective it looks identical to opening a local shell pane.
- TUI's `RequestOpenPane(PaneRequestKind::DockerExec { ... })` (the C1 placeholder variant) gets wired: `Enter` in scope view sends the command, awaits `PaneOpened`, then opens the new pane inside whatever multi-pane model v1.1 picks (background-stash vs. tab-strip — the design pass that was deferred with this slice).

**Original acceptance (v1.1).** Provision a known container; exec into it; send `pwd\n`; verify expected output in pane scrollback.

**Not in scope (Phase 3 overall).** Docker Compose, swarm, multi-host. Cross-container networking visualization (Phase 4+).

**Risks.** Socket discovery across Docker runtimes was the main engineering risk; Slice A's structured-error connect with a transparent reason field shoulders that. Logs and exec streaming may surface backpressure scenarios that didn't appear in pty work — broadcast capacity may need tuning per-subscription kind.

---

## Phase 4 — Ports + processes panels (local) · ✅ (2026-04-14)

Proposal pass signed off 2026-04-14: (Q1) Processes lives as a toggle-mode sub-state within the Ports tile (lowercase `p` toggles between Ports and Processes views) rather than a new Decision #7 tile — solves the "processes without a bound port" flow while respecting the god-view layout; (Q2) probe uses the cross-OS `netstat2` wrapper (procfs on Linux, libproc on macOS) plus `sysinfo` for pid → process name; (Q3) daemon-side correlation so clients stay dumb; (Q4) 2 s refresh cadence matching Docker; (Q5) 4 sub-slices.

**Goal.** Two more scope panels backed by native per-OS probes.

**Slice breakdown.**

### Slice 4a — Daemon Ports probe + wire + correlation + opt-in test · ✅ (`1111bbf`, `8285543`, `4ba452e`)

**Delivered.**
- Wire protocol bumped to **v5**. New subscription `Subscription::Ports { id }`. New events `Event::PortList { ports, source }` and `Event::PortsUnavailable { reason }`. New struct `ProbePort { local_ip, local_port, protocol, pid, process_name, container_id: Option<String>, partial: bool }`.
- `tepegoz-probe` crate scaffold filled in:
  - `ports::list_ports()` facade returns `Vec<ProbePort>` for the current platform. Uses `netstat2` for TCP listener enumeration (wraps procfs on Linux, libproc on macOS); uses `sysinfo` to resolve pid → process name in a single sweep per poll.
  - `linux::container_id_for_pid()` reads `/proc/<pid>/cgroup` and extracts a docker container id — handles cgroup v1 direct (`/docker/<id>`), v1 systemd (`/system.slice/docker-<id>.scope`), v2 (same suffix under `/system.slice`), and kubelet-nested (`/kubepods/.../docker-<id>.scope`). Accepts 12–64 hex-char ids. Returns `None` for non-docker cgroups.
  - `SOURCE_LABEL` const: `linux-procfs` / `macos-libproc` / `unsupported`. Delivered as `Event::PortList { source }` so the TUI can surface it in the tile footer (mirrors Docker's `engine_source`).
- `tepegoz-core::client::forward_ports` task: per-`Subscribe(Ports)` poll loop, hooked into the uniform `HashMap<id, AbortHandle>` subscription model; polls every 2 s via `tokio::task::spawn_blocking(list_ports)` so the blocking filesystem / syscall work doesn't stall the runtime. Emits `PortsUnavailable { reason }` exactly once per availability transition (mirrors Docker's once-per-flip guard).
- Daemon-side macOS correlation: `forward_ports` opportunistically opens a `tepegoz_docker::Engine` connection when any port row has `container_id == None` and a non-zero pid. Matches `ProbePort.local_port` against each container's `DockerContainer.ports[].public_port` (skipping `public_port == 0`). First match wins. Docker-down gracefully degrades to `container_id = None` without blocking the Ports subscription. Linux skips this block entirely — the probe already correlates via cgroup, so there's no need for a per-poll Docker roundtrip.
- Tests: 3 new proto codec roundtrips; 2 cross-OS probe smoke tests; 9 Linux-only cgroup-parser cases; 1 always-on + 1 opt-in integration test in `crates/tepegoz-core/tests/ports_scope.rs`. 172 total on macOS / 181 on ubuntu-latest.

**Deviations from the proposal.**
- Proposal said netlink `NETLINK_SOCK_DIAG` for Linux listening-socket enumeration; 4a uses `netstat2` which wraps procfs `/proc/net/tcp*` text parsing instead. The decision and rationale are captured in `crates/tepegoz-probe/Cargo.toml`: procfs parsing is mature, the API surface is small, and inode → pid correlation is the same work in either direction. Upgrade to `NETLINK_SOCK_DIAG` as a polish commit if profiling ever shows text parsing hot — wire shape does not change.
- Proposal said TCP + UDP listeners; 4a ships TCP only. UDP is a straightforward addition (toggle `ProtocolFlags::UDP` in the netstat2 call) but brings ambiguity — UDP sockets don't have a LISTEN state — so deferred until the TUI surfaces them meaningfully.

**Acceptance tests.**
- `tepegoz-proto::codec::{subscribe_ports_roundtrip, port_list_event_roundtrip, ports_unavailable_event_roundtrip}` — wire integrity for all three new variants including `ProbePort` with `container_id: Some(_)` and `partial: true` rows.
- `tepegoz-probe::ports::tests::{source_label_matches_platform, list_ports_returns_a_vec_without_panicking}` — smoke test that `list_ports()` returns on any supported OS without panicking and emits only TCP rows with non-zero `local_port`.
- `tepegoz-probe::linux::tests::cgroup_*` (9 cases, Linux-only) — cgroup parser correctness across v1 direct, v1 systemd, v2, kubelet-nested, non-docker, empty, too-short-id, non-hex-after-docker, and containerd-with-docker-substring paths.
- `crates/tepegoz-core/tests/ports_scope.rs::ports_subscription_emits_either_port_list_or_unavailable` — always-on: subscribes, asserts the daemon emits exactly one of `PortList | PortsUnavailable` within 30 s with non-empty `source`/`reason` string.
- `crates/tepegoz-core/tests/ports_scope.rs::ports_subscription_sees_locally_bound_listener_within_budget` — opt-in `TEPEGOZ_PROBE_TEST=1`: binds an ephemeral TCP listener in the test process, subscribes, drains events until a `PortList` includes the bound `local_port`, asserts the row attributes `pid == std::process::id()`, `protocol == "tcp"`, and non-empty `process_name` within a 6 s budget.

### Slice 4b — Daemon Processes probe + wire + integration test · ✅ (`d626f4f`)

**Delivered.**
- Wire protocol bumped to **v6**. New subscription `Subscription::Processes { id }`. New events `Event::ProcessList { rows: Vec<ProbeProcess>, source }` and `Event::ProcessesUnavailable { reason }`. New struct `ProbeProcess { pid, parent_pid, start_time_unix_secs, command, cpu_percent: Option<f32>, mem_bytes, partial }`. The `Option<f32>` for `cpu_percent` is deliberate — `None` signals "not yet measured" (first sample after subscription, before any delta); `Some(x)` signals a measured value including `Some(0.0)` which correctly means "idle". The TUI renders `None` as an em-dash; wire-level it's a one-byte tag.
- `tepegoz-probe::processes` module with `ProcessesProbe` struct. Stateful (`{ system: sysinfo::System, first_sample: bool }`) since sysinfo's CPU% comes from a delta between consecutive `refresh_processes_specifics` calls. `ProcessesProbe::sample() -> Result<Vec<ProbeProcess>, ProcessesError>` refreshes the system snapshot and returns one row per visible process. First call emits `cpu_percent: None` (sysinfo has no prior delta); subsequent calls emit `Some(x)`. `start_time_unix_secs` populated from `sysinfo::Process::start_time()` so `(pid, start_time)` can serve as a stable identity for selection persistence under pid-reuse (4c concern; wire is already shaped for it).
- `tepegoz-probe::processes::SOURCE_LABEL = "sysinfo"` — delivered in `Event::ProcessList { source }` so the TUI can surface the backend in the tile footer.
- Daemon `forward_processes` task in `tepegoz-core::client`: per-`Subscribe(Processes)` poll loop in the uniform `HashMap<id, AbortHandle>` pattern. Refreshes every 2 s. The probe is stateful, so the task owns the `ProcessesProbe` and moves it into `spawn_blocking` each iteration (sysinfo's refresh is sync /proc reads + syscalls, not async), receiving it back through the closure return tuple to preserve the delta computation across iterations. On `JoinError` (probe task panics) the task resets to a fresh probe — the next emitted event will again carry `cpu_percent: None` (correct per the probe contract).
- Tests: 3 new proto codec roundtrips (`subscribe_processes_roundtrip`, `process_list_event_roundtrip_preserves_first_sample_cpu_none` pinning `None` ≠ `Some(0.0)`, `processes_unavailable_event_roundtrip`); 3 probe unit tests (first-sample-None invariant, second-sample-Some invariant, own-pid-appears-with-non-empty-command); 1 always-on + 1 opt-in integration test in `crates/tepegoz-core/tests/processes_scope.rs`. 180 total on macOS / 189 on ubuntu-latest.

**No deviations from the CTO's 4b sign-off.** The CTO's three 4b-specific notes all baked in: first-sample CPU% = None semantic (wire carries it via `Option<f32>` + probe emits None on first refresh + integration test pins it); `(pid, start_time)` stable identity for 4c selection persistence (`start_time_unix_secs` shipped on `ProbeProcess`); opt-in test shape (spawned child with cmdline assertion + `ChildGuard` force-kills on Drop).

**Acceptance tests.**
- `tepegoz-proto::codec::{subscribe_processes_roundtrip, process_list_event_roundtrip_preserves_first_sample_cpu_none, processes_unavailable_event_roundtrip}` — wire integrity including the `None` ≠ `Some(0.0)` invariant (the roundtrip test asserts `r[0].cpu_percent.is_none()` after rkyv serialization / deserialization).
- `tepegoz-probe::processes::tests::{first_sample_returns_cpu_none_for_every_row, second_sample_returns_cpu_some_for_every_row, sample_contains_current_test_process}` — probe contract + self-attribution.
- `crates/tepegoz-core/tests/processes_scope.rs::processes_subscription_emits_either_process_list_or_unavailable` (always-on): asserts `ProcessList` xor `ProcessesUnavailable` within 30 s with non-empty source / reason AND that every row in the first `ProcessList` carries `cpu_percent: None`.
- `crates/tepegoz-core/tests/processes_scope.rs::processes_subscription_sees_spawned_child_within_budget` (opt-in `TEPEGOZ_PROBE_TEST=1`): spawns a known `sleep 30` child (`ChildGuard` force-kills on Drop), subscribes, drains until the child's pid appears with command containing `"sleep"`, non-zero `start_time_unix_secs`, non-zero `mem_bytes` (or `partial: true`). 5 s budget covers one refresh boundary.

### Slice 4c — Ports tile TUI with Processes toggle · ✅ (`4b850c1`)

**Delivered.**
- New `ScopeKind::Ports` variant; Ports tile in the god-view layout flipped from `Placeholder` to `Scope(ScopeKind::Ports)` (`tile.rs`) with render dispatch in `session.rs`.
- `PortsScope` state wraps two coequal views — `PortsView` (with `PortsViewState::{Connecting, Available {rows, source}, Unavailable {reason}}`) and `ProcessesView` (analogous). Both subscriptions live concurrently; `active: PortsActiveView::{Ports, Processes}` is the render switch and is flipped by `p`.
- `PortKey { protocol, local_port, pid }` and `ProcessKey { pid, start_time_unix_secs }` are the stable identities for selection persistence. `reanchor_selection(old_key)` on state-change tries to place the cursor on the matching key; falls back to clamping into the new visible range if the old entity is gone. Pid-reuse under a different `start_time` never silently retargets selection (state-machine test pins it).
- Input routing: `handle_forward_bytes` now matches `routes_to_scope(self.view.focused)` as a two-arm dispatch (`Docker` → `handle_scope_key`; `Ports` → `handle_ports_key`). `handle_ports_key` absorbs `p` as the toggle at the outer scope (unless a filter is active, in which case `p` is a filter character) then delegates to `handle_ports_list_key` or `handle_processes_list_key` depending on `active`. Each per-view handler owns its own filter + navigation logic, matching the Docker precedent.
- Renderer in `scope::ports::render` mirrors `scope::docker::render`'s shape: three-state lifecycle, filter bar on top when active, tabular body, help bar at bottom, em-dash for `cpu_percent: None` in the Processes table, `UDP coming v1.1` footer hint in the Ports status bar per the CTO's 4c UDP-resolution requirement.
- Help bar copy adapts: Ports view → `[j/k] nav · [/] filter · [p] Processes`; Processes view → `[j/k] nav · [/] filter · [p] Ports`; filter-active → `[Enter] apply · [Esc] clear · [Backspace] delete`.
- Selection persistence works across three scenarios (all tested): (1) rows reorder → cursor follows the selected key to its new index; (2) selected entity disappears → cursor clamps into the new visible range; (3) pid reuse with different `start_time` → cursor stays on the original `(pid, start_time)` row rather than drifting to the reused pid.

**CTO-flagged notes status.**
- **Tile-title-footer discoverability:** landed. Help bar advertises `[p] Processes` / `[p] Ports`.
- **Selection persistence:** landed. `(protocol, port, pid)` for Ports, `(pid, start_time)` for Processes, with clamp fallback. State-machine tests cover all three scenarios.
- **First-sample CPU% em-dash:** landed. Renderer checks `cpu_percent: Option<f32>` and emits `—` for `None`, `f32` for `Some`. Render test pins em-dash presence + absence of `0.0` for all-None rows.
- **UDP resolution:** option (c) — deferred to v1.1 with an explicit footer hint (`UDP coming v1.1`). Implemented in `render_ports_status_bar`. Revisit if user feedback demands UDP in v1.

**Deviations from the proposal.**
- Optional 5th (cmdline-truncation visual hint): skipped. Requires either a wire flag on `ProbeProcess` (protocol bump, heavy for a cosmetic hint) or a heuristic in the renderer (false-positive prone). `docs/OPERATIONS.md` Common Issues already documents the macOS-truncation behavior so users have an answer. Revisit if signal demands.

**Acceptance tests (207 on macOS / 216 on ubuntu-latest).**
- `tepegoz-tui::app::tests` +13: event routing (PortList, PortsUnavailable, ProcessList, ProcessesUnavailable), toggle semantics (cycles views, absorbed during filter), independent selection per view, selection persistence by key under reorder + disappearance + pid-reuse-with-different-start-time, filter typing / backspace / enter / esc, Ports-focused stdin routes to handle_ports_key rather than SendInput.
- `tepegoz-tui::scope::ports::tests` +14: three-state lifecycle per view, port + process tables with selection marker + container column, `cpu_percent: None` em-dash vs `Some(12.5)` number, unavailable verbatim reason, empty-list messages ("No listening ports" / "No running processes"), help bar shows `[p] Processes` / `[p] Ports` per view, filter bar caret, partial-row `?` cue.
- `tile::tests::routes_to_scope_returns_scope_kind_only_for_scope_tiles` updated to assert `TileId::Ports` now routes to `Some(ScopeKind::Ports)`.

**Gate.** CI green on macOS + ubuntu-latest, then CTO review before 4d.

### Slice 4d — Phase 4 e2e + manual demo script · ✅ (`d202edf`, `bee6aba`, `77aa9ca`, _close commit_)

**Delivered.**
- **Combined E2E integration test** at `crates/tepegoz-core/tests/ports_processes_e2e.rs`:
  - `ports_and_processes_see_spawned_child_and_see_it_disappear` (opt-in `TEPEGOZ_PROBE_TEST=1`): spawns a python3 child that binds a loopback TCP port and sleeps, reads the assigned port from the child's stdout (handshake to avoid racing the probe against the bind), subscribes to BOTH Ports + Processes, asserts the child pid appears in both within 6 s, kills the child, asserts it disappears from both within 6 s. 6 s budget = 2 s cadence + one refresh boundary + slack.
  - `docker_bound_port_surfaces_with_container_correlation` (opt-in on BOTH `TEPEGOZ_PROBE_TEST=1` AND `TEPEGOZ_DOCKER_TEST=1`): starts an alpine container publishing `<host_port>:80`, subscribes to Ports, asserts the row for that port carries `container: Some(<id>)` within 6 s. Pins the README mockup's `:3000 web (docker)` feature — the one most likely to silently regress since it spans three subsystems (probe, daemon, docker).
  - `tokio::process::Command::kill_on_drop(true)` handles subprocess cleanup without a hand-rolled `ChildGuard`; a sync `DockerContainerGuard` Drop handles the container.
- **Manual demo script** in `docs/OPERATIONS.md` "Slice 4d manual demo prep": 8 scenarios + pass/fail matrix. All 8 are the gate (unlike C3's 1–8-gate + 9-advisory). Scenarios 7 and 8 specifically pin 4c-deliverable behavior that the integration tests can't fully exercise — UDP footer hint legibility at 120×40, and second-sample CPU% transition from em-dash to number.

**Acceptance tests.**
- 2 new opt-in integration tests in `ports_processes_e2e.rs` (see above).
- Both skip cleanly when env vars are unset so CI stays green without provisioning probes or Docker.
- 209 total tests on macOS / 218 on ubuntu-latest (207 + 2 always-on skip paths; opt-in paths add nothing to the default count).

**Acceptance (Phase 4 close).** All 8 manual-demo scenarios sign off; `TEPEGOZ_PROBE_TEST=1` opt-in integration tests green locally + on the hosts where user preps elevated access; `cargo test --workspace` green on both OSes with env vars unset. On user sign-off, a separate close commit flips STATUS row 4 to ✅ (not bundled into the 4d feature commit — the close is triggered by user action per the Phase 3 precedent).

**Not in scope (Phase 4).** Remote probes (Phase 6 with the agent). Process signal actions (kill keybind) — candidate follow-up for v1.1. Port tied-to-pane navigation (`Enter` on a port row opens a shell / log tail). UDP listeners.

**Risks.** Text parsing of `/proc/net/tcp*` is CPU-cheap but allocates — if hundreds of listeners, check profiling once Phase 5 adds remote probing. macOS libproc pidfdinfo under netstat2 has known flakiness under sandbox restrictions; the `partial: true` pattern is the escape valve.

---

## Phase 5 — SSH transport + remote pty · 🟠 (5a + 5b landed)

**Goal.** `tepegoz connect user@host` opens a remote pty pane — same UX as local.

Proposal pass signed off 2026-04-14: Q1 ssh concrete + trait-in-Phase-10; Q2 ssh_config + overrides (config.toml + env) with first-non-empty-wins + `tepegoz doctor --ssh-hosts` diagnostic; Q3 russh-direct in Phase 5, agent-backed in Phase 6; Q4 SSH_AUTH_SOCK + IdentityFile (NOT layered under Decision #2); **Q5 tab-strip inside PTY tile** (option b per README mockup — "all activity visible" is the product promise, one line of chrome is the point); Q6 four-state markers (`●`/`◐`/`○`/`⚠`) + state machine + Ctrl-b r recovery; Q7 five sub-slices.

### Slice 5a — `tepegoz-ssh` crate: connect + auth + TOFU + ssh_config · ✅ (`2b54d2e`)

**Delivered.**
- `tepegoz-ssh` crate with concrete API (no trait abstraction — that's Phase 10): `HostList::discover()`, `connect_host(alias, &HostList, &KnownHostsStore)`, `open_session(&SshSession)`. Auth chain via `$SSH_AUTH_SOCK` → `IdentityFile` from ssh_config; russh errors surfaced verbatim including `KeyIsEncrypted` for passphrase-protected keys with no agent.
- Host-key TOFU against `data_dir/tepegoz/known_hosts` (Linux `$XDG_DATA_HOME`, macOS `~/Library/Application Support`). OpenSSH-compatible format; never touches `~/.ssh/known_hosts`. First contact auto-trusts; mismatch rejects with `SshError::HostKeyMismatch { path, line }`.
- ssh_config parser via `russh-config` (hybrid: manual `Host`-line walk collects concrete aliases, then per-alias resolution through russh-config's standard ssh_config(5) merge rules).
- Tests: 14 cross-OS lib tests (config + known_hosts) + 1 opt-in integration against testcontainer openssh-server on `TEPEGOZ_SSH_TEST=1`.
- `docs/ARCHITECTURE.md §8` gains "SSH auth" + "SSH host-key TOFU" bullets distinguishing from Decision #2 (at-rest root key) and Phase 6 (agent binary TOFU).

### Slice 5b — Host discovery + Fleet tile + `doctor --ssh-hosts` · ✅ (`<5b commit>`)

**Delivered.**
- Wire protocol bumped to **v7**. New subscription `Subscription::Fleet { id }`. New events `Event::HostList { hosts: Vec<HostEntry>, source }` and `Event::HostStateChanged { alias, state }`. `HostEntry` moved from `tepegoz-ssh` to `tepegoz-proto` as the shared wire type (re-exported from `tepegoz-ssh`); `identity_files: Vec<String>` (not `Vec<PathBuf>`) so rkyv round-trips cleanly. `HostState` enum carries Q6's four user-visible states (`Disconnected`/`Connecting`/`Connected`/`Degraded` + the `⚠` terminal states `AuthFailed`/`HostKeyMismatch`) plus Phase 6's `AgentNotDeployed`/`AgentVersionMismatch` additive states.
- `tepegoz-core::client` gains `fleet_subs: HashMap<u64, AbortHandle>` alongside the existing pane / docker / ports / processes subscription maps — uniformity preserved per the Phase 3 contract. `forward_fleet` task runs `HostList::discover()` on `spawn_blocking`, emits one `HostList` snapshot, emits one `HostStateChanged { Disconnected }` per host, then parks until cancelled. 5c's connection supervisor replaces the park with the real state machine.
- TUI `ScopeKind::Fleet` replaces the 5-phase placeholder. `FleetScope` hosts `FleetScopeState::{Connecting, Available { hosts, states, source }}` (no `Unavailable` variant — discovery failure emits empty `HostList` + error-labeled source). Per-host state map stays stable across refreshes. Selection persistence via `FleetKey(alias)`. Filter matches alias / hostname / user. Help bar advertises `[j/k] nav · [/] filter · Ctrl-b h/j/k/l focus`.
- Fleet tile renderer (`scope::fleet::render`) draws four-glyph state markers per Q6 (`●` Connected / `◐` Connecting+Degraded / `○` Disconnected / `⚠` AuthFailed+HostKeyMismatch+agent errors). "procs" column renders as em-dash placeholder — Phase 6 fills it with real values, no one-shot SSH probe workaround (per user sign-off "em-dash is honest; don't stand up scope creep dressed as polish"). Footer shows resolved source label when it's an override; hidden when ssh_config.
- `tepegoz doctor --ssh-hosts` CLI dumps the resolved host list + source label, mirrors `--claude-layout`'s shape.
- Slice 5a follow-ups folded: (#2) `~` expansion in tepegoz config.toml `identity_file` strings; (#3) `known_hosts` file mode set to `0600` after trust writes, matching OpenSSH convention; (#4) agent certificate identities skipped with explicit "N certificate identity(ies) skipped (not supported in Phase 5)" entry in `AuthFailed.reason`; (#5) Include-directive behavior pinned by test — `russh-config` raises `HostNotFound` on `Include` lines, mitigated by pre-stripping Includes before parsing (top-level hosts still resolve; Include'd hosts are documented as a Phase 5 limitation in OPERATIONS).
- `docs/OPERATIONS.md` gains an "SSH Fleet discovery (Phase 5 Slice 5b)" section: precedence + config.toml schema + `tepegoz doctor --ssh-hosts` usage + documented Phase 5 limitations + host-key TOFU explanation.
- Tests: 3 new proto codec roundtrips (`subscribe_fleet`, `host_list_event_roundtrip_preserves_all_fields`, `host_state_changed_event_roundtrip_covers_all_variants`); 3 new `tepegoz-ssh` lib tests (tilde expansion, Include-directive limitation pin, known_hosts 0600 mode); 8 new TUI render tests in `scope::fleet` (three-state lifecycle, selection marker, four-glyph state rendering, empty-list hints, filter/help-bar swaps, em-dash procs column); 1 new default integration test `fleet_scope.rs` (subscribe → HostList + Disconnected-per-host within a 15 s budget). 241 total on macOS (was 226 post-5a).

**5b-to-5c gap** (explicit — not a bug): Fleet renders `all Disconnected` between this slice and 5c's merge because the connection supervisor isn't up yet. Same degrade-gracefully shape as Phase 3's `DockerUnavailable`. 5c replaces the parked `forward_fleet` task body with real state transitions.

### Slice 5c-i — Supervisor state machine + heartbeat + backoff + autoconnect · ✅ (`<5c-i commit>`)

**Delivered.**
- Per-host `host_supervisor` task spawned inside a `tokio::task::JoinSet` by the rewritten `forward_fleet` coordinator. Subscription cancellation drops the JoinSet, which aborts every supervisor — same aggregate-lifecycle pattern as Phase 3's Docker forwarders.
- State machine: `Disconnected` → `Connecting` → `Connected` → `Degraded` (on first heartbeat miss) → `Disconnected` → (backoff) → `Connecting` …. Emits `Event::HostStateChanged` on every transition.
- Heartbeat: `keepalive@openssh.com` every 30 s via `russh::client::Handle::send_keepalive(want_reply=true)`. Miss counter increments on send-Err or `handle.is_closed()`; miss ≥ 1 → `Degraded`; miss ≥ 3 → `Disconnected` + trigger reconnect. russh 0.60's keepalive is fire-and-forget (no Future resolves on server ack), so the Degraded step may be skipped under clean TCP close — the wire/state-machine shape is correct either way, and Phase 6's agent can track ACKs natively.
- Reconnect backoff ladder: 1 / 2 / 5 / 15 / 60 s; cap at 60 s; resets to 1 s when a connection held ≥ 30 s before dying (so transient failures don't permanently elevate the next-retry for a healthy host).
- Lazy-connect default: `autoconnect = false`. Hosts with `autoconnect = true` in tepegoz `config.toml` dial on startup; ssh_config-sourced and env-sourced hosts always wait (no autoconnect concept there). 5c-ii's `FleetAction::Reconnect` wakes lazy hosts.
- ProxyJump pre-check (5a follow-up #1): hosts with `ProxyJump` set in ssh_config transition straight to `AuthFailed` (terminal) with a warn-level log naming the unsupported directive. 5c-ii's red toast reads the `AuthFailed` state as the ⚠ trigger; reason is visible in daemon logs for now.
- Terminal states (`AuthFailed`, `HostKeyMismatch`) park the supervisor forever in 5c-i (no action channel yet); 5c-ii adds Reconnect to reset them.
- Env-var overrides: `TEPEGOZ_CONFIG_DIR` + `TEPEGOZ_DATA_DIR` override the `dirs` crate's config/data paths. Primary use is portable integration tests on macOS (where `dirs::config_dir()` ignores `XDG_CONFIG_HOME`); also useful for headless containers.
- `HostList::autoconnect: HashSet<String>` — daemon-side only (not on the wire); populated only when the source is `HostSource::TepegozConfig`.
- **Tests**: 1 new autoconnect-parse unit test; 1 new opt-in integration test (`TEPEGOZ_SSH_TEST=1 + TEPEGOZ_DOCKER_TEST=1`) that provisions a tepegoz config.toml with `autoconnect = true`, starts an openssh-server container, observes the full `Disconnected → Connecting → Connected` happy path, kills the container, observes `Disconnected` within 90 s + at least one reconnect attempt. 5b fleet_scope test relaxed to accept any structurally-valid `HostState` since ProxyJump hosts in the test machine's ssh_config now emit `AuthFailed` directly.

No wire-protocol change. 5c-ii bumps wire to v8 for `FleetAction` + `FleetActionResult`.

### Slice 5c-ii — FleetAction wire + Ctrl-b r reconnect UX · ⚪

Upcoming. Wire protocol v8: `Payload::FleetAction { alias, kind: FleetActionKind::{Reconnect, Disconnect} }` + `Payload::FleetActionResult { alias, kind, outcome: Success | Failure { reason } }`. Daemon dispatches to the appropriate supervisor via per-host mpsc channels kept in `forward_fleet`'s coordinator scope. TUI `Ctrl-b r` keybind on a focused Fleet row dispatches Reconnect; toast on `AuthFailed` / `HostKeyMismatch` / ProxyJump surfaces the reason verbatim.

### Slice 5d — Remote pty open + pane-stack + `tepegoz connect <alias>` · ⚪

Upcoming. `OpenPane` grows `target: PaneTarget::{Local, Remote { alias }}` (wire v8); daemon `RemotePane` wraps an SSH channel with `request_pty` + `request_shell`; TUI pane-stack lands (Q5's tab strip inside the PTY tile — `[N label*]` format + desaturation for inactive; `Ctrl-b 0..9`/`n`/`p`/`&`/`w`); `tepegoz connect <alias>` CLI subcommand.

### Slice 5e — Error surfaces + disconnect recovery + e2e manual demo · ⚪

Upcoming. Inline "`— ssh connection lost: <reason> —`" marker in the pane scrollback; `⚠` red state rendering with russh-verbatim toast; host-key-change rejection path. Phase 5 close: 8-scenario manual demo.

**Not in scope.** QUIC (Phase 10). Multi-host agent coordination (Phase 6). ProxyJump (v1.1). SSH certificate auth (v1.1). PKCS#11 hardware tokens (v1.1, indirect via ssh-agent).

**Risks.** Pure-SSH latency may feel sluggish for live telemetry; QUIC in Phase 10 is the relief valve. Acceptable for Phase 5 since the killer app here is remote pty. Phase-5-era remote-pane session-persistence gap (dropped SSH kills pane) is documented limitation — Phase 6's agent-backed remote pty fixes it without changing wire protocol.

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
