# Handoff

Session-boundary handshake between the CTO (planner/architect) and the engineer. Both sides write here before their context clears; both sides read both sections when starting a fresh session.

Not a journal. Not a changelog. Captures *in-flight thinking that isn't in the canonical docs yet*:

- `docs/STATUS.md` is authoritative for current phase state.
- `docs/DECISIONS.md` is authoritative for locked architectural commitments.
- `docs/ROADMAP.md` is authoritative for phase plans.
- `docs/ISSUES.md` is authoritative for active bugs.
- **HANDOFF.md is authoritative for what's in the CTO's or engineer's head that hasn't yet crystallized into those docs.**

When docs and HANDOFF conflict, docs win. Update HANDOFF (or delete the stale entry), don't act on stale planning notes.

---

## CTO section

**Last updated:** 2026-04-14, post-C1.5c sign-off, C3 authorized and in engineer's hands.

### What I just signed off on
- **C1.5c manual demo** passed in the user's real terminal. All 8 acceptance steps green: god-view first-run layout, PTY tile live, focus nav natural, vim preserves across focus moves, Docker tile live + concurrent with PTY, engine-unavailable recovery, detach/reattach preserves state. Phase 2 invariant intact.
- **Decision #7 (tiled god view, opinionated default, vt100 via `vt100` crate)** locked in `docs/DECISIONS.md`.
- **C3 authorized** with three UX clarifications locked: logs panel is a sub-state within the Docker tile (not a modal); toast overlay is a 1-line strip at the bottom of the scope row with auto-dismiss (~3s success / ~8s failure) and stack-of-3; confirm modal for K/X is inline within the Docker tile's Rect (not full-screen), 10s auto-cancel, `Ctrl-b k` focus-away also cancels.

### What's in flight with the engineer
- **Slice C3 — Docker scope actions + toasts + logs panel.** Sub-landing structure I directed:
  - **C3a:** `r`/`s`/`K`/`X` keybinds + confirm modal for K/X + pending-action 30s timeout sweep + toast overlay primitive (stacking, auto-dismiss, non-blocking).
  - **C3b:** `l` keybind opens logs sub-state in Docker tile; `Subscribe(DockerLogs)` on entry, `Unsubscribe` on exit; scrolling transcript with `j`/`k`/PgUp/PgDn/G navigation, `Esc`/`q` returns to list; `DockerStreamEnded` renders a terminal line.
  - **C3c:** Opt-in end-to-end test via `TEPEGOZ_DOCKER_TEST=1` provisioning an alpine container; update `docs/OPERATIONS.md` with a C3 manual demo script.
- Reporting cadence: engineer pings per sub-slice when CI-green on both OSes and pushed.

### What I'm expecting next
- Engineer's C3a ping (CI-green on both OSes, pushed). I read and sign off or redirect before C3b starts.
- Same cadence for C3b and C3c.
- After C3c CI-green: user runs the C3 manual demo (direct them with a pass/fail matrix like I did for C1.5c). If they sign off, Phase 3 closes (`docs/STATUS.md` row 3 → ✅).

### Open questions I'm holding (not yet in DECISIONS.md)

- **Slice D (`DockerExec` → new pane) design pass is blocking.** Under Decision #7 the layout is fixed with exactly one pty tile. "Open a new pane" in the tiled world is architecturally non-trivial — three options I've been weighing: (a) replace the current pty tile contents with the exec session, stash the original as a detach-able background pane; (b) treat the pty tile as a tab-strip of panes, with a thin tab bar at the top of the tile; (c) defer `DockerExec` to v1.1 entirely. None are obvious. **Do not let the engineer start Slice D coding off C3 momentum** — they must ping for a design pass first. I'd run that design pass the same way as Slice C (3–5 specific questions, engineer proposal, user signoff before code).
- **Phase 4 (Ports + Processes panels)** is the natural next phase once Phase 3 closes. Tiles already exist as placeholders in the god-view layout. The subscription pattern is mature (uniform `HashMap<id, AbortHandle>` model documented in `docs/ARCHITECTURE.md` §9). Should be the smoothest phase yet if we stick to the pattern.
- **OSC 0 title refresh on focus change** was left stubbed in C1.5b (`AppAction::FocusTile(TileId)` only debug-logs). Candidate future use: update `tepegoz · [PTY]` / `[Docker]` / etc. when focus moves. Don't force it; land if it genuinely helps the user distinguish focus externally.

### Observations about engineer patterns (load-bearing for future direction)

- Highly disciplined at diagnose-before-fixing. At C2 gate they caught a real daemon bug (pane_subs leak) while trying to build the vim-preservation test — refused to ship a test that didn't exercise the real mechanism, which surfaced an invisible zombie-task leak that would have shown up as "daemon feels slow" weeks later.
- Strong commit hygiene: messages capture *why* and blast-radius, not just *what*. Commit message for `43b28eb` is a good reference model.
- Good at salvage logic during pivots: during the C1.5 tiling correction, explicitly called out what survives from C1/C2 and what goes, updated docs in the same commit as the pivot. Minimal rework churn.
- Executes cross-OS CI discipline (two-OS green on every push) without me asking. Caught the `printf \x1b` vs `\033` POSIX gotcha via CI, not local-only testing.
- Volunteers judgment calls at the right level: flags 3–5 tactical decisions per slice for review, doesn't flag every naming choice. Matches the `feedback_implementation_autonomy` model.

### Standing context (if you're the fresh CTO reading cold)

- You are the CTO / planner / architect on this project. User promoted you 2026-04-13. You don't write code; the engineer does. Your job is proposal review, architectural sign-off, ordering of work, and flagging product-level drift.
- The user relays between two Claude Code sessions (you + engineer). The engineer doesn't see your reasoning, only the directives the user relays. Write the engineer-facing messages as self-contained, unambiguous, and ordered — they should pick up cold from the relay.
- The project's spec hierarchy is `README.md` + mockup first, `docs/DECISIONS.md` second, other `docs/` third. Check README before signing off on UX proposals (see memory: `feedback_cross_check_vision_before_signoff.md`).
- Six locked architectural decisions in `docs/DECISIONS.md`; changing any of those requires user sign-off. #7 (tiled god view) was added 2026-04-14.
- Working memory: `~/.claude/projects/-Users-emin-Documents-projects-personal/memory/` — `MEMORY.md` is the index.

---

## Engineer section

**Last updated:** 2026-04-14, post-C3c land.

### Where I left off

C3a + C3b + C3c shipped. Three commits on `main`. C3c is one commit: the integration test + 9-scenario manual demo script. 165 tests green, fmt + clippy clean.

C3c covers:

- New integration test `crates/tepegoz-core/tests/docker_scope.rs::restart_propagates_to_follow_up_container_list`. Opt-in `TEPEGOZ_DOCKER_TEST=1`. Provisions a unique-per-PID alpine container, connects a client, subscribes to `Docker`, captures pre-restart (state, status), sleeps 2 s so "Up N seconds" advances visibly, sends `DockerAction::Restart` with a known `request_id`, then drains events until (a) matching `DockerActionResult::Success` AND (b) a post-Success ContainerList shows `state != pre_state || status != pre_status`. Force-removes on Drop. Verified locally against Docker Desktop in ~6 s. CI runs with the env var unset so it skips cleanly.
- Fully rewritten `docs/OPERATIONS.md` "Slice C3 manual demo prep" — 9 scenarios + pass/fail matrix. Replaces the stub Step 7 from the C1.5c section.
- `docs/STATUS.md` gets a new "Phase 3 polish candidates" section recording the three CTO-flagged non-urgent follow-ups (bounded `tail_lines` default, logs tile zoom, color palette feedback revisit).

C3a + C3b recap still valid — see commit `8a9176c` (C3a) and `fc5ded4` (C3b) for full scope. The TUI has: r/s immediate dispatch, K/X inline confirm modal with K→K absorption, pending-action 30 s timeout sweep, toast overlay (stack of 3, drop-oldest, per-kind auto-dismiss), logs panel as Docker-tile sub-state with DockerStreamEnded handling + read-only transcript.

Test count: 165 (C3b was 164; +1 for the C3c integration test). Lib tests unchanged from C3b's 130.

C3b body covers:

- `DockerScope.view: DockerView::{List, Logs(LogsView)}`. `LogsView` carries container_id, container_name, sub_id, `lines: VecDeque<LogLine>` capped at `MAX_LOG_LINES = 10_000` (drop-oldest), per-stream `pending_stdout/pending_stderr: Vec<u8>` for chunks that split mid-line, `scroll_offset`, `at_tail`, `stream_ended: Option<String>`.
- `l` on the focused list view with a selected container sends `Subscribe(DockerLogs { id, container_id, follow: true, tail_lines: 0 })` and transitions to `Logs(...)`. No-op when nothing selected, empty list, or Docker Unavailable.
- `ContainerLog { stream, data }` on the logs sub feeds `LogsView::ingest`, which appends to the per-stream pending buffer and flushes every `\n`-terminated line into the capped ring. CRLF detected and both bytes stripped. stdout/stderr stay separate so an interleaved stderr line doesn't corrupt a stdout half-line.
- `DockerStreamEnded { reason }` flushes trailing pending bytes as a final line, records the reason on `stream_ended`, disables `at_tail`. Renderer paints a dimmed "— log stream ended: `<reason>` —" line.
- Scroll: `j`/`k`/Down/Up by 1; PgUp/PgDn by `LOGS_PAGE_LINES = 10`; `G`/End/Bottom jump-to-tail + re-enable `at_tail`. Scrolling up sets `at_tail=false`; reaching offset 0 via scroll-down re-enables it. `Esc`/`q` unsubscribe + return to List.
- Logs view persists across focus moves (unlike `pending_confirm`). Action keybinds (`r`/`s`/`K`/`X`/`l`/filter) are all ignored while logs are showing.
- Stale `ContainerLog`/`DockerStreamEnded` events on the now-unsubscribed sub id drop silently via `DockerScope::is_current_logs_sub(id)`.
- `ScopeKey::PgUp` + `PgDn` added; CSI parser extended (`~5` → PgUp, `~6` → PgDn).
- Render: scope::docker::render dispatches on view. Logs view layout is `[status(1), body(Min), help(1)]`. Status shows line count + tail on/off + stream live/ended[:reason]. Body paints each `LogLine` with stream-colored text (stdout gray `" "` prefix, stderr yellow `"!"` prefix). Help bar swaps to `[j/k] scroll · [PgUp/PgDn] page · [G] tail · [Esc/q] back`. Confirm modal is suppressed while in logs view (guarded in `render_list_view`).

Test count: 164 (up from 143). tepegoz-tui lib tests: 130 (app 69, scope::docker 14, input 22, tile 13, toast 5, pty_tile 3, scope::placeholder 3, helpers 1).

### What I'm mid-flight on

_Nothing._ Waiting on CTO review of C3c. After sign-off, the user runs the manual demo in a real terminal to close Phase 3.

### What I'm expecting from the CTO next

- Sign-off on C3c or redirect on the integration-test shape / demo-script contents.
- User runs the 9-scenario manual demo in a real terminal. Pass/fail matrix in `docs/OPERATIONS.md` "Slice C3 manual demo prep". Scenarios 1–8 gate Phase 3 close; scenario 9 (tile-sized logs sanity) is observational.
- If user signs off on 1–8: Phase 3 row in `docs/STATUS.md` goes ✅. Any gotchas from scenario 9 land in `docs/ISSUES.md` as a Phase-3-polish item; not a blocker.
- Then Slice D design pass (the blocker before any Slice D code can start — fixed layout has exactly one pty tile, so "DockerExec → new pane" is architecturally non-trivial; CTO has three candidate approaches he wants to run a design pass on before approving implementation).
- Phase 4 (Ports + Processes panels) is the natural next phase once Phase 3 closes. Tiles already exist as placeholders in the god-view layout; the subscription pattern is mature. Should be the smoothest phase yet if we stick to the form.

### Anything that would surprise a fresh-me

- **`push_toast` has a `push_toast_at(now, ...)` variant for the sweep.** Toast `expires_at` is computed from an explicit `Instant` rather than always `Instant::now()` so the `sweep_expired(now)` code path doesn't evict a freshly-pushed timeout toast in the same pass. Tests pass synthetic "31 s in the future" nows to simulate time travel.
- **The `next_sub_id` allocator is shared across subscription ids and DockerAction request ids and the per-container DockerLogs sub id.** The daemon correlates by id embedded in the payload, so collision between namespaces is a non-issue. One monotonic counter keeps everything simple.
- **Focus-away cancellation fires *before* the focus mutation** in `handle_focus_direction`, so the old focus (`TileId::Docker`) is still observable when we clear `pending_confirm`. Order matters.
- **Confirm modal takes priority in `handle_scope_key` over the filter branch.** Logs view takes priority over *both* — if `view == Logs`, handle_logs_key runs and returns before confirm/filter even get checked. (Can't actually coexist today — `l` fails while a confirm is pending, because confirm absorbs or cancels `l`. But the handler order is the right defense.)
- **K/X during an already-open confirm are absorbed, not cancels.** If you press K then K again, the modal stays showing Kill (does not toggle off, does not switch to a second Kill with fresh deadline, does not switch target). Per CTO push-back on C3a. The old "any non-y cancels" rule is replaced: y/Y confirms, K/X absorb, anything else cancels.
- **R and S (capitals) are now no-ops on the list view.** Only lowercase `r`/`s` dispatch actions. Rule: capital = destructive (K/X); lowercase = safe/navigation (r/s/j/k/h/l). Previously C3a had `r|R` and `s|S` as case-insensitive aliases; removed per CTO push-back to unify the convention.
- **Toast rendering lives in `toast.rs`, not `session.rs`.** Pulled into its own module so render tests verify overlay positioning against a real `TileLayout` without dragging the full runtime in. Same pattern should apply to Phase 4/5/7 scope panels.
- **`LogsView::container_id` is `#[allow(dead_code)]`-marked.** The renderer displays `container_name` instead (shorter + readable); the id is kept for tests and any future "reopen logs after reconnect" flow. Fully intentional.
- **CRLF handling in `LogsView::ingest`.** A trailing `\r` before the `\n` is stripped along with the `\n` so Windows-container logs render cleanly. Tested (`crlf_terminated_lines_strip_both_bytes`).
- **`tail_lines: 0` on `Subscribe(DockerLogs)` means "all history"** per Slice B's wire contract. For a chatty long-lived container this dumps megabytes on `l` press. Flagged to CTO as a Phase-3-polish candidate; not added to C3b scope. If you're adding it, `1000` is the sensible default with a way to grab "more" later.
- **`MAX_LOG_LINES = 10_000` is the buffer cap.** Oldest drops on overflow. Memory ≈ 1–2 MiB for typical lines. Testable via `max_log_lines_drops_oldest`.
