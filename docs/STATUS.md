# Status

## Current phase
**Phase 2 — Complete.** Next: Phase 3 (Docker scope).

## Demonstrable state

### Phase 0 — Scaffold (complete)
Workspace compiles on mac + linux × x86_64 + arm64. `tepegoz --help` works. CI runs fmt, clippy `-D warnings`, native tests, and cross-build matrix via `cargo-zigbuild`.

### Phase 1 — Daemon ↔ TUI wire protocol (complete)
- Daemon binds a per-user Unix socket (`$XDG_RUNTIME_DIR/tepegoz-$uid/daemon.sock`, fallback `$TMPDIR` or `/tmp`). Default-path parent chmod 0700; socket 0600. Override paths leave parent perms alone.
- Wire protocol: rkyv-archived `Envelope { version, payload }` with length-prefix framing and `bytecheck` validation on every read.
- `Hello/Welcome/Ping/Pong/Subscribe/Unsubscribe/Event/Error` messages; status subscription streams `StatusSnapshot` at 1 Hz.
- Graceful SIGINT shutdown; stale-socket detection refuses startup under an existing daemon.
- **Test:** `daemon_persistence.rs` — `clients_total` increments across reconnect, `uptime_seconds` doesn't regress, pid stable.

### Phase 2 — Local pty multiplex + persistence (complete)
- `tepegoz-pty` crate: `PtyManager` owns a `HashMap<PaneId, Arc<Pane>>`. Each pane wraps a portable-pty master, a blocking reader thread, a waiter thread, and a `tokio::sync::broadcast` channel.
- Per-pane ring buffer (2 MiB default): `VecDeque<Bytes>` with total-byte accounting; oldest chunks drop on overflow.
- Wire protocol extended (`PROTOCOL_VERSION` = 2): `OpenPane`, `AttachPane`, `ClosePane`, `ListPanes`, `SendInput`, `ResizePane`; daemon events `PaneSnapshot`, `PaneOutput`, `PaneExit`, `PaneLagged`; responses `PaneOpened`, `PaneList`.
- TUI is a raw-passthrough attacher: enters raw mode + alt screen, pipes stdin → `SendInput`, pipes `PaneOutput` → stdout verbatim, handles SIGWINCH → `ResizePane`. Detach via `Ctrl-b` then `d` or `q`.
- Daemon client session uses a single writer task draining an mpsc channel — every outgoing frame is serialized without per-write locks. Each `AttachPane` spawns a forwarder task that translates pane broadcast events into protocol events keyed by subscription id.
- **Correctness**:
  - Reader holds the scrollback lock across both append and broadcast — a subscriber sees each byte in exactly one of {snapshot, live stream}, never both. Prevents duplicated output on attach (was visible as doubled prompts/lines).
  - New panes start in the TUI's current working directory, not `$HOME`. portable-pty defaults to `$HOME` when cwd is unset; the TUI now passes `std::env::current_dir()`.
  - Daemon stamps `TEPEGOZ_PANE_ID=<id>` into every pty's env. The TUI refuses to run if that var is set, preventing a recursive `tepegoz tui` inside an already-attached pane (would feed output back into itself infinitely).
- **Tests** (all green):
  - `pty_persistence.rs`: client #1 opens pane, sends `echo MARKER_ALPHA\n`, verifies output; drops; client #2 reconnects, re-attaches, receives `PaneSnapshot` containing `MARKER_ALPHA` from the ring buffer.
  - `tepegoz-pty::tests::subscribe_does_not_duplicate_bytes`: drives 50 markers mid-stream, asserts each appears exactly once across snapshot + live.
  - `tepegoz-pty::tests::pane_honors_cwd_and_exposes_pane_id_env`: shell starts in requested cwd and `$TEPEGOZ_PANE_ID` is exported.

### Demo commands
```sh
cargo build
# terminal 1
./target/debug/tepegoz daemon
# terminal 2
./target/debug/tepegoz tui     # opens a shell, you land in it
# type anything, run commands
# detach: Ctrl-b then d         (daemon + shell keep running)
./target/debug/tepegoz tui     # reattach — scrollback replays, you keep going
# pane exits on its own (e.g. `exit`) → TUI shows "[pane N exited]"
```

Daemon logs to stdout. TUI logs to `${XDG_CACHE_HOME:-$HOME/.cache}/tepegoz/tui.log` (or `$TEPEGOZ_LOG_FILE`) to avoid corrupting the display.

## Next phase
**Phase 3 — Docker scope panel.** `bollard` integration, socket discovery (Docker Desktop, Colima, Rancher, Linux native), container list + log tail + exec-into-pane + lifecycle actions. This is the first scope panel; its UX pattern will set the template for ports/processes/logs in Phase 4.

## Full roadmap
See `CLAUDE.md` for the phase list. Target release 0.1.0 at end of Phase 10.
