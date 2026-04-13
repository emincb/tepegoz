# Issues

Active bugs and their diagnostic state. Resolved issues archived below with fix commit.

---

## Active

_(none)_

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
