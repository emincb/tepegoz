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

## Slice C manual demo prep (Phase 3)

The TUI scope view is the part where eyeball-confirmation has historically diverged from test-passes (Phase 2 immediate-detach was exactly this). Acceptance for C2/C3 includes a manual demo against a standing fixture container. **Step 1 is make-or-break**: if vim-preservation across the Scope→Pane synthetic re-attach fails, stop and apply a fallback mitigation (see `docs/ISSUES.md`) before anything else.

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

**Step 1 — vim preservation (MAKE-OR-BREAK).** The byte-level proxy
(`crates/tepegoz-core/tests/vim_preservation.rs`) passes in CI, but this
is the real-terminal check. If it fails, apply the fallback from
`docs/ISSUES.md` — Resize-after-attach first.

```
# In the attached pane:
vim /tmp/tepegoz-demo.txt
# press `i` (insert mode)
# type: HELLO FROM STEP 1
# press <Esc>
# status line should read something like: "/tmp/tepegoz-demo.txt" [New File]
# move cursor with h/l/j/k to some non-trivial position

# Switch to scope view:
Ctrl-b s
# → expect: container table populated with tepegoz-slice-c-victim.
#   state "running", the tick-N image, port column empty.

# Switch back to attached pane:
Ctrl-b a
# → expect: vim's screen intact. Status line still shows the file name.
#   The text "HELLO FROM STEP 1" is visible in the buffer. Cursor
#   position preserved. No garbled escape sequences.

# If vim's screen is broken (blank screen, wrong cursor, garbled text):
# STOP HERE. This is the case CTO §3 warned about. Apply mitigation (1)
# from docs/ISSUES.md ("Resize-after-attach") and re-test. Escalate to
# (2) only if (1) doesn't fix it.

# If vim is intact, exit vim:
# :q!
```

**Step 2 — scope rendering, navigation, filter.**

```
Ctrl-b s                    # switch back to scope view
j, k, or ↑/↓                # move selection (▶ marker tracks)
g / G                       # jump to top / bottom
/ tepegoz                   # open filter input, type "tepegoz"; list narrows
<Enter>                     # commit filter (bar stays; caret disappears)
<Esc>                       # clear filter entirely
```

**Step 3 — engine-unavailable-mid-session recovery** (CTO §7 Step 10).

```
# In scope view, still showing containers:
# Kill the docker daemon from OUTSIDE tepegoz.
#   macOS Docker Desktop: menu → Quit
#   macOS Colima:          `colima stop`  (terminal 3)
#   Linux:                 `sudo systemctl stop docker`  (terminal 3)
# → expect: within ~5s the scope view swaps to the Unavailable panel
#   (red border, "Docker engine unavailable", verbatim reason from the
#   daemon). The TUI must NOT crash or hang.

# Restart docker:
#   macOS Docker Desktop: launch the app
#   macOS Colima:          `colima start`
#   Linux:                 `sudo systemctl start docker`
# → expect: within ~5s (daemon reconnect interval) the scope view swaps
#   back to the container table. The victim container should reappear
#   if still running, or the list may be empty if docker was stopped
#   long enough that the container was removed.

# (If `docker run` removed the container during the stop — e.g., with
# --rm on SIGTERM — just `docker run -d --name tepegoz-slice-c-victim
# alpine sh -c "..."` again.)
```

**Step 4 — detach + reattach (Phase 2 invariant).**

```
Ctrl-b d                    # detach (daemon + pane still running)
./target/debug/tepegoz tui  # reattach — scrollback replay visible
```

**Step 5 — C3 keybinds** (enabled only after Slice C3 lands):

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
