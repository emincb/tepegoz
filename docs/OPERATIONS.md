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

## Slice 4d manual demo prep (Phase 4)

Closes Phase 4. Same shape as C3: 8 scenarios, pass/fail matrix, all rows are the gate (rows 7 and 8 are the 4c-deliverable eyeball confirmations that tests can't fully exercise).

### Prep

```sh
# Terminal 0 — build + run daemon
cargo build
./target/debug/tepegoz daemon

# Terminal 1 — standing Docker victim (gives scenario 4 a real :13000 → container row)
docker run -d --name tepegoz-slice-4d-victim -p 13000:80 alpine sleep 600

# Terminal 2 — TUI
./target/debug/tepegoz tui
```

Expected initial state on TUI launch: the Ports tile (bottom-middle, no longer a placeholder) shows `connecting…` briefly, then a populated table within ~2 s. `Ctrl-b j` → `Ctrl-b l` focuses the Ports tile; the border brightens to cyan.

### Demo sequence

**Step 1 — Ports populates within 2 s; first-render CPU% is em-dash.**

Fresh `tepegoz tui`. The Ports tile transitions from `connecting…` to a table of listening ports within ~2 s. Press `p` to toggle to Processes view. First render: every row's CPU% column shows `—` (em-dash), NOT `0.0`. Sanity: toggle back with `p` to Ports view.

→ Pass: table populates ≤ 2 s; em-dash visible on first render; toggling doesn't reset either view's selection.

**Step 2 — Filter narrows / commits / clears in both views.**

On Ports view with focus: press `/`, type `post` (or any substring matching at least one listener's process_name / local_port / container_id). Filter bar shows `filter: post_` with yellow caret; table narrows to matching rows. Press Enter: caret disappears, filter stays applied ("N/M port(s) · ... · filter: post"). Press `/`, then Esc: filter clears, table returns to full list.

Repeat on Processes view (`p` toggles there). Filter matches on `command` or `pid`.

→ Pass: both views narrow/commit/clear independently; each keeps its own filter state across toggle.

**Step 3 — `p` toggles views; title swaps; help-bar hint matches active view.**

On Ports view: border title reads `ports`; help-bar footer reads `[j/k] nav · [/] filter · [p] Processes`. Press `p`: title swaps to `processes`; help-bar reads `[j/k] nav · [/] filter · [p] Ports`. Press `p` again: back to Ports.

→ Pass: title, help-bar hint, and active view all swap together; no flicker, no stale title.

**Step 4 — Docker-bound port shows container column.**

The standing `tepegoz-slice-4d-victim` container publishes port 13000 → container's :80. Focus Ports tile; filter `13000` (or scroll to find it). The row for `tcp 0.0.0.0:13000` (or equivalent) has a non-empty CONTAINER column (first 12 hex chars of the alpine container's id).

→ Pass: the :13000 row carries a container column entry (short id). No other loopback-only listener shows a container entry.

**Step 5 — Kill the owning process externally; rows disappear within ~3 s.**

Note a non-system process's pid from the Ports or Processes view (e.g. a browser helper, your editor). From a third terminal: `kill <pid>`. Within ~3 s, the Ports row AND the Processes row for that pid disappear. Selection re-anchors to a neighboring row (doesn't stick on the deleted entity).

Safer alternative: `python3 -c "import socket,time; s=socket.socket(); s.bind(('127.0.0.1',24001)); s.listen(); time.sleep(300)" &` (records the PID in `$!`). Then `kill $!` and watch 127.0.0.1:24001 disappear from Ports + python3 disappear from Processes.

→ Pass: both rows disappear within ~3 s; selection doesn't crash the TUI; the tile re-renders cleanly.

**Step 6 — Kill Docker externally; Ports tile keeps working, container column empties.**

From another terminal: Docker Desktop → Quit, or `colima stop`, or `sudo systemctl stop docker`. Watch the Ports tile for the next ~4 s: it does NOT flash unavailable / crash / stall. The CONTAINER column for previously-correlated rows (like the :13000 row) goes empty. The Docker tile (separate scope) transitions to Unavailable — expected, not a Ports concern. Restart Docker: the Ports tile's container column refills within one refresh cycle.

→ Pass: Ports keeps serving live data through a Docker outage; correlation gracefully degrades to empty; recovers on Docker restart.

**Step 7 — UDP footer hint visible and unambiguous at 120×40.**

On the Ports view status bar (bottom of the status row): the text `UDP coming v1.1` appears at the end, not truncated by tile width. At the standing demo terminal size (at least 120×40), the full phrase is legible.

→ Pass: `UDP coming v1.1` renders in full; user understands UDP is deferred, not broken.

**Step 8 — Second-sample CPU% transitions em-dash → number.**

On Processes view, first render has every row at `—`. Wait at least 2 s (ideally 4 s). At least one row (typically `tepegoz`, your shell, or a visible-activity process like `Electron Helper`) transitions from `—` to a real number (e.g. `0.2`, `12.5`). The em-dash disappears for measured processes.

→ Pass: at least one row's CPU% is a non-`—` number after the second refresh; em-dash is transient, not permanent.

### Pass/fail matrix

| # | Scenario | Pass |
|---|---|---|
| 1 | Ports populates ≤ 2 s; first-render Processes CPU% = em-dash | ☐ |
| 2 | Filter narrows/commits/clears in both views independently | ☐ |
| 3 | `p` toggles views; title swaps; help-bar hint matches active view | ☐ |
| 4 | `:13000` row shows a CONTAINER column entry (short docker id) | ☐ |
| 5 | Killing a process externally removes it from both Ports + Processes ≤ 3 s without crashing | ☐ |
| 6 | Ports keeps working through a Docker outage; CONTAINER column empties, recovers on restart | ☐ |
| 7 | `UDP coming v1.1` footer hint visible and un-truncated at 120×40 | ☐ |
| 8 | Second-sample CPU% transitions from `—` to a number for at least one row | ☐ |

**All 8 scenarios are the gate.** Rows 7 and 8 pin 4c-deliverable behavior that the integration tests don't fully exercise. If any fail, record the gotcha in `docs/ISSUES.md` as a Phase-4 polish item — fix before the Phase 4 close commit flips row 4 to ✅.

### Tear down

```sh
docker rm -f tepegoz-slice-4d-victim 2>/dev/null
pkill -f "python3 -c import socket" 2>/dev/null
pkill -f "tepegoz daemon" 2>/dev/null
```

## SSH Fleet discovery (Phase 5 Slice 5b)

Tepegöz resolves the SSH Fleet host list from three sources, in strict
precedence (**first non-empty source wins — no merging**):

1. **Tepegöz `config.toml`** — the `[ssh.hosts]` table at:
   - Linux: `$XDG_CONFIG_HOME/tepegoz/config.toml` (falls back to
     `~/.config/tepegoz/config.toml`)
   - macOS: `~/Library/Application Support/tepegoz/config.toml`

   Example:

   ```toml
   [[ssh.hosts]]
   alias = "prod-api"
   hostname = "10.0.0.5"
   user = "deploy"
   port = 22
   identity_file = "~/.ssh/id_ed25519_prod"   # ~ expansion supported
   autoconnect = true                          # daemon dials on startup

   [[ssh.hosts]]
   alias = "bench-01"
   hostname = "10.0.2.5"
   user = "bench"
   # autoconnect omitted → defaults to false (lazy-connect —
   # waits for `Ctrl-b r` on the focused Fleet row, or
   # `tepegoz connect bench-01` from the CLI in Slice 5d).
   ```

   **`autoconnect`** is a per-host policy flag, not a wire field.
   Clients render Fleet connection states (`●` / `◐` / `○` / `⚠`),
   not per-host policy — the daemon consults `autoconnect` only
   during supervisor spawn. ssh_config- and env-sourced hosts are
   always lazy-connect (ssh_config has no autoconnect concept).

2. **`TEPEGOZ_SSH_HOSTS` env** — comma-separated list of aliases.
   Aliases are looked up in `~/.ssh/config`, so this is the right knob
   when you want tepegöz to show a subset of your ssh_config hosts
   without editing config files. Single-user machines won't typically
   need this; CI / scripted contexts do.

   ```sh
   TEPEGOZ_SSH_HOSTS=staging,dev-eu tepegoz daemon
   ```

3. **`~/.ssh/config`** — every concrete (non-wildcard) `Host` entry
   becomes a Fleet row. `User`, `Hostname`, `Port`, `IdentityFile`, and
   `ProxyJump` are resolved via `russh-config` per ssh_config(5) merge
   rules + percent-token expansion.

**First non-empty wins, no merging.** If your `config.toml` has any
`[[ssh.hosts]]` entries, `~/.ssh/config` is **not consulted** — this
avoids surprise-merging behavior where adding one override silently
changes the rest of the list. The Fleet tile's footer renders the
resolved source when it's an override (tepegoz config.toml / env),
hidden when the source is the user's ssh_config.

### Env overrides for config + data dirs (Phase 5 Slice 5c-i)

Two env vars short-circuit the `dirs` crate's platform lookup:

- `TEPEGOZ_CONFIG_DIR=<dir>` — makes the config file `<dir>/config.toml`.
- `TEPEGOZ_DATA_DIR=<dir>` — makes the SSH known_hosts file
  `<dir>/known_hosts` (and, later, the state DB + recordings under
  Phase 8).

Primary use is **portable integration tests on macOS**: `dirs::config_dir()`
ignores `XDG_CONFIG_HOME` on macOS (returns
`~/Library/Application Support` unconditionally), so a test that needs
to land a tepegoz `config.toml` without mutating the user's real
directory relies on `TEPEGOZ_CONFIG_DIR` pointing at a tempdir.
Secondary use is headless containers (no standard home layout).

Setting either env var is the user's choice — production installs
shouldn't need them.

### Diagnostic: `tepegoz doctor --ssh-hosts`

Dumps the resolved host list + source label:

```sh
$ tepegoz doctor --ssh-hosts
source: ssh_config (/home/alice/.ssh/config)
hosts (3):
  staging  alice@staging.internal:2222
    IdentityFile: /home/alice/.ssh/id_ed25519_staging
  dev-eu  alice@dev-eu.eu.dev:22
  bench-01  bench@10.0.2.5:22
    ProxyJump: bastion (not supported in v1 — Slice 5c surfaces this)
```

Use it when `tepegoz connect <alias>` can't find a host, or to verify
that an override layer is active.

### Phase 5 limitations (documented, not bugs)

- **`Include` directives in `~/.ssh/config` are not followed.** Hosts
  defined only in an Include'd file are invisible to tepegoz. Workaround:
  flatten your ssh_config, or list the hosts explicitly in
  `tepegoz/config.toml`. Pinned by
  `tepegoz-ssh::config::tests::include_directive_is_not_followed_phase_5_limitation`
  — if that test starts failing, russh-config grew Include support and
  this limitation is gone.
- **`ProxyJump` hosts are captured but not dialed.** Slice 5c surfaces
  a clear `SshError::ConnectFailed { reason: "host requires ProxyJump
  which is not supported in Phase 5 (v1.1)" }` so the user sees why,
  rather than an opaque network timeout.
- **SSH certificate identities in `$SSH_AUTH_SOCK` are skipped** with
  an explicit "N certificate identity(ies) skipped (not supported in
  Phase 5)" entry in the `SshError::AuthFailed.reason` chain, rather
  than a silent skip.
- **Remote pty session persistence across SSH disconnects lands in
  Phase 6.** In Phase 5 a dropped SSH connection kills the remote pane
  (Q3 proposal accepted limit; the agent-backed remote pty in Phase 6
  replaces this transparently without any wire-protocol change).
- **Between 5b merge and 5c merge**, every host in the Fleet tile
  renders as `○ Disconnected` — the connection supervisor that drives
  real transitions ships in 5c. Same degrade-gracefully shape as Phase
  3's `DockerUnavailable`.

### Host-key TOFU

On first connect to a given `(hostname, port)`, tepegöz auto-accepts
the server's host key and persists it to:

- Linux: `$XDG_DATA_HOME/tepegoz/known_hosts`
- macOS: `~/Library/Application Support/tepegoz/known_hosts`

**Tepegöz never touches `~/.ssh/known_hosts`** — our SSH layer is
additive to your OpenSSH state, not destructive to it. File format is
OpenSSH-compatible so you can inspect or hand-edit with standard
tools; file mode is `0600` on Unix.

On key mismatch (presented key differs from stored), tepegöz rejects
the connection with a structured error pointing at the stored record's
file + line. Recovery:

```sh
$ tepegoz doctor --ssh-forget staging
removed 1 entry(ies) for staging.internal:22 from /Users/alice/Library/Application Support/tepegoz/known_hosts — next connection to 'staging' will re-TOFU the new key
```

Only exact single-host entries (tepegoz's own writes) are removed.
Multi-host comma-separated patterns and hashed `|1|…` entries are
treated as user-owned and preserved — if you maintain those by hand,
tepegöz won't surgery them. The file mode stays `0600` across the
rewrite.

**Intentionally a two-step recovery** (`--ssh-forget` then reconnect)
so a legitimate key rotation gets verified before the new key is
trusted.

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
