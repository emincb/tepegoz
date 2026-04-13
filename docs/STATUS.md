# Status

## Current phase
**Phase 1 — Complete.** Next: Phase 2.

## Demonstrable state

### Phase 0 — Scaffold (complete)
Workspace compiles on mac + linux × x86_64 + arm64. `tepegoz --help` works. CI runs fmt, clippy `-D warnings`, native tests, and cross-build matrix via `cargo-zigbuild`.

### Phase 1 — Daemon ↔ TUI (complete)
- `tepegoz daemon` binds a per-user Unix socket at `$XDG_RUNTIME_DIR/tepegoz-$uid/daemon.sock` (fallback to `$TMPDIR` or `/tmp`). Default-path parent is chmod `0700`; socket is `0600`. Override paths (`--socket`) leave parent perms alone (user-managed).
- Wire protocol: rkyv-archived `Envelope { version, payload }` with length-prefix framing, `bytecheck` validation on every `read_envelope`. Messages: `Hello/Welcome/Ping/Pong/Subscribe/Unsubscribe/Event/Error`.
- `tepegoz tui` connects, handshakes (Hello → Welcome), subscribes to `Status`, and renders a live panel at 1 Hz. Tracing goes to `${XDG_CACHE_HOME:-$HOME/.cache}/tepegoz/tui.log` (or `$TEPEGOZ_LOG_FILE` override) to avoid corrupting the display.
- Graceful shutdown: daemon handles SIGINT/SIGTERM, removes socket on exit, waits briefly for in-flight clients.
- Stale-socket detection: if a socket file exists but no daemon responds, it's evicted; if another daemon responds, startup refuses.
- **Acceptance test:** `tepegoz-core/tests/daemon_persistence.rs` spawns the daemon, connects client #1, captures snapshot, drops, reconnects as client #2, and asserts `clients_total` incremented, `uptime_seconds` did not regress, and `daemon_pid` stayed identical. Passes green.

### Demo commands
```sh
cargo build
# terminal 1
./target/debug/tepegoz daemon
# terminal 2
./target/debug/tepegoz tui
# quit with q or Esc; reopen; uptime continues, clients_total increments.
```

## Next phase
**Phase 2 — Local pty multiplex + persistence.** Daemon owns ptys, per-pane ring buffer (2 MB default), tiled layout in TUI, detach/reattach with backlog replay.

## Full roadmap
See `CLAUDE.md` for the phase list. Target release 0.1.0 at end of Phase 10.
