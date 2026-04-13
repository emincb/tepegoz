# Operations

Build, test, run, debug. Kept terse and factual.

## Dev environment

- Rust 1.94.1 pinned via `mise.toml`; mise auto-activates on `cd` into the repo.
- First-time setup:
  ```sh
  cd ~/Documents/projects/personal/tepegoz
  mise install     # pulls rust 1.94.1 if not already installed
  cargo --version  # → 1.94.1
  ```

## Build / lint / test

```sh
cargo build                                              # debug build
cargo build --release                                    # release build
cargo fmt --all                                          # format in place
cargo fmt --all -- --check                               # verify format (CI uses this)
cargo clippy --workspace --all-targets -- -D warnings    # strict clippy (CI uses this)
cargo test --workspace                                   # all tests
cargo test -p tepegoz-core --test pty_persistence        # single integration test
```

## Running daemon + TUI

Terminal 1 (daemon):
```sh
./target/debug/tepegoz daemon
# stdout carries tracing output; SIGINT (Ctrl-C) for graceful shutdown
```

Terminal 2 (TUI, from any directory — shell will spawn with that as cwd):
```sh
./target/debug/tepegoz tui
# lands in a shell in the daemon's pty
# detach with Ctrl-b d (or Ctrl-b q)
# reattach with another `./target/debug/tepegoz tui` — scrollback replays
```

Useful flags:
- `--socket /path/to/sock` — override daemon's socket path; TUI must match
- `--log-level debug` or `RUST_LOG=debug` — more verbose tracing
- `TEPEGOZ_LOG_FILE=/path/to/tui.log` — override TUI log destination

## Log locations

| Binary | Destination | Why |
|---|---|---|
| `tepegoz daemon` | stdout | visible to the operator; redirect with `> file.log` if you want persistence |
| `tepegoz tui` | `~/.cache/tepegoz/tui.log` (or `$TEPEGOZ_LOG_FILE`) | stdout is the pty passthrough; would corrupt display |
| `tepegoz agent` (Phase 6) | stderr | stdin/stdout reserved for the protocol |

Tail live:
```sh
tail -f ~/.cache/tepegoz/tui.log
```

## Slice C1.5c manual demo prep (Phase 3)

The TUI god view is the part where eyeball-confirmation has historically diverged from test-passes (Phase 2 immediate-detach was exactly this). C1.5c acceptance is a manual demo against a standing fixture container. The gating checks per CTO direction: god-view layout renders on first launch with no config; focus navigation (`Ctrl-b h/j/k/l` + arrows) feels natural; vim in the pty tile renders correctly and survives focus movement; Docker tile populates within ~2 s; placeholder tiles are clearly labeled and non-interactive; detach/reattach preserves state; engine-unavailable-mid-session recovers cleanly.

Prior to C1.5 this section documented a Scope→Pane mode-switch demo with Ctrl-b s / Ctrl-b a. Those keys no longer exist — the tiled layout shows pty + scopes simultaneously. The victim-container prep below carries over unchanged from the earlier C2c3 version.

### Prep

```sh
# Standing victim container — the demo's fixture for the scope panel.
# Produces continuous log output (for the `l` keybind, C3), is safe to
# Restart/Kill/Remove (no state-loss risk), lives long enough for stats
# sampling to settle.
docker run -d --name tepegoz-slice-c-victim alpine sh -c \
  "i=0; while true; do echo tick-\$i; i=\$((i+1)); sleep 1; done"

# Build + start daemon (terminal 1):
cargo build
./target/debug/tepegoz daemon
```

### Demo sequence

Run `./target/debug/tepegoz tui` in terminal 2.

**Step 1 — god-view layout on first launch.**

```
# → expect on first launch with no setup:
#     · PTY tile spanning the top (shell prompt visible, focused by default)
#     · Docker tile bottom-left (populated within ~2 s with the victim
#       container; state "running", the tick-N image, port column empty)
#     · Ports tile bottom-middle — labeled placeholder "Ports — Phase 4",
#       dimmed border
#     · Fleet tile bottom-right — labeled placeholder "SSH Fleet — Phase 5"
#     · Claude Code strip bottom-full-width — labeled placeholder
#       "Claude Code — Phase 9"
#   Focused tile has a bright cyan border; unfocused tiles are dim gray.
```

**Step 2 — focus navigation.**

```
Ctrl-b j              # PTY → Docker tile (border becomes bright on Docker)
Ctrl-b l              # Docker → Ports (placeholder; focused dim-cyan,
                      #   "Phase 4 — not yet implemented" hint appears below
                      #   the label)
Ctrl-b k              # Ports → PTY (same column, one row up — bright on PTY)
Ctrl-b <Down arrow>   # same as Ctrl-b j — arrow equivalents
Ctrl-b <Up arrow>     # back to PTY

# → expect: movement feels natural; focused tile is visually distinct;
#   unfocused tiles keep rendering (Docker table ticks through live,
#   PTY shell continues to produce output).
```

**Step 3 — vim preservation in the pty tile (MAKE-OR-BREAK).**

The automated proxy (`crates/tepegoz-core/tests/vim_preservation.rs`,
now a vt100 reconstruction test) passes in CI; this is the real
terminal check — vt100 output is what the user will actually see.

```
# Focus PTY if not already focused (Ctrl-b k from the scope row, or
# default focus on fresh launch).
vim /tmp/tepegoz-demo.txt
# press `i` (insert mode)
# type: HELLO FROM STEP 3
# press <Esc>
# Status line should read: "/tmp/tepegoz-demo.txt" [New File]
# Move cursor with h/l/j/k to a non-trivial position.

Ctrl-b j              # focus Docker; vim stays on-screen in the pty tile
Ctrl-b k              # focus PTY again
# → expect: vim's screen intact (text, cursor, status line). The tile
#   border changed cyan/gray but the vim buffer did not scramble.

# Detach + reattach:
Ctrl-b d              # detach
./target/debug/tepegoz tui
# → expect: vim's screen still visible in the pty tile on reattach.

# If vim's screen is broken:
# STOP. This is the vt100 rendering equivalent of CTO §3's concern.
# Debug: check `tui.log` for ratatui draw errors; verify the vt100
# crate is processing all received bytes (log PaneOutput byte counts
# vs parser.screen() contents).

# Exit vim:
# :q!
```

**Step 4 — docker tile navigation + filter.**

```
Ctrl-b j              # focus Docker tile
j, k, or ↑/↓          # move selection (▶ marker tracks)
g / G                 # jump to top / bottom
/ tepegoz             # open filter input, type "tepegoz"; list narrows
<Enter>               # commit filter (bar stays; caret disappears)
<Esc>                 # clear filter entirely

# → expect: while Docker is focused, plain j/k/g/G act on the list (not
#   focus). Ctrl-b j/k/etc. continue to move focus between tiles.
#   The PTY tile keeps rendering in the background.
```

**Step 5 — engine-unavailable-mid-session recovery** (CTO §7).

```
# With the Docker tile showing containers (focused or not — the
# subscription is always alive):
# Kill the docker daemon from OUTSIDE tepegoz.
#   macOS Docker Desktop: menu → Quit
#   macOS Colima:          `colima stop`  (terminal 3)
#   Linux:                 `sudo systemctl stop docker`  (terminal 3)
# → expect: within ~5 s the Docker tile swaps to the Unavailable state
#   (red border, "Docker engine unavailable", verbatim reason). The
#   TUI must NOT crash or hang; PTY tile + placeholder tiles continue
#   rendering unchanged.

# Restart docker:
#   macOS Docker Desktop: launch the app
#   macOS Colima:          `colima start`
#   Linux:                 `sudo systemctl start docker`
# → expect: within ~5 s (daemon reconnect interval) the Docker tile
#   swaps back to the container table. The victim container reappears
#   if still running.

# (If the stop removed the container, just `docker run -d --name
# tepegoz-slice-c-victim alpine sh -c "..."` again.)
```

**Step 6 — tiny-terminal fallback (optional).**

Resize the terminal window below 80×24. The entire layout collapses
to a single bordered "Terminal too small for god view — Resize to at
least 80×24" tile. Resize back up; the god view reappears with PTY
focused and all state preserved.

**Step 7 — C3 keybinds** (enabled only after Slice C3 lands):

```
l       → open logs panel for selected row; tick-N output streams
r       → restart selected container; toast confirms; table updates ~2s
s       → stop selected container; toast confirms
K, y    → kill with confirm; n cancels
X, y    → force-remove with confirm; n cancels
Enter   → exec into container (Slice D; opens new pane)
```

### Tear down

```sh
docker rm -f tepegoz-slice-c-victim
```

## Common issues

### POSIX `printf` portability in integration tests
Integration tests that drive `/bin/sh` must use POSIX-portable syntax. `printf '\xNN'` (hex) works on macOS `/bin/sh` (bash in POSIX mode — accepts the GNU extension) but NOT on Linux `/bin/sh` (dash, Ubuntu default — strict POSIX, hex escapes emitted literally). Use `\NNN` octal instead. CI (ubuntu-latest + macos-latest) will catch this, but local macOS runs will not. See `vim_preservation.rs` for the canonical example.

### "another tepegoz daemon is already running"
Another daemon holds the socket. `pkill -f "tepegoz daemon"` then retry. (Or use `--socket /different/path` to run a second daemon.)

### "no daemon socket at ... — is `tepegoz daemon` running?"
TUI can't find the daemon. Confirm daemon is running; confirm socket paths match (both default to the same value unless overridden).

### "this shell is already inside tepegoz pane N"
You ran `tepegoz tui` from inside a shell that's already a tepegoz pane. Detach with `Ctrl-b d` and run `tepegoz tui` from the outer terminal.

### Shell starts in `$HOME` instead of `cwd`
Fixed in `321ed5e`. Rebuild (`cargo build`) after pulling.

### TUI detaches immediately
Active bug tracked in `docs/ISSUES.md#active`. Diagnostic tracing shipped at `f12d194`; share `~/.cache/tepegoz/tui.log` for diagnosis.

### Terminal stuck in weird state after a crash
If the TUI's `TerminalGuard` didn't run (rare; only on abort/kill -9), raw mode may persist. `reset` or `stty sane` restores. The guard runs on panic (unwind) and normal exit.

### Daemon refuses to start — "Operation not permitted"
You passed `--socket /tmp/foo.sock` where `/tmp` isn't owned by you. For default path (XDG_RUNTIME_DIR / TMPDIR / /tmp fallback under `tepegoz-<uid>/`), the parent is auto-created 0700.

## Diagnosing the running daemon

```sh
# is it running?
ps aux | grep "tepegoz daemon" | grep -v grep

# is the socket there?
ls -l "${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/tepegoz-$(id -u)/daemon.sock"

# what's it logging? check its terminal's stdout, or the file you redirected to.
```

## Release process (Phase 10)

TBD. Activated at Phase 10 per `docs/ROADMAP.md#phase-10--quic-hot-path--release-010--`.
