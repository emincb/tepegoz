# Issues

Active bugs and their diagnostic state. Resolved issues archived below with fix commit.

---

## Active

_(none)_

---

## Phase 5 close caveats (manual demo coverage gap)

Phase 5 closed on 2026-04-15 with scenarios 1-4 of the 8-scenario
manual demo walked by the user; scenarios 5-8 were not manually
validated. Each underlying piece of the failure-mode machinery IS
machine-tested, but the end-to-end "real SSH failure → red toast
renders" path is only exercised by opt-in integration tests, never
by a live user eyeball.

**What IS verified at close:**

- Scenarios 1-4 (CLI connect, multi-pane open via Fleet, tab
  switching, detach/reattach): manually walked end-to-end against a
  live `lscr.io/linuxserver/openssh-server` container.
- Scenario 5-6 daemon machinery (heartbeat → Degraded → Disconnected
  → reconnect): `crates/tepegoz-core/tests/fleet_scope.rs::
  fleet_supervisor_connects_autoconnect_host_and_reconnects_after_
  container_kill` (opt-in, ran green at 5c-i).
- Scenario 7-8 pieces: `tepegoz-ssh::known_hosts` tests (mismatch
  returns file:line, `forget` preserves hand-edited entries, 0600
  mode), `tepegoz-ssh::session` auth-chain tests (passphrase-key-no-
  agent surfaces verbatim), `tepegoz-tui::scope::fleet` render tests
  (4 glyphs render correctly per state), TUI toast-gating unit tests
  (transition-into-terminal only).

**What IS NOT verified:**

- End-to-end TUI rendering on a real SSH failure: does the red toast
  text read correctly? does the `⚠` glyph appear with the right
  timing? does `tepegoz doctor --ssh-forget <alias>` followed by
  reconnect actually re-TOFU cleanly?

**If a user reports a bug in this area:**

- Check `${XDG_CACHE_HOME:-~/.cache}/tepegoz/tui.log` for
  `HostStateChanged` trace events around the reported failure.
- Re-run the opt-in integration test with
  `TEPEGOZ_SSH_TEST=1 TEPEGOZ_DOCKER_TEST=1 cargo test -p
  tepegoz-core --test fleet_scope` to confirm the daemon still
  transitions correctly.
- If the daemon transitions correctly but the TUI doesn't render the
  expected toast, the bug is in `app.rs::handle_fleet_event` (toast
  gating) or `scope/fleet.rs` (glyph mapping). Neither path has been
  touched since 5c-ii.

**Why close anyway:** the failure-mode UX is insurance-tier
functionality, not daily-use; the user's explicit priority call is
"move on, fix if reports surface." Slice 6.0 touches the TUI
rendering layer anyway (mouse capture + hover states + help
overlay), so any real regression in this area would likely surface
during 6.0 work.

---

## Known limitations, Phase 6 upgrade path

Documented gaps that are **not bugs** — they're accepted scope
boundaries for Phase 5's russh-direct remote pty design, with a named
mechanism that closes the gap in a later phase without changing the
wire protocol. If one of these starts behaving like a bug (user reports
crash, wrong data, security issue), re-classify and move up to Active.

### Remote pane dies on SSH disconnect (Phase 5 → Phase 6)

Phase 5 Slice 5d-i wires each `OpenPane { target: Remote { alias } }`
to a **fresh SSH session** opened via `tepegoz_ssh::connect_host` with
`channel.request_pty` + `request_shell`. When the TCP connection drops
(network flap, server restart, explicit disconnect), russh's session
task ends; the channel closes; our `RemotePane` driver task emits
`PaneUpdate::Exit`; the pane is dead. Client renders the terminal
pane with its accumulated scrollback intact; user must open a fresh
remote pane to reconnect.

**This is not persistence**. There's no server-side session
continuity; a user running `vim` on a flappy connection loses their
edit state at every flap.

**Phase 6 upgrade path**. Phase 6 deploys a `tepegoz-agent` binary
to the remote host via `scp` + exec over SSH. The agent runs as a
long-lived process that owns the actual local pty on the remote;
SSH carries our wire protocol over stdio to the agent. When SSH
flaps, the agent stays up (local shell process still running); on
reconnect, the daemon reopens the SSH channel to the same agent
pid and re-attaches. Full persistence of the remote shell across
network disruption — same shape as local panes' Phase 2 detach-
and-reattach invariant.

The wire protocol does NOT change between Phase 5 and Phase 6 for
this — `OpenPane { target: Remote { alias } }` continues to mean
"open a remote pty against this host." Only the daemon-side
`RemotePane::open` implementation swaps from "open russh channel
with pty-req" to "forward OpenPane to agent over SSH stdio."

Q3 of the Phase 5 proposal (signed off 2026-04-14) accepted this
staging explicitly: "Phase 5 ships russh-direct; Phase 6 swaps to
agent-backed without changing the wire shape."

### Per-pane SSH connection overhead (Phase 5 → Phase 6)

Each Phase-5-era `RemotePane` opens its own fresh TCP + TLS-equivalent
handshake + auth round. Three panes against `staging` = three
independent SSH connections. The Fleet supervisor's session (managed
by 5c's `host_supervisor`) is **not reused** — that session exists
for keepalive + state-marker tracking, not pane-bytes proxying.

**Phase 6 upgrade path**. Agent deployment opens one SSH session
per host; the agent multiplexes multiple panes over that one stdio
channel. Panes become cheap (one `OpenPane` wire frame each), and
the "how many connections am I opening?" question disappears from
the user's mental model.

### Orphaned old-version agent binaries on remote hosts (Phase 6 Slice 6b known limitation)

`tepegoz-ssh::deploy_agent` uploads to `$HOME/.cache/tepegoz/agent-v<N>` where N is the current `PROTOCOL_VERSION`. When N bumps (Phase 6 later slice or Phase 10 release), old `agent-v<N-1>` files stay behind on remote hosts — nothing garbage-collects them. Impact: a few hundred KB of stranded storage per host per protocol bump. Not a correctness bug (controller only ever looks at the current-N path) and not a 6b blocker per the brief. Recovery if it matters: `ssh <host> rm -f '~/.cache/tepegoz/agent-v*'` then reconnect to trigger a fresh deploy. Proper fix is a `cleanup_stale_versions` helper called from `deploy_agent` that rms any `agent-v*` not matching the current protocol version. Landing path: Phase 6 close polish or v1.1.

### Universal macOS `lipo` deferred to Phase 10 release pipeline

Slice 6b's brief permitted landing universal macOS `lipo` "if straightforward; defer to Phase 10 release pipeline with ISSUES pointer if painful." Skipped because `lipo` requires Xcode / `llvm-lipo` tooling that isn't guaranteed on developer machines or CI runners; fabricating it via zigbuild + custom wrapper scripts isn't worth the complexity for a slice whose value is the deploy pipeline, not the build ergonomics. Impact: controllers embed two separate darwin binaries (x86_64 + aarch64) rather than one fat binary. Total size overhead: ~250–400 KiB depending on release-agent profile compression. Phase 10 release pipeline revisits this alongside minisign signing + the full release artifact matrix — that's where an Xcode-bearing build machine is already a prerequisite. No ISSUES cross-references bite 6b's downstream work; remote deploy path already selects the right arch binary via `detect_target`.

### Panes past slot 9 are not reachable today (Slice 6.0 regression)

Slice 6.0 removed `Ctrl-b 0..9`, `Ctrl-b n`, `Ctrl-b p` in favor of
click-to-switch on the mouse-driven tab strip. The strip still caps
at 9 numbered slots + a `[+N]` overflow indicator; the `[+N]` glyph
is NOT clickable, and no keybind survives to reach the 10th+ pane.
Consequence: if a user stacks 10+ remote panes in a session, tabs
10 and beyond are unreachable until one of the first 9 is closed
(which shifts the stack). Was `Ctrl-b 0` → slot 10 pre-6.0;
removed as part of the keybind-simplification pass.

Workarounds today: close a visible pane via the `[×]` affordance
(or `Ctrl-b &`) to shift a hidden one into view; or use the
documented `tepegoz connect <alias>` CLI to drop directly into a
remote shell without going through the stack.

**Phase 6 upgrade path**. The agent-based multiplex (one SSH
connection per host, many panes over it) makes >9-pane workloads
much more likely, so Phase 6 revisits the overflow overlay — either
a clickable pane-list modal off the `[+N]` glyph, or an infinite-
scroll horizontal tab strip. The right design question will surface
concretely once the agent pipeline is in place; deferring until
then avoids locking in an overlay the agent UX might obsolete.

### `Ctrl-b w` pane-list overlay deferred to v1.1 *(superseded by Slice 6.0)*

5d-ii originally reserved `Ctrl-b w` for a pane-list overlay to
enumerate panes past slot 9. Slice 6.0 removed `Ctrl-b w` entirely
along with the other pane-nav keybinds; the overlay concept is
re-scoped under "Panes past slot 9 are not reachable today" above,
with the Phase 6 revisit owning the final design decision.

### Pane stack is session-local (5d-ii → Phase 6 agent consolidation)

The TUI's `pane_stack: Vec<PaneEntry>` is built from
`ensure_pane`'s startup response and live `PaneOpened` / `PaneExit`
events. On `Ctrl-b d` the socket closes; on reattach the new session
re-enters `ensure_pane`'s `ListPanes` reuse path, which picks a
single alive pane — the pre-detach stack structure (tab order, which
pane was active) is NOT restored. The user sees one pane in the tab
strip even if the daemon still has multiple alive panes.

**Phase 6 upgrade path**. The agent deployment unlocks a cleaner
reattach model: on handshake, the daemon returns the client's prior
session's stack ordering + active index via a new `SessionResume`
frame (wire bump). Until then, `tepegoz tui` reattach after detach
gives you one pane, and extra alive panes can be re-discovered via
`tepegoz doctor` (future) or by closing the attached pane to let
`ensure_pane` cycle through the next alive one.

### FIFO `OpenPane` correlation has edge cases (5d-ii → Phase 6 wire v10)

The wire protocol has no per-request id on `OpenPane`. Clients
correlate the `PaneOpened` response to their request by FIFO order
(the daemon processes commands serially on a single writer task, so
reply order matches request order). 5e's prefix-guard
(`info.message.starts_with("open pane")` / `"open remote pane"`)
keeps unrelated `Error` envelopes from mis-consuming the queue, but
the design still carries one thin edge: if an `OpenPane` fails and
then AttachPane against a DIFFERENT stale pane also fails with an
`"open pane"`-prefixed message (daemon-side malformation), the
mis-attribution reopens. Not observed in practice — daemon's
AttachPane error prefix is `"attach pane"`, distinct from OpenPane's.

**Phase 6 upgrade path**. Wire v10 (agent-backed panes) adds a
`request_id: u64` field to `OpenPane`'s `PaneOpened` and `Error`
responses, matching `DockerAction` / `FleetAction`. Client-side FIFO
correlation becomes unnecessary and the prefix-guard in the TUI's
Error handler becomes dead code to delete.

---

## Resolved

### ✅ Tab never reaches the pty shell — Slice 6.0.1 carve-out
Closed 2026-04-15 by the Slice 6.0.1 commit. `App::handle_tab` forwards `\t` / `\x1b[Z` to the active pane's SendInput when the PTY tile is focused; elsewhere Tab / Shift-Tab still cycle tile focus. Decision #7's Input / interaction amendment carries the carve-out wording. Pinned by `tab_on_pty_focus_forwards_tab_byte_to_pty_not_cycle` + `shift_tab_on_pty_focus_forwards_csi_z_to_pty_not_cycle` in `tepegoz-tui::app::tests`.

### ✅ Vim-preservation across Scope→Pane re-attach — moot after Decision #7
Closed 2026-04-14. The synthetic re-attach pattern was removed in C1.5b when the `View::{Pane, Scope}` mode model gave way to the tiled god view: vim lives in the always-on pty tile and its `AttachPane` subscription never tears down. The automated vt100 reconstruction test (`crates/tepegoz-core/tests/vim_preservation.rs`) was kept and repurposed as the per-CI pin for "bytes flowing into the pty tile render correctly through the vt100 parser." C1.5c's real-terminal run on 2026-04-14 confirmed vim renders + survives focus movement + detach/reattach with no visible corruption. The two fallback mitigations (resize-after-attach, keep-sub-alive-with-drop) are no longer applicable since there is no mode switch to interfere with.


### ✅ "TUI immediate-detach on attach" was user confusion, not a bug
Reported 2026-04-13, closed same day after reading `~/.cache/tepegoz/tui.log`. The log showed every `UserDetach` was preceded by real `\x02` + `d` bytes on stdin, and one session had the user pasting `./target/debug/tepegoz tui` *inside* the attached pane — the inner invocation hit the `TEPEGOZ_PANE_ID` guard, which the user read as an outer-shell error. Root cause: the pane's zsh prompt is visually identical to the outer shell, so there was no way to tell you were attached. Mitigation: TUI now sets an OSC 0 window title (`tepegoz · pane N`) on attach and clears it on detach, giving an unambiguous visual marker. `f12d194`'s tracing demoted from `info`/`warn` to `debug`.

### ✅ Scrollback/broadcast race duplicated bytes on attach · `eab274c`
The reader released the scrollback mutex between append and broadcast. Subscribers calling `subscribe()` in that window observed bytes in both snapshot and live stream — TUI rendered doubled prompts/lines on attach. Fix: hold the scrollback lock across both operations. Regression test: `tepegoz-pty::tests::subscribe_does_not_duplicate_bytes` (50 markers mid-stream, each must appear exactly once).

### ✅ Shell spawned in `$HOME`, not `current_dir` · `321ed5e`
`portable-pty::CommandBuilder` defaults `cwd` to `$HOME` when unset. TUI was sending `cwd: None`. Fix: TUI passes `std::env::current_dir()` in `OpenPaneSpec`. Regression test: `tepegoz-pty::tests::pane_honors_cwd_and_exposes_pane_id_env` (`pwd` output contains requested cwd).

### ✅ Recursive `tepegoz tui` glitched terminal · `321ed5e`
Running `tepegoz tui` inside an already-attached pane created a feedback loop: inner TUI's stdout was the pty slave, so every byte (alt-screen escapes, attach command, scrollback replay) looped back into the same pane's output, was rebroadcast to both subscribers, and written to both stdouts. Fix: daemon stamps `TEPEGOZ_PANE_ID=<id>` into pty env; TUI refuses to run if that var is set in its own env, with a clear error message pointing at `Ctrl-b d` to detach first.
