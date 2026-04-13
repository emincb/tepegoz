# Status

## Current state

**Phase 3 in progress (Slices A + B landed).** On top of the Slice A foundation (`Subscribe(Docker)` container list + `DockerUnavailable` graceful degradation), the daemon now also handles **lifecycle actions** (`Payload::DockerAction { Start | Stop | Restart | Kill | Remove }` → `DockerActionResult`), **logs streaming** (`Subscribe(DockerLogs)` → `Event::ContainerLog` chunks, terminating with `Event::DockerStreamEnded`), and **container stats** (`Subscribe(DockerStats)` → `Event::ContainerStats` samples with CPU% computed from cpu/precpu deltas). Wire protocol bumped to v4. TUI is still raw-passthrough — scope view + scope/pty switch arrives in Slice C.

Phase 2 closed cleanly: the "immediate-detach" report turned out to be user confusion (attached pane shell is visually identical to the outer shell) — see `docs/ISSUES.md` resolved entry. TUI now paints an OSC 0 window title (`tepegoz · pane N`) on attach so the pane is unmistakable.

## Phase matrix

| # | Name | Status | Commit(s) | Acceptance test(s) |
|---|---|---|---|---|
| 0 | Scaffold | ✅ | `81c7731` | `tepegoz --help`, green CI |
| 1 | Proto + daemon + TUI round-trip | ✅ | `3715bf9` | `daemon_persistence.rs` |
| 2 | Local pty multiplex + persistence | ✅ | `eab274c`, `321ed5e` | `pty_persistence.rs`, `subscribe_does_not_duplicate_bytes`, `pane_honors_cwd_and_exposes_pane_id_env` |
| 3 | Docker scope panel | 🟠 (Slices A + B landed) | `24dc244`, _Slice B HEAD_ | `docker_scope.rs` (6 cases, includes opt-in end-to-end), codec roundtrips for v3 + v4, docker crate translation tests |
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
- TUI is a raw-passthrough attacher: raw mode + alternate screen; stdin → `SendInput`; `PaneOutput` → stdout verbatim; `SIGWINCH` → `ResizePane`. Detach via `Ctrl-b d` or `Ctrl-b q`. (Docker scope view in the TUI lands in Slice C — see `docs/ROADMAP.md#phase-3--docker-scope-panel`.)
- Panes inherit `TEPEGOZ_PANE_ID=<id>` in env. TUI refuses to run if its own env has that var (prevents recursive attach feedback loop).
- Shells spawn in the TUI's `current_dir()` rather than `$HOME` (portable-pty's default).
- TUI sets the terminal window title to `tepegoz · pane N` on attach (OSC 0) and clears it on detach, so an attached pane is visually distinct from the outer shell.

## Test coverage (32 tests, all green)

- `tepegoz-proto::codec` — 11 (envelope/status roundtrip, frame-too-large guard, Subscribe(Docker | DockerLogs) roundtrips, ContainerList / DockerUnavailable / ContainerLog / ContainerStats event roundtrips, DockerAction request + result roundtrips)
- `tepegoz-pty` — 4 (scrollback eviction, scrollback snapshot, subscribe-no-duplicates, cwd+pane_id env)
- `tepegoz-docker` — 7 (socket discovery order, discovery without HOME, into_wire translation, into_wire empty-state default, stats_to_wire CPU% formula, stats_to_wire zero-cpu fallback when delta unavailable, stats_to_wire missing-memory-section default)
- `tepegoz-tui::input` — 7 (InputFilter: pass-through, detach-d, detach-q, non-detach, split-across-chunks, double-Ctrl-B)
- `crates/tepegoz-core/tests/daemon_persistence.rs` — 1 (phase-1 acceptance)
- `crates/tepegoz-core/tests/pty_persistence.rs` — 1 (phase-2 acceptance)
- `crates/tepegoz-core/tests/docker_scope.rs` — 6 (phase-3 acceptance):
  - Subscribe(Docker) emits ContainerList xor DockerUnavailable (always-on)
  - Opt-in `TEPEGOZ_DOCKER_TEST=1` insists on Available for Subscribe(Docker)
  - DockerAction against unreachable engine returns Failure with non-empty reason (always-on)
  - Subscribe(DockerLogs) against unreachable engine emits DockerStreamEnded (always-on)
  - Subscribe(DockerStats) against unreachable engine emits DockerStreamEnded (always-on)
  - Opt-in `TEPEGOZ_DOCKER_TEST=1` end-to-end: provisions an alpine container, observes a log marker, observes a stats sample with mem_bytes > 0, performs Restart and asserts Success

## Next slice

**Phase 3 Slice C — TUI scope view + scope/pty switch.** Bring ratatui back to the TUI; render the container table (name/image/state/cpu%/mem/ports) from a `Subscribe(Docker)`. Add a view-mode switch (`Ctrl-b s` → scope, `Ctrl-b a` → attached pane). Wire keybinds `r/s/k/x/l/Enter` for the lifecycle and logs/exec drilldowns. Then Slice D (`DockerExec` opens a new pty pane). Details in `docs/ROADMAP.md#phase-3--docker-scope-panel`.
