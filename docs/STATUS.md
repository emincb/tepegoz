# Status

## Current state

**Phase 2 tests pass; one user-visible acceptance bug under diagnosis.** Phase 3 blocked on Phase 2 clearance.

Last commit: `f12d194` (diagnostic tracing shipped for the Phase 2 bug). See `docs/ISSUES.md#active` for the active issue.

## Phase matrix

| # | Name | Status | Commit(s) | Acceptance test(s) |
|---|---|---|---|---|
| 0 | Scaffold | ✅ | `81c7731` | `tepegoz --help`, green CI |
| 1 | Proto + daemon + TUI round-trip | ✅ | `3715bf9` | `daemon_persistence.rs` |
| 2 | Local pty multiplex + persistence | 🟡 tests green, UX bug | `eab274c`, `321ed5e`, `f12d194` | `pty_persistence.rs`, `subscribe_does_not_duplicate_bytes`, `pane_honors_cwd_and_exposes_pane_id_env` |
| 3 | Docker scope panel | ⚪ blocked | — | — |
| 4 | Ports + processes panels (local) | ⚪ | — | — |
| 5 | SSH transport + remote pty | ⚪ | — | — |
| 6 | Agent binary + remote scopes | ⚪ | — | — |
| 7 | Port scanner | ⚪ | — | — |
| 8 | Recording + replay | ⚪ | — | — |
| 9 | Claude Code pane awareness | ⚪ | — | — |
| 10 | QUIC hot path + release 0.1.0 | ⚪ | — | — |

Status key: ✅ complete · 🟡 code+tests green, user acceptance pending · 🟠 in progress · ⚪ not started.

## What works end-to-end (as of `f12d194`)

- Daemon binds user-scoped Unix socket at `$XDG_RUNTIME_DIR/tepegoz-$uid/daemon.sock` (fallback `$TMPDIR` or `/tmp`). Parent dir `0700` when default path (we own it), socket `0600`. Override paths leave parent perms alone.
- Stale-socket eviction on startup. Refuses to start under another live daemon. Graceful SIGINT shutdown with socket cleanup.
- Wire protocol v2: rkyv-archived `Envelope { version, payload }`, 4-byte big-endian length prefix, bytecheck validation on every read. Messages:
  - Commands: `Hello`, `Ping`, `Subscribe(Status)`, `Unsubscribe`, `OpenPane`, `AttachPane`, `ClosePane`, `ListPanes`, `SendInput`, `ResizePane`
  - Responses: `Welcome`, `Pong`, `PaneOpened`, `PaneList`, `Error`
  - Events (in `Event(EventFrame)`): `Status`, `PaneSnapshot`, `PaneOutput`, `PaneExit`, `PaneLagged`
- PTY manager owns panes; per-pane 2 MiB ring buffer (`VecDeque<Bytes>` with total-byte accounting, oldest-chunks-evict on overflow); per-pane reader + waiter threads; `tokio::sync::broadcast` channel for subscribers.
- Daemon client session: single dedicated writer task drains an mpsc of outgoing envelopes — no per-write locks. Each `AttachPane` spawns a forwarder task.
- TUI is a raw-passthrough attacher: raw mode + alternate screen; stdin → `SendInput`; `PaneOutput` → stdout verbatim; `SIGWINCH` → `ResizePane`. Detach via `Ctrl-b d` or `Ctrl-b q`.
- Panes inherit `TEPEGOZ_PANE_ID=<id>` in env. TUI refuses to run if its own env has that var (prevents recursive attach feedback loop).
- Shells spawn in the TUI's `current_dir()` rather than `$HOME` (portable-pty's default).

## Test coverage (15 tests, all green)

- `tepegoz-proto::codec` — 3 (envelope roundtrip, status roundtrip, frame-too-large guard)
- `tepegoz-pty` — 4 (scrollback eviction, scrollback snapshot, subscribe-no-duplicates, cwd+pane_id env)
- `tepegoz-tui::input` — 7 (InputFilter: pass-through, detach-d, detach-q, non-detach, split-across-chunks, double-Ctrl-B)
- `crates/tepegoz-core/tests/daemon_persistence.rs` — 1 (phase-1 acceptance)
- `crates/tepegoz-core/tests/pty_persistence.rs` — 1 (phase-2 acceptance)

## Pending to clear Phase 2

1. Diagnose the TUI immediate-detach bug (see `docs/ISSUES.md#active`). Diagnostic tracing in `f12d194` logs every stdin read and filter action to `~/.cache/tepegoz/tui.log`.
2. Land root-cause fix + regression test.
3. Revert or demote diagnostic tracing to `debug` level.
4. Mark Phase 2 ✅; unblock Phase 3.

## Next phase

**Phase 3 — Docker scope panel.** Details in `docs/ROADMAP.md#phase-3--docker-scope-panel`.
