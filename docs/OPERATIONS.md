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
# detach with Ctrl-b d
# reattach with another `./target/debug/tepegoz tui` — scrollback replays
```

The TUI keyboard surface (Slice 6.0 + 6c-iii) is six bindings plus mouse:
- `Tab` / `Shift-Tab` — cycle tile focus
- arrow keys / `j` / `k` — navigate rows inside the focused scope
- `Enter` — primary action on the selected row (Fleet → open remote pane)
- `Esc` — cancel / back (clears filter, exits logs view, dismisses help + host picker)
- `Ctrl-b d` — detach
- `Ctrl-b t` — open the host picker on target-capable tiles (Docker in 6c-iii;
  Ports + Processes in 6d). Use arrows/`j`/`k` + Enter to commit a retarget,
  or Esc to cancel. Greyed-out rows are hosts that aren't currently reachable —
  selectable but not committable.
- `Ctrl-b ?` — toggle the in-TUI help overlay (authoritative reference)
- mouse — click to focus tile + select row; click the Docker tile title bar to
  open the host picker; click a tab to switch panes; click the `[×]` affordance
  next to the tab strip to close the active pane; double-click a Fleet row to
  open a remote pane

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

The TUI god view is the part where eyeball-confirmation has historically diverged from test-passes (Phase 2 immediate-detach was exactly this). C1.5c acceptance is a manual demo against a standing fixture container. The gating checks per CTO direction: god-view layout renders on first launch with no config; focus navigation (`Tab` / `Shift-Tab` documented, `Ctrl-b h/j/k/l` + arrows as undocumented aliases per Slice 6.0) feels natural; vim in the pty tile renders correctly and survives focus movement; Docker tile populates within ~2 s; placeholder tiles are clearly labeled and non-interactive; detach/reattach preserves state; engine-unavailable-mid-session recovers cleanly.

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
Tab                   # PTY → Docker (border becomes bright on Docker)
Tab                   # Docker → Ports
Tab                   # Ports → Fleet
Tab                   # Fleet → ClaudeCode (placeholder)
Tab                   # ClaudeCode → PTY (wraps)
Shift-Tab             # reverse cycle

# Mouse equivalent — click any tile to focus it; hover reveals a
# cyan-dimmed border on the tile under the pointer.

# Undocumented aliases for muscle memory:
Ctrl-b j              # PTY → Docker (directional; same spatial behavior as before)
Ctrl-b l              # Docker → Ports (horizontal neighbor)

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

Tab                   # focus Docker; vim stays on-screen in the pty tile
Shift-Tab             # focus PTY again (or Tab through the remaining tiles)
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
Tab                   # focus Docker tile (from PTY)
j, k, or ↑/↓          # move selection (▶ marker tracks)
g / G                 # jump to top / bottom
/ tepegoz             # open filter input, type "tepegoz"; list narrows
<Enter>               # commit filter (bar stays; caret disappears)
<Esc>                 # clear filter entirely

# → expect: while Docker is focused, plain j/k/g/G act on the list (not
#   focus). Tab / Shift-Tab (or undocumented Ctrl-b h/j/k/l) continue
#   to move focus between tiles. The PTY tile keeps rendering in the
#   background.
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

## Slice 5e manual demo prep (Phase 5 close)

Closes Phase 5. Same shape as 4d: 8 scenarios, pass/fail matrix, all rows are the gate. Scenarios 1-3 cover the 5d-ii pane-stack + tab strip + CLI; 4 documents the session-local stack policy; 5-8 cover the 5a-5c-i SSH lifecycle surface (drop / reconnect / auth fail / TOFU mismatch).

The demo needs a real SSH server you can reach. `cargo xtask demo-phase-5 up` provisions everything: a `linuxserver/openssh-server` Docker fixture, a throwaway ed25519 keypair, a tepegoz `config.toml` pointing `staging` at the fixture, and a daemon bound to isolated config/data dirs — no interaction with your real `~/.ssh/known_hosts` or `~/.config/tepegoz`. Scope gate is one command up, one command down; the old multi-terminal shell script is gone (landed with the `cargo xtask demo-phase-5` commit as part of Slice 5e's "standing rule: manual demos ship with a one-command runner").

### Prep

```sh
# Terminal 0 — bring the fixture up. Stays blocking on Ctrl-C.
cargo xtask demo-phase-5 up
```

Expected output (paths will vary by platform):

```
sshd container: tepegoz-demo-phase-5-sshd on 127.0.0.1:<random-port>
tepegoz config: /tmp/tepegoz-demo-phase-5/tepegoz-config/config.toml
daemon socket:  /tmp/tepegoz-<uid>/daemon.sock
demo root:      /tmp/tepegoz-demo-phase-5

Ready. Run 'tepegoz tui' in a new terminal.
```

All subsequent commands run from a **different terminal** — Terminal 0 stays blocked on Ctrl-C for the duration of the demo. The xtask prints the `demo root` path; scenarios 7-8 below reference files under that path. On macOS, `demo root` typically resolves to `/var/folders/…/T/tepegoz-demo-phase-5/`; on Linux, usually `/tmp/tepegoz-demo-phase-5/`. Export it once for convenience:

```sh
# Terminal 1 — copy the demo-root path the xtask printed.
export DEMO_ROOT=<demo root from xtask output>
```

Sanity-check the host list (optional):

```sh
TEPEGOZ_CONFIG_DIR="$DEMO_ROOT/tepegoz-config" \
TEPEGOZ_DATA_DIR="$DEMO_ROOT/tepegoz-data" \
  ./target/debug/tepegoz doctor --ssh-hosts
# Expect: source: tepegoz config (...) / hosts (1): staging tepegoz@127.0.0.1:<port>
```

### Demo sequence

**Step 1 — `tepegoz connect staging` opens a remote pane from the CLI.**

```sh
./target/debug/tepegoz connect staging
```

The TUI launches with the god view; the PTY tile's tab strip shows a single `[1 ssh:staging*]` entry. The remote shell prompt (linuxserver image's default `tepegoz@<container-id>$`) is responsive — type `uname -a` + Enter, see `Linux <hostname> ... GNU/Linux`. The Fleet tile's `staging` row glyph shifts from `○` (Disconnected) → `◐` (Connecting) → `●` (Connected) within the connect window. Press `Ctrl-b d` to detach: TUI exits cleanly, terminal returns to your outer shell prompt, the daemon + the remote pane stay alive.

→ Pass: `connect staging` opens, runs a real remote command, detaches with `Ctrl-b d`. Tab strip shows exactly one `ssh:staging` entry (no local root pane).

**Step 2 — `tepegoz tui` + plain `Enter` on Fleet opens a second remote pane.**

```sh
./target/debug/tepegoz tui
```

The TUI launches with a single local pane: tab strip shows `[1 zsh*]` (or `bash`/`fish` per `$SHELL`). Press `Tab` three times to cycle through to the Fleet tile (PTY → Docker → Ports → Fleet). The border highlights bright cyan when focused. The `staging` row is highlighted via `▶` selection marker. Press `Enter`: an Info toast `opening ssh:staging…` flashes; within 1-3 s the tab strip updates to `[1 zsh] [2 ssh:staging*] [×]`, focus jumps back to the PTY tile, and the remote shell prompt appears. Double-clicking the row works identically.

→ Pass: tab strip shows both entries with `*` on the new remote pane + the `[×]` close affordance at the tail; focus returned to PTY automatically; remote shell responds to `uname -a`.

**Step 3 — clicking a tab switches panes without losing scrollback; `[×]` closes the active pane.**

In the local pane (tab 1), run `seq 1 30` so there's distinguishable scrollback. Click the `[2 ssh:staging]` tab: active marker shifts, the remote pane's last screen reappears (NOT a refresh of the local one). Click `[1 zsh]`: back to the local pane with `seq 1 30` output still visible. Click the `[×]` affordance: the currently-active pane closes (the other tab becomes active; if the stack would empty, a fresh local root spawns). Note: `Ctrl-b &` still closes as an undocumented keyboard alias.

→ Pass: switching tabs preserves each pane's vt100 screen content; `[×]` closes the active tab and promotes the sibling.

**Step 4 — `Ctrl-b d` + reattach: pane stack is session-local, expect a single pane on reattach (NOT a regression).**

Re-open a second remote pane if Step 3 left you with one tab. From the two-pane state, press `Ctrl-b d`. TUI exits; outer shell returns. Re-launch:

```sh
./target/debug/tepegoz tui
```

The tab strip now shows ONE entry — whichever pane the daemon's `ListPanes` returns first. This is the session-local stack policy (`docs/ISSUES.md#pane-stack-is-session-local`): the TUI doesn't persist tab order across detach. Both daemon-side panes are still alive (one of them is what you're attached to); to verify, click `[×]` to close the visible pane — the other one becomes visible in slot 1 (or, if both are gone, a fresh local root spawns).

→ Pass: reattach shows one pane; closing it surfaces the other (proves both daemon-side panes survived the detach); user understands the session-local policy.

Detach (`Ctrl-b d`) and clean up the daemon-side panes by reconnecting + `[×]` clicks until you're back to a single local pane (or kill the daemon and restart it for Step 5).

**Step 5 — SSH server drop mid-session: remote pane terminates cleanly with a toast.**

Re-launch the TUI, then `Tab` three times to focus the Fleet tile (or `Ctrl-b j → l → l` via the undocumented aliases), select the `staging` row, and press plain `Enter` to open the remote pane. Confirm the prompt is responsive. From a third terminal:

```sh
docker stop tepegoz-demo-phase-5-sshd
```

Within seconds, the active remote pane gets an Info toast like `pane ssh:staging exited (code <code>)`, the tab strip drops the entry (or auto-reopens a local root if it was the only pane), and focus returns to whichever pane is now active. The Fleet row glyph shifts to `○` (Disconnected) within ~30 s as the supervisor's heartbeat times out. Restart the container so subsequent steps can use it: `docker start tepegoz-demo-phase-5-sshd`.

→ Pass: SSH drop surfaces as a per-pane Info toast (NOT a TUI crash); tab strip updates; Fleet row glyph turns gray within the heartbeat window; restart restores the host to discoverable state.

**Step 6 — `r` on Fleet row dispatches a reconnect with an Info "dispatched" toast.**

With the container restarted (per Step 5 cleanup), focus the Fleet tile; the `staging` row should be at `○` (Disconnected) initially, transitioning to `◐` → `●` if the supervisor is auto-reconnecting. To force the demo: Tab to the Fleet tile, select the `staging` row, press `r`. An Info toast `reconnect staging — dispatched` appears; the row glyph transitions through `◐` to `●` within a few seconds.

→ Pass: `r` produces the Info "dispatched" toast (NOT a state-change toast; that comes seconds later via `HostStateChanged`); the row glyph reaches `●` within the connect window.

**Step 7 — Auth failure on first connect: `⚠` row glyph + red toast with verbatim russh reason.**

Detach (`Ctrl-b d`). Generate a second keypair the sshd doesn't know about, swap it into config, re-launch:

```sh
ssh-keygen -t ed25519 -N "" -f "$DEMO_ROOT/wrong_key" -q -C "tepegoz-5e-wrong"
sed -i.bak "s|identity_file = .*|identity_file = \"$DEMO_ROOT/wrong_key\"|" \
  "$DEMO_ROOT/tepegoz-config/config.toml"
./target/debug/tepegoz tui
```

Focus Fleet, press `r` on `staging` to force a connect attempt. The row glyph transitions to `⚠` (red); a red toast appears with the russh failure attempts list (something like `staging: auth failed — publickey: Permission denied`). Recovery:

```sh
sed -i.bak "s|identity_file = .*|identity_file = \"$DEMO_ROOT/id_ed25519\"|" \
  "$DEMO_ROOT/tepegoz-config/config.toml"
```

Press `r` again on the row — should succeed this time (or the next supervisor retry will succeed within the backoff window).

→ Pass: red `⚠` glyph + verbatim russh-reasoned red toast; restoring the correct key + retrying recovers cleanly.

**Step 8 — Host-key mismatch after TOFU: red toast with path:line + `tepegoz doctor --ssh-forget` recovery + clean re-TOFU.**

After Step 7's recovery, your tepegoz known_hosts file has a TOFU'd entry for `127.0.0.1:<port>`. To simulate a host-key change, regenerate the sshd container's keys:

```sh
docker exec tepegoz-demo-phase-5-sshd rm -f /config/ssh_host_keys/*
docker restart tepegoz-demo-phase-5-sshd
```

Re-attach (or press `r` on the Fleet row if still attached): the row glyph transitions to `⚠` (red); a red toast surfaces `staging: host key rejected — <reason from russh, includes path:line of the stored entry>` (the entry lives in `$DEMO_ROOT/tepegoz-data/known_hosts`). Recover via:

```sh
TEPEGOZ_CONFIG_DIR="$DEMO_ROOT/tepegoz-config" \
TEPEGOZ_DATA_DIR="$DEMO_ROOT/tepegoz-data" \
  ./target/debug/tepegoz doctor --ssh-forget staging
# Expect: removed N entry(ies) for 127.0.0.1:<port> ... — next connection ... will re-TOFU
```

Re-attach the TUI + press `r` on the `staging` row in the Fleet tile. The supervisor re-TOFUs the new host key and connects cleanly: glyph back to `●`.

→ Pass: mismatch surfaces with file:line in the toast; `--ssh-forget` removes the stale entry; subsequent connect succeeds via re-TOFU.

### Pass/fail matrix

| # | Scenario | Pass |
|---|---|---|
| 1 | `tepegoz connect staging` opens, runs a remote command, `Ctrl-b d` detaches cleanly | ☐ |
| 2 | Plain `Enter` on the selected Fleet row from `tepegoz tui` opens a 2nd pane; tab strip shows both with `*` on new + `[×]` close affordance at tail | ☐ |
| 3 | Clicking a tab swaps active pane; each pane preserves its own scrollback across switches; clicking `[×]` closes the active pane | ☐ |
| 4 | `Ctrl-b d` + reattach → 1 pane (session-local stack); closing it surfaces the other surviving pane | ☐ |
| 5 | `docker stop` mid-session → per-pane Info toast; tab strip drops entry; Fleet row → `○` within heartbeat window | ☐ |
| 6 | `r` on Fleet row → Info "dispatched" toast; row glyph reaches `●` within connect window | ☐ |
| 7 | Wrong IdentityFile → `⚠` red glyph + red toast with russh attempts list; restoring correct key + retry recovers | ☐ |
| 8 | Container key regen → red `⚠` glyph + red toast `host key rejected — <reason>`; `doctor --ssh-forget` removes; re-TOFU connects cleanly | ☐ |

**All 8 scenarios are the gate.** Scenarios 4-5-7-8 in particular pin behaviors the integration tests can't fully exercise (session-local reattach, mid-session SSH drop UX, TOFU recovery loop). If any fail, file a 5e polish item in `docs/ISSUES.md` — fix before the Phase 5 close commit flips row 5 to ✅.

### Tear down

In Terminal 0, press `Ctrl-C` — the xtask kills the daemon, removes the sshd container, and deletes the demo root directory. Prints `Torn down.` when complete.

If Terminal 0's xtask is already gone (crashed, closed terminal, lost ssh connection), run the idempotent tear-down from any shell:

```sh
cargo xtask demo-phase-5 down
```

`down` is safe to run multiple times; when there's nothing to clean up it prints `Torn down.` and exits 0.

## Building agents + Phase 6 Slice 6a demo

**Cross-compile all four agent targets.** Requires `zig` + `cargo-zigbuild` on PATH (plain cargo can't cross-link a Darwin SDK from a Linux host or vice versa):

```sh
# One-time setup
brew install zig                          # or https://ziglang.org/download/
cargo install cargo-zigbuild

# Build agents for all four target triples
cargo xtask build-agents
```

Output layout:

```text
target/agents/x86_64-unknown-linux-musl/{tepegoz-agent, manifest.json}
target/agents/aarch64-unknown-linux-musl/{tepegoz-agent, manifest.json}
target/agents/x86_64-apple-darwin/{tepegoz-agent, manifest.json}
target/agents/aarch64-apple-darwin/{tepegoz-agent, manifest.json}
```

Each `manifest.json` carries the compiled-in `protocol_version`, `target_triple`, and `built_at_unix_secs`. The controller's `build.rs` (`crates/tepegoz/build.rs`) picks these up on the next `cargo build` and embeds each binary via `include_bytes!`, asserting the manifest `protocol_version` matches the proto text file at `crates/tepegoz-proto/PROTOCOL_VERSION`. Mismatch is a hard compile failure; the diagnostic names the offending triple and suggests `cargo xtask build-agents` as the fix.

Plain `cargo build` (no `build-agents` invocation) still succeeds: the `build.rs` emits one `cargo:warning` about the missing tree and populates every `agents::embedded_agents::<ARCH>` slot with `None`. Remote deploy (Phase 6 Slice 6b onward) will surface a runtime error in that state; until 6b lands the fallback path is a harmless no-op.

**Phase 6 Slice 6a local handshake demo.** No SSH yet — this builds `tepegoz-agent` for the host target only, spawns it as a subprocess with piped stdio, drives a single `AgentHandshake` envelope, prints the pretty-formatted response. Proves the agent + wire + codec scaffolding work end-to-end:

```sh
cargo xtask demo-phase-6 up
# Expect output like:
#   agent handshake ✓
#     request_id:   1
#     version:      10
#     os:           macos
#     arch:         aarch64
#     capabilities: (none — 6a ships an empty list; 6c/d populate)

cargo xtask demo-phase-6 down
# Idempotent tempdir cleanup; nothing persistent to remove in 6a.
```

## Remote agent deploy + handshake (Phase 6 Slice 6b)

Slice 6b turns the 6a-embedded agent binaries into something you can actually run on a remote host. Three moving parts:

1. **`cargo xtask demo-phase-6 up --remote`** — spawns a throwaway sshd container (`tepegoz-demo-phase-6-sshd`, separate from demo-phase-5's), cross-builds `tepegoz-agent` for `x86_64-unknown-linux-musl`, connects via tepegoz-ssh, deploys, and drives one handshake round-trip over the exec channel. Validates the full Slice 6b pipeline end-to-end against a known fixture.

   ```sh
   # Requires: docker, ssh-keygen, cargo-zigbuild (cross-platform) OR
   # a Linux host with the musl target + linker installed.
   cargo xtask demo-phase-6 up --remote
   # Expect:
   #   [demo-phase-6] sshd listening at 127.0.0.1:<port>
   #   [demo-phase-6] cross-building tepegoz-agent for x86_64-unknown-linux-musl…
   #   [demo-phase-6] connecting via SSH (TOFU → isolated known_hosts)…
   #   [demo-phase-6] deploying agent (idempotent — cache hit if already matching)…
   #   [demo-phase-6]   path: /config/.cache/tepegoz/agent-v10 (uploaded now)
   #   remote agent handshake ✓
   #     version:      10
   #     os:           linux
   #     arch:         x86_64
   #     capabilities: (none — 6a/6b ship empty; 6c/d populate)
   cargo xtask demo-phase-6 down --remote
   # → docker rm -f + tempdir cleanup (idempotent)
   ```

2. **`tepegoz doctor --agents`** — observation-only report of the remote-agent deploy state across every Fleet host. For each host: connects via SSH → `uname -sm` → looks up the embedded blob for that target → inspects `$HOME/.cache/tepegoz/agent-v<N>` on the remote → reports presence/absence + SHA256 match. Non-fatal on per-host errors (unreachable hosts, etc. — logged inline + iteration continues).

   ```sh
   tepegoz doctor --agents
   # Example output:
   # source: ssh_config (/Users/emin/.ssh/config)
   # agents (3 host(s)):
   #   staging              x86_64-unknown-linux-musl       ✓ matches embedded
   #     /home/ubuntu/.cache/tepegoz/agent-v10 (214008 bytes, mtime 1744000000)
   #     remote   sha256: 3a7b5f2e8c1d4f9b
   #   dev                  aarch64-apple-darwin            ✗ absent — would deploy on next connect
   #   old-box              x86_64-unknown-linux-musl       ⚠ drift — redeploy needed
   #     /home/ubuntu/.cache/tepegoz/agent-v10 (203456 bytes, mtime 1743800000)
   #     remote   sha256: 9f8e7d6c5b4a3928
   #     embedded sha256: 3a7b5f2e8c1d4f9b
   ```

   `doctor --agents` does NOT deploy — it reports what a fresh `tepegoz connect <alias>` (or a Slice 6c+ remote subscription) would do. Use it before deploys / after version bumps / when a remote flow fails unexpectedly.

3. **Agent TOFU model**. Slice 6b does NOT maintain a first-seen stored-hash database for agent binaries (unlike SSH host keys). The embedded binary is the source of truth — every deploy verifies by content-hash against `sha2::Sha256::digest(&embedded_bytes)`. If the remote's file matches, no upload happens (cache-hit branch). If it diverges, we upload + verify + one-retry on mismatch + terminal error on second mismatch. Rationale: the agent's identity is its binary hash + `PROTOCOL_VERSION`, both controller-owned at build time — SSH-style "first-seen-wins" semantics don't add safety here, just complexity.

**Known limitations** (`docs/ISSUES.md`):
- Protocol version bumps leave orphaned `agent-v<N-1>` binaries on remote hosts (~1 MB each). Not a correctness issue; Phase 6 close or v1.1 will add a cleanup helper.
- Universal macOS `lipo` deferred to the Phase 10 release pipeline (requires Xcode tooling not guaranteed on dev/CI boxes). Controllers embed two separate darwin binaries for now.

## Remote Docker scope (Phase 6 Slice 6c)

Slice 6c delivers the first user-visible remote scope: the TUI's Docker tile can route its container list + logs + stats + actions through any Fleet host's deployed agent instead of the daemon's local docker engine.

**Retarget flow** (keyboard):

1. Focus the Docker tile (`Tab` or click).
2. `Ctrl-b t` — opens the centered host picker modal.
3. Arrows / `j` / `k` — navigate rows. Local is always first; Fleet hosts follow in discovery order.
4. `Enter` — commit. Greyed-out rows (host not in `Connected` state) are no-ops; the modal stays open so you can pick a reachable host or reach remediation via the Fleet tile + `Ctrl-b r`.
5. `Esc` — dismiss without changing the target.

**Retarget flow** (mouse): click anywhere on the Docker tile's title bar to open the picker. Any click outside the modal dismisses it; commit is keyboard-driven (`Enter`).

**Tile title suffix**: the Docker tile title now reads `docker · local` or `docker · <alias>`, making the current target visible at a glance. Logs view titles read `docker · logs · <container> · <target>`.

**Error surfaces**: a remote target that's unreachable at subscribe time (agent not deployed, agent lacks docker capability, writer channel dropped) surfaces as `Event::DockerUnavailable { reason }` — the same shape the TUI already shows when the local docker engine is down. The reason string identifies the alias + diagnosis. No separate UX path for "remote failed" vs "local failed".

```sh
# Demo: deploy an agent + subscribe Docker remotely end-to-end
cargo xtask demo-phase-6 up --remote
# Expect (after handshake):
#   [demo-phase-6] subscribed Docker on the agent; awaiting first event…
#
#     remote Docker subscribe ✓ (DockerUnavailable path)
#       event:       DockerUnavailable
#       reason:      docker engine unavailable: …
#       note:        bind-mount /var/run/docker.sock into the sshd
#                    container (and add tepegoz to the docker group)
#                    for a ContainerList.
cargo xtask demo-phase-6 down --remote
```

**Out of scope for 6c**: host aggregation ("show all hosts' Docker at once" — v1.1 polish). Retarget for the Ports + Processes tiles (6d — same `Ctrl-b t` + modal, reused as-is with a different capability string).

## Remote Ports + Processes scopes (Phase 6 Slice 6d-ii)

Slice 6d-ii extends the 6c-iii retarget UX to the Ports + Processes tiles. Same `Ctrl-b t` + click-on-title-bar gestures, same picker modal, same `(no docker)` / `(no ports)` / `(no processes)` greying — the picker is tile-agnostic with a per-invocation `required_capability` string.

**Title-bar suffix** on the Ports tile reads `ports · <target>` or `processes · <target>` depending on which view is currently active (toggled with `p`). The two views have **independent targets** — a user can run local Ports + remote Processes simultaneously without one retarget bleeding into the other.

**Ports / Processes capabilities** are always present on supported platforms (Linux + macOS) — the probes are statically linked into the agent. Unlike Docker (which probes `Engine::connect` at handshake time), Ports + Processes don't have an external dependency that can be down. A subscription-time failure surfaces as `PortsUnavailable` / `ProcessesUnavailable` on the wire (e.g. permission-denied on `/proc` reads).

```sh
# Demo: subscribe Docker + Ports + Processes against a remote agent
cargo xtask demo-phase-6 up --remote
# Expect (after handshake):
#
#     remote Docker subscribe ✓ (DockerUnavailable path)
#       reason:      docker engine unavailable: …
#
#     remote Ports subscribe ✓
#       event:       PortList
#       ports:       N
#       source:      …
#
#     remote Processes subscribe ✓
#       event:       ProcessList
#       rows:        N
#       source:      sysinfo
cargo xtask demo-phase-6 down --remote
```

**Out of scope for 6d**: per-host procs aggregation in the Fleet tile (CTO carve-out — v1.1 polish; the user can already see per-host process counts by retargeting the Processes tile to a specific host).

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
