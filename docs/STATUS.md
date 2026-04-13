# Status

## Current state

**Phase 3 in progress (Slices A + B + pre-C cleanups + C1 landed).** On top of the Slices A/B daemon-side docker scope (container list, lifecycle actions, log + stats streaming), the TUI is now structured around a pure-state-machine `App` + `AppEvent` / `AppAction` event bus. `Ctrl-b s` switches to a ratatui-rendered scope view (Slice C1 ships a stub; Slice C2 wires the container table). `Ctrl-b a` switches back to the attached pane via a synthetic re-attach (fresh `AttachPane` subscription, daemon replays scrollback as `PaneSnapshot`). Wire protocol unchanged at v4.

Phase 2 closed cleanly: the "immediate-detach" report turned out to be user confusion (attached pane shell is visually identical to the outer shell) — see `docs/ISSUES.md` resolved entry. TUI now paints an OSC 0 window title (`tepegoz · pane N`) on attach so the pane is unmistakable.

## Phase matrix

| # | Name | Status | Commit(s) | Acceptance test(s) |
|---|---|---|---|---|
| 0 | Scaffold | ✅ | `81c7731` | `tepegoz --help`, green CI |
| 1 | Proto + daemon + TUI round-trip | ✅ | `3715bf9` | `daemon_persistence.rs` |
| 2 | Local pty multiplex + persistence | ✅ | `eab274c`, `321ed5e` | `pty_persistence.rs`, `subscribe_does_not_duplicate_bytes`, `pane_honors_cwd_and_exposes_pane_id_env` |
| 3 | Docker scope panel | 🟠 (Slices A + B + C1 landed) | `24dc244`, `816765b`, `e4d2113`, _C1 HEAD_ | `docker_scope.rs` (6 cases, opt-in end-to-end), `daemon_persistence.rs` (incl. version-mismatch), 26 TUI state-machine + InputFilter cases, codec roundtrips for v3 + v4, docker crate translation tests |
| 4 | Ports + processes panels (local) | ⚪ | — | — |
| 5 | SSH transport + remote pty | ⚪ | — | — |
| 6 | Agent binary + remote scopes | ⚪ | — | — |
| 7 | Port scanner | ⚪ | — | — |
| 8 | Recording + replay | ⚪ | — | — |
| 9 | Claude Code pane awareness | ⚪ | — | — |
| 10 | QUIC hot path + release 0.1.0 | ⚪ | — | — |

Status key: ✅ complete · 🟡 code+tests green, user acceptance pending · 🟠 in progress · ⚪ not started.

## What works end-to-end

- Daemon binds user-scoped Unix socket at `$XDG_RUNTIME_DIR/tepegoz-$uid/daemon.sock` (fallback `$TMPDIR` or `/tmp`). Parent dir `0700` when default path (we own it), socket `0600`. Override paths leave parent perms alone.
- Stale-socket eviction on startup. Refuses to start under another live daemon. Graceful SIGINT shutdown with socket cleanup.
- Wire protocol v4: rkyv-archived `Envelope { version, payload }`, 4-byte big-endian length prefix, bytecheck validation on every read. Messages:
  - Commands: `Hello`, `Ping`, `Subscribe(Status | Docker | DockerLogs | DockerStats)`, `Unsubscribe`, `OpenPane`, `AttachPane`, `ClosePane`, `ListPanes`, `SendInput`, `ResizePane`, `DockerAction`
  - Responses: `Welcome`, `Pong`, `PaneOpened`, `PaneList`, `DockerActionResult`, `Error`
  - Events (in `Event(EventFrame)`): `Status`, `PaneSnapshot`, `PaneOutput`, `PaneExit`, `PaneLagged`, `ContainerList`, `DockerUnavailable`, `ContainerLog`, `ContainerStats`, `DockerStreamEnded`
- PTY manager owns panes; per-pane 2 MiB ring buffer (`VecDeque<Bytes>` with total-byte accounting, oldest-chunks-evict on overflow); per-pane reader + waiter threads; `tokio::sync::broadcast` channel for subscribers.
- Docker subscription: per-`Subscribe(Docker)` task walks platform socket candidates (`$DOCKER_HOST` env > Docker Desktop > Colima > Rancher Desktop > native Linux), pings the first reachable engine, and emits a `ContainerList` immediately plus every 2 s. On unreachable engine: emits `DockerUnavailable { reason }` once per availability transition, retries `Engine::connect` every 5 s. Survives `dockerd` restarts; survives the user starting docker after subscribing.
- Docker lifecycle actions: `Payload::DockerAction { request_id, container_id, kind }` runs in a spawned task so a slow daemon doesn't stall the session loop. Always replies with `DockerActionResult { request_id, container_id, kind, outcome }` — engine-unavailable and bollard errors both surface as `Failure { reason }` (verbatim from dockerd; daemon doesn't try to classify). `Remove` is force-remove (`docker rm -f` semantics).
- Docker logs streaming: per-`Subscribe(DockerLogs)` task opens a bollard log stream (stdout + stderr) and forwards each chunk as `Event::ContainerLog { stream, data }`. Always emits a terminal `Event::DockerStreamEnded { reason }` — engine unreachable, container exit, container removal — so a UI knows the stream is done and never spins waiting for chunks that won't come.
- Docker stats: per-`Subscribe(DockerStats)` task streams bollard stats (~1/sec). CPU% is computed from cpu/precpu deltas using the standard docker-stats-CLI formula; `0.0` whenever the calculation can't be performed (first sample, missing precpu on Windows, sys_delta=0). Like logs, terminates with `DockerStreamEnded`.
- Daemon client session: single dedicated writer task drains an mpsc of outgoing envelopes — no per-write locks. Each `AttachPane` spawns a forwarder task; each `Subscribe(Docker | DockerLogs | DockerStats)` spawns a forwarder/poll task tracked in a `HashMap<id, AbortHandle>` so `Unsubscribe { id }` can cancel just that subscription.
- TUI architecture: pure-state-machine `App` (in `tepegoz-tui/src/app.rs`) handles every external happening as an `AppEvent` (StdinChunk, DaemonEnvelope, Resize, Tick, PendingActionTimeout) and emits `AppAction`s (SendEnvelope, WriteStdout, EnterPaneMode, EnterScopeMode, DrawScope, Detach{User|PaneExited}). The `AppRuntime` (in `tepegoz-tui/src/session.rs`) executes those actions against the daemon socket, stdin/stdout, and ratatui's terminal. State-machine tests live in `app::tests`.
- TUI view modes: `View::Pane` (raw passthrough — current behavior; daemon stamps OSC 0 title `tepegoz · pane N` on attach) and `View::Scope(ScopeKind::Docker)` (ratatui-rendered; Slice C1 ships a stub indicating "Docker scope — Slice C2 incoming"; Slice C2 wires the container table). Switch via `Ctrl-b s` (→ Scope) / `Ctrl-b a` (→ Pane); detach via `Ctrl-b d` / `Ctrl-b q` from either view.
- Mode switch mechanics: Pane → Scope clears the screen and starts the ratatui draw cycle. Scope → Pane clears the screen, cancels the previous `AttachPane` subscription, and sends a fresh `AttachPane` so the daemon replays current scrollback as `PaneSnapshot`. **Vim-preservation across the synthetic re-attach is a make-or-break check for Slice C2** — see `docs/ROADMAP.md` Slice C2 risks.
- Daemon handshake: rejects protocol-version mismatches at the socket level. Both `Envelope.version` and `Hello.client_version` must equal the daemon's `PROTOCOL_VERSION`; otherwise the daemon sends a structured `Error(VersionMismatch)` naming both versions and closes. Without this guard a v3 client connecting to a v4 daemon would silently handshake and later trip an opaque rkyv decode error.
- Panes inherit `TEPEGOZ_PANE_ID=<id>` in env. TUI refuses to run if its own env has that var (prevents recursive attach feedback loop).
- Shells spawn in the TUI's `current_dir()` rather than `$HOME` (portable-pty's default).
- TUI sets the terminal window title to `tepegoz · pane N` on attach (OSC 0) and clears it on detach, so an attached pane is visually distinct from the outer shell.

## Test coverage (57 tests, all green)

- `tepegoz-proto::codec` — 11 (envelope/status roundtrip, frame-too-large guard, Subscribe(Docker | DockerLogs) roundtrips, ContainerList / DockerUnavailable / ContainerLog / ContainerStats event roundtrips, DockerAction request + result roundtrips)
- `tepegoz-pty` — 4 (scrollback eviction, scrollback snapshot, subscribe-no-duplicates, cwd+pane_id env)
- `tepegoz-docker` — 7 (socket discovery order, discovery without HOME, into_wire translation, into_wire empty-state default, stats_to_wire CPU% formula, stats_to_wire zero-cpu fallback, stats_to_wire missing-memory-section default)
- `tepegoz-tui::input` — 12 (InputFilter pass-through, detach-d, detach-q, switch-to-scope, switch-to-pane, help, non-detach, detach-splits-stream, switch-splits-stream, detach split-across-chunks, switch split-across-chunks, double-Ctrl-B)
- `tepegoz-tui::app` — 14 (App state machine: initial_actions allocates AttachPane + ResizePane; Ctrl-b d emits user detach; pane keystrokes forward as SendInput; Ctrl-b s switches to scope + EnterScopeMode + DrawScope; second-switch is idempotent; Ctrl-b a returns to pane with synthetic re-attach (Unsubscribe + fresh AttachPane); PaneOutput in Pane mode emits WriteStdout; PaneSnapshot in Pane mode emits WriteStdout; PaneOutput in Scope mode is dropped; PaneExit propagates exit_code via DetachReason::PaneExited; events for unknown subscription ids are silently dropped; Resize forwards to daemon and only redraws in scope; Tick is a no-op in Pane and emits DrawScope in Scope)
- `crates/tepegoz-core/tests/daemon_persistence.rs` — 2 (phase-1 acceptance + handshake_rejects_protocol_version_mismatch)
- `crates/tepegoz-core/tests/pty_persistence.rs` — 1 (phase-2 acceptance)
- `crates/tepegoz-core/tests/docker_scope.rs` — 6 (phase-3 acceptance):
  - Subscribe(Docker) emits ContainerList xor DockerUnavailable (always-on)
  - Opt-in `TEPEGOZ_DOCKER_TEST=1` insists on Available for Subscribe(Docker)
  - DockerAction against unreachable engine returns Failure with non-empty reason (always-on)
  - Subscribe(DockerLogs) against unreachable engine emits DockerStreamEnded (always-on)
  - Subscribe(DockerStats) against unreachable engine emits DockerStreamEnded (always-on)
  - Opt-in `TEPEGOZ_DOCKER_TEST=1` end-to-end: provisions an alpine container, observes a log marker, observes a stats sample with mem_bytes > 0, performs Restart and asserts Success

## Next slice

**Phase 3 Slice C2 — Docker scope rendering + subscription lifecycle.** Replace the C1 "scope view stub" with the real container table (ratatui Table widget). Wire `Subscribe(Docker)` on enter / `Unsubscribe` on leave. Three distinct visual states: `Connecting` ("Connecting to docker engine…"), `Available` (table; `containers.len() == 0` → "No containers"), `Unavailable` (verbatim reason from the daemon). Navigation (↑↓ / j k / g G / Home End), filter (/), Esc-to-clear. **Vim-preservation across Scope→Pane synthetic re-attach is a make-or-break check.** Headless render test using `ratatui::backend::TestBackend`. Details in `docs/ROADMAP.md#phase-3--docker-scope-panel`.
