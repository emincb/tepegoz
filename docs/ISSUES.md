# Issues

Active bugs and their diagnostic state. Resolved issues archived below with fix commit.

---

## Active

_(none)_

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

### `Ctrl-b w` pane-list overlay deferred (5d-ii → 5e or v1.1)

5d-ii lands the pane-stack with `Ctrl-b 0..9` jump + `Ctrl-b n`/`p`
cycle. The list-view overlay for >9 panes is scoped for 5e polish.
Users with >9 concurrent panes today can still navigate via cycling;
jump-by-number just wraps at 9. Not a bug, but flag as a v1.1 polish
item if real-world usage surfaces it.

---

## Resolved

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
