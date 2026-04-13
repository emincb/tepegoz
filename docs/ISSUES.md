# Issues

Active bugs and their diagnostic state. Resolved issues archived below with fix commit.

---

## Active

### 🟠 TUI immediate-detach on attach (opened 2026-04-13)

**Status.** Diagnostic tracing shipped at `f12d194`. Awaiting reproducer logs to identify the offending byte stream.

**Symptom.** `./target/debug/tepegoz tui` after a freshly-restarted daemon briefly shows an alt-screen (usually blank), then exits with `[detached — daemon and pane 1 still running]` before the user types anything. On returning to the real terminal, scrollback may appear wiped.

**What's known.**
- Exit reason is `ExitReason::UserDetach`. That variant is only produced when `InputFilter` returns `InputAction::Detach`.
- `InputFilter::Detach` only fires after seeing byte `0x02` (Ctrl-b) followed by `'d'` or `'q'`.
- Therefore stdin must deliver `\x02d` or `\x02q` (possibly split across reads — filter handles split correctly) before any user input.
- The `TEPEGOZ_PANE_ID` guard bails with a different error message, so that's not the trigger.
- Environment: macOS, Ghostty terminal, zsh.
- Race fix at `eab274c` and cwd/pane_id fixes at `321ed5e` are in place. All unit + integration tests pass. The bug is only visible in a real terminal with user shell integration.

**Hypotheses, ordered by likelihood.**
1. Ghostty or zsh shell-integration emits a sequence at alt-screen entry that coincidentally matches the detach prefix.
2. A leftover pane from a pre-fix daemon (opened without `TEPEGOZ_PANE_ID` env) interacts badly on reattach.
3. Terminal sends a DA/status-report response that contains `\x02` followed by `d`/`q`.
4. `tokio::io::stdin` returns unexpected bytes on first read under a condition I haven't reproduced locally.

**Diagnostic plan.**
- `f12d194` logs every `stdin.read` → `n=N, preview="<hex-escaped>"` and every InputFilter action (Forward len/preview, or Detach as warn) to `~/.cache/tepegoz/tui.log`.
- Next user run with a fresh daemon will surface the exact bytes delivered to stdin before detach. Source is then traceable.

**Reproduction attempts so far.**
- `script` on macOS: either closes stdin (→ `ExitReason::StdinClosed`) or sends `^D` (→ `ExitReason::PaneExited` via shell EOF). Neither triggers `UserDetach` — likely because the path needs a real tty and the user's specific shell-integration state.
- Tests pass because they exercise `PtyManager` + protocol directly without going through `stdin → InputFilter`.

**When resolved.**
- Land root-cause fix + regression test (shape depends on finding).
- Revert or demote `f12d194` tracing to `debug`.
- Mark Phase 2 ✅ in `docs/STATUS.md` and unblock Phase 3.

---

## Resolved

### ✅ Scrollback/broadcast race duplicated bytes on attach · `eab274c`
The reader released the scrollback mutex between append and broadcast. Subscribers calling `subscribe()` in that window observed bytes in both snapshot and live stream — TUI rendered doubled prompts/lines on attach. Fix: hold the scrollback lock across both operations. Regression test: `tepegoz-pty::tests::subscribe_does_not_duplicate_bytes` (50 markers mid-stream, each must appear exactly once).

### ✅ Shell spawned in `$HOME`, not `current_dir` · `321ed5e`
`portable-pty::CommandBuilder` defaults `cwd` to `$HOME` when unset. TUI was sending `cwd: None`. Fix: TUI passes `std::env::current_dir()` in `OpenPaneSpec`. Regression test: `tepegoz-pty::tests::pane_honors_cwd_and_exposes_pane_id_env` (`pwd` output contains requested cwd).

### ✅ Recursive `tepegoz tui` glitched terminal · `321ed5e`
Running `tepegoz tui` inside an already-attached pane created a feedback loop: inner TUI's stdout was the pty slave, so every byte (alt-screen escapes, attach command, scrollback replay) looped back into the same pane's output, was rebroadcast to both subscribers, and written to both stdouts. Fix: daemon stamps `TEPEGOZ_PANE_ID=<id>` into pty env; TUI refuses to run if that var is set in its own env, with a clear error message pointing at `Ctrl-b d` to detach first.
