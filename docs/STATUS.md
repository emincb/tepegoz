# Status

## Current state

**Phase 3 in progress (Slice A landed).** Daemon now serves a `Subscribe(Docker)` subscription that streams `ContainerList` events from the local docker engine, or `DockerUnavailable` with a structured reason when no engine is reachable. Wire protocol bumped to v3. TUI is unchanged in this slice — scope view + scope/pty switch arrives in Slice C.

Phase 2 closed cleanly: the "immediate-detach" report turned out to be user confusion (attached pane shell is visually identical to the outer shell) — see `docs/ISSUES.md` resolved entry. TUI now paints an OSC 0 window title (`tepegoz · pane N`) on attach so the pane is unmistakable.

## Phase matrix

| # | Name | Status | Commit(s) | Acceptance test(s) |
|---|---|---|---|---|
| 0 | Scaffold | ✅ | `81c7731` | `tepegoz --help`, green CI |
| 1 | Proto + daemon + TUI round-trip | ✅ | `3715bf9` | `daemon_persistence.rs` |
| 2 | Local pty multiplex + persistence | ✅ | `eab274c`, `321ed5e` | `pty_persistence.rs`, `subscribe_does_not_duplicate_bytes`, `pane_honors_cwd_and_exposes_pane_id_env` |
| 3 | Docker scope panel | 🟠 (Slice A landed) | — | `docker_scope.rs`, `into_wire_translates_bollard_summary`, codec roundtrips |
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
- Wire protocol v3: rkyv-archived `Envelope { version, payload }`, 4-byte big-endian length prefix, bytecheck validation on every read. Messages:
  - Commands: `Hello`, `Ping`, `Subscribe(Status | Docker)`, `Unsubscribe`, `OpenPane`, `AttachPane`, `ClosePane`, `ListPanes`, `SendInput`, `ResizePane`
  - Responses: `Welcome`, `Pong`, `PaneOpened`, `PaneList`, `Error`
  - Events (in `Event(EventFrame)`): `Status`, `PaneSnapshot`, `PaneOutput`, `PaneExit`, `PaneLagged`, `ContainerList`, `DockerUnavailable`
- PTY manager owns panes; per-pane 2 MiB ring buffer (`VecDeque<Bytes>` with total-byte accounting, oldest-chunks-evict on overflow); per-pane reader + waiter threads; `tokio::sync::broadcast` channel for subscribers.
- Docker subscription: per-`Subscribe(Docker)` task walks platform socket candidates (`$DOCKER_HOST` env > Docker Desktop > Colima > Rancher Desktop > native Linux), pings the first reachable engine, and emits a `ContainerList` immediately plus every 2 s. On unreachable engine: emits `DockerUnavailable { reason }` once per availability transition, retries `Engine::connect` every 5 s. Survives `dockerd` restarts; survives the user starting docker after subscribing.
- Daemon client session: single dedicated writer task drains an mpsc of outgoing envelopes — no per-write locks. Each `AttachPane` spawns a forwarder task; each `Subscribe(Docker)` spawns a poll task tracked in a `HashMap<id, AbortHandle>` so `Unsubscribe { id }` can cancel just that subscription.
- TUI is a raw-passthrough attacher: raw mode + alternate screen; stdin → `SendInput`; `PaneOutput` → stdout verbatim; `SIGWINCH` → `ResizePane`. Detach via `Ctrl-b d` or `Ctrl-b q`. (Docker scope view in the TUI lands in Slice C — see `docs/ROADMAP.md#phase-3--docker-scope-panel`.)
- Panes inherit `TEPEGOZ_PANE_ID=<id>` in env. TUI refuses to run if its own env has that var (prevents recursive attach feedback loop).
- Shells spawn in the TUI's `current_dir()` rather than `$HOME` (portable-pty's default).
- TUI sets the terminal window title to `tepegoz · pane N` on attach (OSC 0) and clears it on detach, so an attached pane is visually distinct from the outer shell.

## Test coverage (25 tests, all green)

- `tepegoz-proto::codec` — 6 (envelope/status roundtrip, frame-too-large guard, Subscribe(Docker) roundtrip, ContainerList roundtrip, DockerUnavailable roundtrip)
- `tepegoz-pty` — 4 (scrollback eviction, scrollback snapshot, subscribe-no-duplicates, cwd+pane_id env)
- `tepegoz-docker` — 4 (socket discovery order, discovery without HOME, into_wire translation, into_wire empty-state default)
- `tepegoz-tui::input` — 7 (InputFilter: pass-through, detach-d, detach-q, non-detach, split-across-chunks, double-Ctrl-B)
- `crates/tepegoz-core/tests/daemon_persistence.rs` — 1 (phase-1 acceptance)
- `crates/tepegoz-core/tests/pty_persistence.rs` — 1 (phase-2 acceptance)
- `crates/tepegoz-core/tests/docker_scope.rs` — 2 (phase-3 acceptance: Subscribe(Docker) emits ContainerList xor DockerUnavailable; opt-in `TEPEGOZ_DOCKER_TEST=1` insists on Available)

## Next slice

**Phase 3 Slice B — lifecycle actions + logs streaming.** `DockerAction(Start|Stop|Restart|Kill|Remove)`, `DockerLogs(container_id, follow)` subscription, `ContainerStats` periodic events. Then Slice C (TUI scope view + scope/pty switch) and Slice D (`DockerExec` opens a new pty pane). Details in `docs/ROADMAP.md#phase-3--docker-scope-panel`.
