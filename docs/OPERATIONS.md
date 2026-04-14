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

### Tear down

```sh
docker rm -f tepegoz-slice-c-victim
```

## Slice C3 manual demo prep (Phase 3)

C3 adds action keybinds (`r`/`s`/`K`/`X`), the toast overlay, pending-action timeout sweep, and the logs sub-state inside the Docker tile. Automated coverage is in `tepegoz-tui::app::tests` + `tepegoz-tui::scope::docker::tests` + `tepegoz-tui::toast::tests` (hermetic) and `crates/tepegoz-core/tests/docker_scope.rs::restart_propagates_to_follow_up_container_list` (opt-in `TEPEGOZ_DOCKER_TEST=1`, end-to-end). This section is the real-terminal eyeball check the user runs before Phase 3 closes.

### Prep

Reuse the standing victim from C1.5c if still running, or provision fresh:

```sh
docker run -d --name tepegoz-slice-c-victim alpine sh -c \
  "i=0; while true; do echo tick-\$i; i=\$((i+1)); sleep 1; done"

# Second victim — short-lived, for the stream-ended demo (Step 8).
docker run -d --name tepegoz-stream-ended-victim alpine sh -c \
  "echo hello; sleep 3"

cargo build
./target/debug/tepegoz daemon    # terminal 1
./target/debug/tepegoz tui        # terminal 2
# → Ctrl-b j to focus the Docker tile; the victims should appear.
```

### Demo sequence

**Step 1 — `r` / `s` immediate dispatch + Success toast.**

```
# Focus Docker; select the tick-victim row (j/k to move ▶).
r                    # immediate Restart
# → expect: a green "ok: Restart tepegoz-slice-c-victim — succeeded"
#   toast appears as a 1-line strip just above the Claude Code tile.
#   Auto-dismiss after ~3 s. The Docker table continues ticking; the
#   container's "STATUS" column shifts (e.g. "Up 12 seconds" → "Up
#   Less than a second") on the next ~2 s refresh.
s                    # immediate Stop
# → expect: another green toast for the Stop. The table shows the
#   container in "exited" state on the next refresh. Run
#   `docker start tepegoz-slice-c-victim` in terminal 3 to bring it
#   back for the remaining steps.
```

**Step 2 — capital `R` is a no-op (case-discipline lock).**

```
# With the tick-victim selected and Docker tile focused:
<Shift>r             # press capital R
# → expect: NOTHING. No toast, no modal, no envelope to the daemon.
#   Case-discipline rule: capital = destructive (K/X only); lowercase
#   = safe (r/s) and navigation (j/k/h/l). The previous C3a aliases
#   (r|R, s|S) were removed per push-back. Verified by
#   capital_r_is_noop_when_docker_focused_after_case_discipline_lock
#   in the state-machine tests; this is the real-terminal confirmation.
```

**Step 3 — `K` / `X` confirm modal + K→K absorption.**

```
# With tick-victim selected:
K                    # capital K
# → expect: a centered bordered box appears inside the Docker tile's
#   Rect (NOT full-screen) with " confirm " in the title, "Kill
#   container tepegoz-slice-c-victim?" body, "[y] confirm · any
#   other key cancels" hint. The pty tile + placeholder tiles stay
#   visible and live around it. Help bar swaps to the confirm-mode
#   hint.

K                    # press K AGAIN while the Kill modal is open
# → expect: ABSORBED. The modal stays showing the original Kill
#   confirm (same container, same deadline — the 10 s auto-cancel
#   does NOT reset). Per C3b UX clarification: K/X during an open
#   confirm must not switch the target or refresh the deadline.

X                    # try X (the other destructive key)
# → expect: also ABSORBED. Modal still shows "Kill container …?",
#   not "Remove container …?".

n                    # cancel
# → expect: modal disappears, no envelope sent. (Also test: Esc
#   cancels; any arbitrary non-y/K/X key cancels.)

# Now exercise the actual confirm-and-dispatch:
X                    # open Remove confirm
y                    # confirm
# → expect: toast "ok: Remove tepegoz-slice-c-victim — succeeded",
#   container vanishes from the table on the next refresh.
```

**Step 4 — Failure toast with verbatim engine reason.**

Requires a container that refuses the action. Stop the engine
briefly to force the Failure path:

```
# Terminal 3: start a fresh victim, then IMMEDIATELY stop docker.
docker run -d --name tepegoz-fail-victim alpine sleep 120
# macOS Docker Desktop: menu → Quit
# macOS Colima:          colima stop
# Linux:                 sudo systemctl stop docker

# In the TUI (Docker tile will swap to Unavailable within ~5 s; wait
# for it, then restart docker so the list repopulates).
# Start docker. Wait for the tile to recover.
# Focus Docker, select tepegoz-fail-victim, then FORCE a failure
# by killing docker AGAIN between the selection and the keypress:
# (this is the race — if the engine vanishes mid-action.)

# Or the simpler way: `docker rm -f tepegoz-fail-victim` from the
# outside, then in the TUI press `r` against the now-gone row
# (the list may still show it for up to 2 s until the next refresh).
docker rm -f tepegoz-fail-victim
# In the TUI, with the stale row still selected:
r
# → expect: red "err: Restart tepegoz-fail-victim failed: <engine
#   reason>" toast. The reason text is VERBATIM from dockerd ("No
#   such container: tepegoz-fail-victim" or similar). Auto-dismiss
#   after ~8 s (longer than Success — user needs time to read).
```

**Step 5 — 30 s pending-action timeout toast.**

```
# The daemon + engine must be reachable when the action is sent, but
# the engine must hang or vanish between send and response. Easiest
# way: ask for a restart of the tick-victim, then immediately SIGSTOP
# dockerd so the action stalls. After 30 s the App emits a timeout
# toast.
# On macOS Colima: `pkill -STOP -x colima` (terminal 3).
# On Linux:        `sudo pkill -STOP dockerd` (terminal 3).
r                    # start Restart
# → within 30 s: red "err: Restart tepegoz-slice-c-victim timed
#   out — check engine" toast. Docker tile remains; no crash.
#   Resume docker (`pkill -CONT …`) and verify the tile recovers.
# Skip this step if you don't have a trivial way to stall dockerd
# locally — the state-machine tests cover this path.
```

**Step 6 — toast stacking + drop-oldest.**

```
# Produce 4 toasts in rapid succession. Easiest: restart the
# tick-victim three times quickly (each Success toast lasts 3 s),
# then a 4th any action.
r   r   r   r
# → expect: 3 toasts visible at once, stacked bottom-aligned above
#   the Claude Code strip. When the 4th arrives, the OLDEST (topmost
#   in the stack) silently disappears; 3 visible again. No keystrokes
#   blocked during this — navigation + other keybinds continue to
#   work unhindered.
```

**Step 7 — logs panel: enter, tail, scroll, exit.**

```
# Focus Docker, select tick-victim.
l
# → expect: Docker tile's content swaps from the container list to
#   a log transcript. Tile title becomes "docker · logs ·
#   tepegoz-slice-c-victim". Status line: "N lines · tail: on ·
#   stream: live". Help bar: "[j/k] scroll · [PgUp/PgDn] page · [G]
#   tail · [Esc/q] back". Lines begin arriving — "tick-0", "tick-1",
#   … one per second, auto-scrolling into view at the tail.

k                    # scroll up one line
# → expect: "tail: on" flips to "tail: off" (yellow). Scroll
#   position holds — new tick-N lines still append to the buffer
#   but the visible window stays put.

<PageUp>             # scroll up 10 lines
<PageDown>           # scroll down 10 lines
# → reaching offset 0 via PgDn re-enables tail: "tail: on".

G                    # jump to tail
# → "tail: on". Live lines visible again.

# Focus-persistence sanity: focus away and back.
Ctrl-b k             # focus PTY tile
Ctrl-b j             # focus Docker tile again
# → expect: logs view is still there, still tailing. (Unlike the
#   confirm modal, logs view persists across focus moves.)

<Esc>                # exit logs view
# → expect: Docker tile returns to the container list view. The
#   App sends Unsubscribe; the daemon stops the log stream.
# (q also exits; try it on the next entry to confirm.)
```

**Step 8 — stream-ended marker on container exit.**

```
# The second victim (tepegoz-stream-ended-victim) sleeps 3 s then
# exits. If it's already exited from earlier, recreate:
docker run -d --name tepegoz-stream-ended-victim alpine sh -c \
  "echo hello; sleep 3"
# → Immediately in the TUI: Ctrl-b j, select stream-ended-victim, l.
# → expect: transcript shows "hello", then after ~3 s a dimmed
#   italic line appears at the tail:
#     — log stream ended: <reason> —
#   Status line: "tail: off · stream: ended: <reason>". The reason
#   is verbatim from dockerd ("container exited" or similar). The
#   transcript stays scrollable; Esc / q returns to the list.
```

**Step 9 — tile-sized logs sanity check** (CTO addition; eyeball only):

```
# At 120×40 the Docker tile is approximately 40 cols × 17 rows. The
# logs sub-state renders inside that tile's Rect — not full-screen.
# Enter logs on tick-victim and verify:
#
# · "tick-N" lines are short and fit cleanly — should be obviously readable.
# · Produce some longer output to stress-test wrapping. Terminal 3:
docker exec tepegoz-slice-c-victim sh -c \
  'echo "{\"level\":\"info\",\"msg\":\"longer line that probably overflows 40 cols for eyeball testing of the ratatui Paragraph widget\",\"ts\":\"2026-04-14T12:00:00Z\"}"'
#
# → expect: long line renders readably. Exact behavior is whatever
#   ratatui's Paragraph does by default (currently: clip at right
#   edge; no horizontal scroll; no wrap). If the result looks awful
#   (unreadable / truncation obscures useful content), record the
#   gotcha in docs/ISSUES.md as a Phase-3-polish item ("logs
#   horizontal wrap or tile zoom") and proceed. Do NOT block C3
#   close on this — Phase-3 polish candidates are listed in
#   docs/STATUS.md's "Phase 3 polish candidates" section.
```

### Pass/fail matrix

| # | Scenario | Pass |
|---|---|---|
| 1 | `r`/`s` immediate dispatch + green Success toast + table refresh | ☐ |
| 2 | Capital `R` is a no-op (no toast, no modal, no envelope) | ☐ |
| 3 | `K`/`X` open inline modal; repeat K/X absorbs; y confirms; n / Esc / other cancel | ☐ |
| 4 | Failure toast renders verbatim dockerd reason text | ☐ |
| 5 | 30 s pending-action timeout emits "timed out — check engine" toast | ☐ |
| 6 | Toast stacking caps at 3; 4th drops oldest silently; keystrokes unblocked | ☐ |
| 7 | Logs panel: enters, tails live, scrolls, `G` resets tail, Esc / q returns | ☐ |
| 8 | `DockerStreamEnded` renders dimmed "— log stream ended: `<reason>` —" line | ☐ |
| 9 | Tile-sized logs sanity: lines render readably in cramped Docker tile | ☐ |

Sign off on rows 1–8 closes Phase 3 (row 3 in `docs/STATUS.md` → ✅). Row 9 is observational — record any gotchas in `docs/ISSUES.md` as a polish item; does NOT gate Phase 3 close.

### Tear down

```sh
docker rm -f tepegoz-slice-c-victim tepegoz-stream-ended-victim 2>/dev/null
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

### Ports scope rows show `partial: true` / empty pid / empty process name
The native ports probe (Phase 4 Slice 4a) attributes listening sockets to processes using `/proc/<pid>/fd` on Linux and `libproc pidfdinfo` on macOS. Neither requires root to see the caller's own sockets, but enumerating **other users'** sockets is privileged:

- **Linux**: the probe reads `/proc/<pid>/fd` per pid. Non-root users can't read other users' `/proc/<pid>/fd` entries — those sockets surface as rows with `pid == 0`, empty `process_name`, and `partial: true`. For the full view, run the daemon as root (or grant `CAP_SYS_PTRACE` + `CAP_NET_ADMIN` via `setcap`).
- **macOS**: `libproc` calls for other-user pids fail silently; same `partial: true` fallback. For the full view, run the daemon with elevated privileges or with full-disk-access granted to the terminal.

Per the project's "root everywhere" user profile this is the expected deployment posture; non-root sessions still produce a useful port list (your own listeners + `partial: true` rows for everyone else's), just with the partial cue for things you can't attribute. Opt-in integration tests run with `TEPEGOZ_PROBE_TEST=1` should therefore be on hosts where you've prepared the access profile the real daemon will have.

### Processes scope rows show first-sample `cpu_percent: None` (em-dash)
Not a bug. The processes probe (Phase 4 Slice 4b) computes CPU% as a delta between consecutive `sysinfo` refreshes; the first `ProcessList` event after `Subscribe(Processes)` has no prior delta to compute against, so every row carries `cpu_percent: None` and the TUI renders it as an em-dash. Subsequent events (2 s after, 4 s after, ...) carry `Some(x)`. If CPU% stays `None` beyond the first refresh, check the daemon's `forward_processes` task logs — a probe-task panic resets the probe to a fresh state, and the next event again emits `None`.

### Processes scope shows empty / abbreviated cmdlines for other users' processes
On macOS, `sysinfo`'s `Process::cmd()` sometimes returns a truncated argv (just the binary name, not the full argument list) for processes not owned by the calling user. This is a `libproc` limitation — elevating the daemon (or granting full-disk-access to the terminal) typically fills the rest. The `partial: true` flag is set when `command` ends up empty; a truncated-but-non-empty command does NOT set `partial`, on the theory that "something is better than nothing" for the display.

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
