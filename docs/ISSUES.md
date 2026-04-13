# Issues

Active bugs and their diagnostic state. Resolved issues archived below with fix commit.

---

## Active

### ⚠️ Vim-preservation across Scope→Pane re-attach — pending eyeball confirmation

Byte-level proxy (`crates/tepegoz-core/tests/vim_preservation.rs`) passes: `PaneSnapshot` after the synthetic re-attach contains the alt-screen entry, cursor positioning, and marker text. Real-terminal confirmation happens at C2c3 manual demo (Step 1 of the demo sequence in `docs/OPERATIONS.md`).

If vim breaks in a real terminal, fallback mitigations ranked by cost:

1. **Resize-after-attach (cheapest).** In `App::switch_to_pane`, after the new `AttachPane`, emit a second `ResizePane` with the current dims. The pty's child receives `SIGWINCH`; vim / htop / less / tmux / anything else that tracks terminal size all redraw on `SIGWINCH`. ~3-line change, no protocol churn.

2. **Keep `AttachPane` alive across mode switches (real refactor).** App keeps the pane subscription live in scope mode but drops incoming `PaneOutput` bytes on the floor instead of writing them to stdout. No synthetic re-attach needed; no scrollback re-transfer cost. Requires changing the `AppAction` set (no more `EnterPaneMode → Unsubscribe + AttachPane` on switch) and the runtime's mode-switch handling. Also requires the runtime to re-emit a synthetic "current screen" buffer on Scope→Pane to repaint whatever the screen looked like before the scope takeover. ~50-line change.

Pick (1) first if eyeball fails. Only escalate to (2) if (1) doesn't fix it.

---

## Resolved

### ✅ "TUI immediate-detach on attach" was user confusion, not a bug
Reported 2026-04-13, closed same day after reading `~/.cache/tepegoz/tui.log`. The log showed every `UserDetach` was preceded by real `\x02` + `d` bytes on stdin, and one session had the user pasting `./target/debug/tepegoz tui` *inside* the attached pane — the inner invocation hit the `TEPEGOZ_PANE_ID` guard, which the user read as an outer-shell error. Root cause: the pane's zsh prompt is visually identical to the outer shell, so there was no way to tell you were attached. Mitigation: TUI now sets an OSC 0 window title (`tepegoz · pane N`) on attach and clears it on detach, giving an unambiguous visual marker. `f12d194`'s tracing demoted from `info`/`warn` to `debug`.

### ✅ Scrollback/broadcast race duplicated bytes on attach · `eab274c`
The reader released the scrollback mutex between append and broadcast. Subscribers calling `subscribe()` in that window observed bytes in both snapshot and live stream — TUI rendered doubled prompts/lines on attach. Fix: hold the scrollback lock across both operations. Regression test: `tepegoz-pty::tests::subscribe_does_not_duplicate_bytes` (50 markers mid-stream, each must appear exactly once).

### ✅ Shell spawned in `$HOME`, not `current_dir` · `321ed5e`
`portable-pty::CommandBuilder` defaults `cwd` to `$HOME` when unset. TUI was sending `cwd: None`. Fix: TUI passes `std::env::current_dir()` in `OpenPaneSpec`. Regression test: `tepegoz-pty::tests::pane_honors_cwd_and_exposes_pane_id_env` (`pwd` output contains requested cwd).

### ✅ Recursive `tepegoz tui` glitched terminal · `321ed5e`
Running `tepegoz tui` inside an already-attached pane created a feedback loop: inner TUI's stdout was the pty slave, so every byte (alt-screen escapes, attach command, scrollback replay) looped back into the same pane's output, was rebroadcast to both subscribers, and written to both stdouts. Fix: daemon stamps `TEPEGOZ_PANE_ID=<id>` into pty env; TUI refuses to run if that var is set in its own env, with a clear error message pointing at `Ctrl-b d` to detach first.
