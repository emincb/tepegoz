# Status

## Current state

**Phase 3 in progress (Slices A + B + pre-C cleanups + C1 + C2-gate + C2c2 + C2c3-automated landed; C2c3 eyeball demo pending user).** On top of the Slices A/B daemon-side docker scope, the TUI is structured around a pure-state-machine `App` + `AppEvent` / `AppAction` event bus. `Ctrl-b s` switches to a ratatui-rendered scope view with a real container table, three distinct lifecycle states (Connecting / Available / Unavailable), navigation (↑↓ / j k / g G / Home End), and filter (/ to enter, Enter to commit, Esc to clear). `Ctrl-b a` switches back to the attached pane via a synthetic re-attach. Byte-level vim-preservation proven (`vim_preservation.rs`). C2c3 automated test pins the ≤2 s Subscribe→ContainerList latency with a provisioned alpine container; eyeball demo (real-terminal vim check + CTO §7 Step 10 engine-unavailable-mid-session) is on the user to run before C3 starts. Wire protocol unchanged at v4.

Phase 2 closed cleanly: the "immediate-detach" report turned out to be user confusion (attached pane shell is visually identical to the outer shell) — see `docs/ISSUES.md` resolved entry. TUI now paints an OSC 0 window title (`tepegoz · pane N`) on attach so the pane is unmistakable.

## Phase matrix

| # | Name | Status | Commit(s) | Acceptance test(s) |
|---|---|---|---|---|
| 0 | Scaffold | ✅ | `81c7731` | `tepegoz --help`, green CI |
| 1 | Proto + daemon + TUI round-trip | ✅ | `3715bf9` | `daemon_persistence.rs` |
| 2 | Local pty multiplex + persistence | ✅ | `eab274c`, `321ed5e` | `pty_persistence.rs`, `subscribe_does_not_duplicate_bytes`, `pane_honors_cwd_and_exposes_pane_id_env` |
| 3 | Docker scope panel | 🟠 (A + B + C1 + C2-gate + C2c2 + C2c3-automated; eyeball pending) | `24dc244`, `816765b`, `e4d2113`, `664703f`, `43b28eb`, `56a8a4f`, `c7c0d39`, _C2c3 HEAD_ | `docker_scope.rs` (7 cases incl. ≤2 s timing), `daemon_persistence.rs` (2), `pane_unsubscribe.rs`, `vim_preservation.rs`, `pty_persistence.rs`, 46 TUI state-machine + InputFilter + scope-renderer cases, codec roundtrips for v3 + v4, docker crate translation tests |
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
- Daemon client session: single dedicated writer task drains an mpsc of outgoing envelopes — no per-write locks. **Subscription uniformity**: pane and docker subscriptions share the same `HashMap<id, AbortHandle>` shape (`pane_subs` and `docker_subs`). `Unsubscribe { id }` cancels across both maps with a single check. Phase 4 (Ports/Processes), 5 (SSH remote pty), 7 (port scanner) inherit this shape — every new subscription kind adds a map alongside, not a new mechanism. (Through Slice C1 `pane_subs` was a `JoinSet<()>` with no per-id key, so `Unsubscribe` of a pane sub silently no-op'd; the C1 TUI's synthetic re-attach was leaking one zombie forwarder per Scope→Pane mode switch. Fixed in the C2-gate commit `43b28eb`; regression test `pane_unsubscribe.rs` pins the invariant.)
- TUI architecture: pure-state-machine `App` (in `tepegoz-tui/src/app.rs`) handles every external happening as an `AppEvent` (StdinChunk, DaemonEnvelope, Resize, Tick, PendingActionTimeout) and emits `AppAction`s (SendEnvelope, WriteStdout, EnterPaneMode, EnterScopeMode, DrawScope, Detach{User|PaneExited}, ShowToast{kind,message}). The `AppRuntime` (in `tepegoz-tui/src/session.rs`) executes those actions against the daemon socket, stdin/stdout, and ratatui's terminal. State-machine tests live in `app::tests`; headless render tests in `scope::docker::tests`.
- TUI view modes: `View::Pane` (raw passthrough — current behavior; daemon stamps OSC 0 title `tepegoz · pane N` on attach) and `View::Scope(ScopeKind::Docker)` (ratatui-rendered; **Slice C2c2 ships the real container table** with three distinct lifecycle states, navigation, and filter). Switch via `Ctrl-b s` (→ Scope) / `Ctrl-b a` (→ Pane); detach via `Ctrl-b d` / `Ctrl-b q` from either view.
- TUI scope view (docker): container table (NAME / IMAGE / STATE / STATUS / PORTS) with selected-row `▶` marker; three lifecycle states `Connecting` (pre-first-event) / `Available` (table; `Empty` sub-state for 0 containers — distinct from Unavailable) / `Unavailable { reason }` (verbatim daemon reason + "Tepegöz will keep retrying" note); navigation ↑↓ / j k / g G / Home End; filter with `/` (name + image substring, case-insensitive), `Enter` commits, `Esc` clears, `Backspace` edits; filter-bar shows caret `_` while active; selection clamps to visible (filter-respecting) count on every state/filter change.
- `AppAction::ShowToast { kind: Info|Success|Error, message }` wire surface: C2c2 routes `Payload::Error` and `DockerActionResult::Failure` to `ToastKind::Error`; runtime stubs as `tracing::warn!` (Error) / `info!` (Info|Success); C3 implements the actual overlay. Success toasts are deferred to C3 because they need `pending_actions` context to reference what the user acted on.
- TUI redraw cadence: in Pane mode there is no draw loop (raw stdout passthrough). In Scope mode a constant 30 Hz `tokio::time::interval` ticks `AppEvent::Tick`, which the App responds to with `AppAction::DrawScope`; the 30 Hz tick is gated off when not in Scope mode (no CPU cost when unused). Implementation diverges slightly from the original "draw on event arrival, capped at 30 Hz" proposal — the constant-tick approach is simpler, ratatui's buffer diff makes redundant draws cheap, and the constant rate provides natural backpressure for C3's logs streaming.
- Mode switch mechanics: Pane → Scope clears the screen and starts the ratatui draw cycle. Scope → Pane clears the screen, sends `Unsubscribe(prev_pane_sub)` (which the daemon now actually honors — see the bug fix above), and sends `AttachPane(new_pane_sub)` so the daemon replays current scrollback as `PaneSnapshot`. **Byte-level vim-preservation across the synthetic re-attach is verified** by `tests/vim_preservation.rs`: a real `/bin/sh` pane emits vim-style escape sequences (alt-screen entry, cursor positioning, marker text); after the synthetic re-attach the new `PaneSnapshot` contains all three. **Eyeball confirmation in a real terminal is still required** before C2 rendering work is signed off — see `docs/OPERATIONS.md` Slice C demo. CTO §3 fallback options if the eyeball reveals problems: (a) Resize-after-attach to force vim's redraw, (b) emit Ctrl-L equivalent, (c) keep AttachPane alive across mode switches.
- Daemon handshake: rejects protocol-version mismatches at the socket level. Both `Envelope.version` and `Hello.client_version` must equal the daemon's `PROTOCOL_VERSION`; otherwise the daemon sends a structured `Error(VersionMismatch)` naming both versions and closes. Without this guard a v3 client connecting to a v4 daemon would silently handshake and later trip an opaque rkyv decode error.
- Panes inherit `TEPEGOZ_PANE_ID=<id>` in env. TUI refuses to run if its own env has that var (prevents recursive attach feedback loop).
- Shells spawn in the TUI's `current_dir()` rather than `$HOME` (portable-pty's default).
- TUI sets the terminal window title to `tepegoz · pane N` on attach (OSC 0) and clears it on detach, so an attached pane is visually distinct from the outer shell.

## Test coverage (80 tests, all green)

- `tepegoz-proto::codec` — 11 (envelope/status roundtrip, frame-too-large guard, Subscribe(Docker | DockerLogs) roundtrips, ContainerList / DockerUnavailable / ContainerLog / ContainerStats event roundtrips, DockerAction request + result roundtrips)
- `tepegoz-pty` — 4 (scrollback eviction, scrollback snapshot, subscribe-no-duplicates, cwd+pane_id env)
- `tepegoz-docker` — 7 (socket discovery order, discovery without HOME, into_wire translation, into_wire empty-state default, stats_to_wire CPU% formula, stats_to_wire zero-cpu fallback, stats_to_wire missing-memory-section default)
- `tepegoz-tui::input` — 12 (InputFilter pass-through, detach-d, detach-q, switch-to-scope, switch-to-pane, help, non-detach, detach-splits-stream, switch-splits-stream, detach split-across-chunks, switch split-across-chunks, double-Ctrl-B)
- `tepegoz-tui::app` — 27 (App state machine: initial_actions; Ctrl-b d user detach; pane keystrokes → SendInput; Ctrl-b s → scope (subscribes Docker, state=Connecting); double-switch-to-scope idempotent; double-switch-to-pane idempotent; Ctrl-b ? in pane dropped; Ctrl-b a → pane with synthetic re-attach + docker Unsubscribe + docker state=Idle; ContainerList → Available; DockerUnavailable → Unavailable; j/k clamp bounds; arrow keys navigate; g/G top/bottom; filter narrows and clamps selection; filter Enter commits; filter Esc clears; filter Backspace edits; Payload::Error → ShowToast(Error); DockerActionResult::Failure → ShowToast(Error); DockerActionResult::Success does not toast in C2; PaneOutput/Snapshot in Pane → WriteStdout; PaneOutput in Scope dropped; PaneExit → Detach(PaneExited) with exit_code; stale sub ignored; Resize forwards + redraws only in Scope; Tick no-op in Pane, DrawScope in Scope)
- `tepegoz-tui::scope::docker` — 7 (headless render tests: Available state with names/images/states/selection marker; Connecting message; Unavailable with verbatim reason; Available-but-empty distinct "No containers" (not conflated with Unavailable); filter matching nothing shows "No containers match filter"; filter bar shows caret when active; ports column renders public + internal mappings)
- `crates/tepegoz-core/tests/daemon_persistence.rs` — 2 (phase-1 acceptance + handshake_rejects_protocol_version_mismatch)
- `crates/tepegoz-core/tests/pty_persistence.rs` — 1 (phase-2 acceptance)
- `crates/tepegoz-core/tests/pane_unsubscribe.rs` — 1 (Slice C2 gate: Unsubscribe cancels pane forwarder; pins fix for the JoinSet/HashMap bug that leaked a zombie per mode switch)
- `crates/tepegoz-core/tests/vim_preservation.rs` — 1 (Slice C2 gate: vim-style escape sequences round-trip through synthetic re-attach at the byte level. Eyeball confirmation in a real terminal is still required — see `docs/OPERATIONS.md` Slice C demo)
- `crates/tepegoz-core/tests/docker_scope.rs` — 7 (phase-3 acceptance):
  - Subscribe(Docker) emits ContainerList xor DockerUnavailable (always-on)
  - Opt-in `TEPEGOZ_DOCKER_TEST=1` insists on Available for Subscribe(Docker)
  - DockerAction against unreachable engine returns Failure with non-empty reason (always-on)
  - Subscribe(DockerLogs) against unreachable engine emits DockerStreamEnded (always-on)
  - Subscribe(DockerStats) against unreachable engine emits DockerStreamEnded (always-on)
  - Opt-in `TEPEGOZ_DOCKER_TEST=1` end-to-end: provisions an alpine container, observes a log marker, observes a stats sample with mem_bytes > 0, performs Restart and asserts Success
  - **C2c3 latency pin** (opt-in): provisions a unique alpine container, subscribes to Docker, asserts first ContainerList arrives in <2 s *and* contains the provisioned container by name

## Next (user action + slice)

**C2c3 eyeball demo — pending user run.** The automated test landed (pin ≤2 s latency + container-by-name). The manual demo in `docs/OPERATIONS.md` has to be driven by a human in a real terminal. Step 1 is vim-preservation (make-or-break — stop and apply the fallback from `docs/ISSUES.md` if it fails). Step 3 is the CTO §7 engine-unavailable-mid-session recovery check.

**Slice C3 — action keybinds + toasts + logs panel.** After the eyeball demo signs off: `r/s/K/X/l` keybinds (K and X require `y` confirm per CTO §4), toast overlay implementation (the wire is already in place via `AppAction::ShowToast`), pending-action 30 s timeout sweep, logs panel via `Subscribe(DockerLogs)`. Then Slice D (`DockerExec` → new pty pane via the `RequestOpenPane(PaneRequestKind::DockerExec { ... })` placeholder in the C1 event bus).
