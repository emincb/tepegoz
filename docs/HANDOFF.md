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

**Last updated:** 2026-04-14, post-C3c sign-off. User manual demo is the gate. Phase 3 closure pending user's 9-scenario run.

### What I just signed off on
- **C3c integration test + 9-scenario manual demo script.** Commit `4dd1208`. Integration test `restart_propagates_to_follow_up_container_list` at `crates/tepegoz-core/tests/docker_scope.rs` — opt-in `TEPEGOZ_DOCKER_TEST=1`, provisions unique-per-PID alpine, 2s sleep to advance status counter, asserts `DockerActionResult::Success` + follow-up `ContainerList` reflects the restart, force-remove on Drop. 9-scenario manual demo in `docs/OPERATIONS.md:207`. Scenarios 1–8 are the gate for Phase 3; scenario 9 (tile-sized logs sanity) is advisory.
- **C3a** (`8a9176c`) — `r`/`s`/`K`/`X` keybinds, confirm modal for K/X, pending-action 30s timeout sweep, toast overlay. Push-back on `r|R`/`s|S` aliases (capital-R-as-no-op) landed in C3b's head.
- **C3b** (`fc5ded4`) — Logs panel as `DockerView::{List, Logs(LogsView)}` sub-state within Docker tile. `l` subscribes; `Esc/q` unsubscribes. Stream-colored (stdout gray, stderr yellow + `!` prefix), CRLF handling, `MAX_LOG_LINES = 10_000` drop-oldest, per-stream pending buffers for split chunks. C3a head cleanup also landed here (capital R no-op, K/X absorption during confirm, strengthened 10s auto-cancel test).
- **Decision #7 (tiled god view)** locked at C1.5a (`2c54c44`). Six C3 UX clarifications locked before code (logs sub-state not modal; toast positioning/stacking; confirm modal inline; K/X absorption during confirm; capital-discipline rule; `tail_lines: 0` semantics).

### What's in flight with the engineer
_Nothing._ Engineer is waiting on the user's manual demo result.

### What I'm expecting next
- **User runs the 9-scenario demo** per `docs/OPERATIONS.md` "Slice C3 manual demo prep". Reports pass/fail on scenarios 1–8 (the gate). Scenario 9 observation captured in `docs/ISSUES.md` if cramped but doesn't block close.
- **If user signs off:** direct engineer to close Phase 3 — flip `docs/STATUS.md` row 3 to ✅, update `docs/HANDOFF.md` CTO section, then flag Slice D design pass (not code) as the next gate before anything new lands.
- **If any scenario 1–8 fails:** inline fix commit before Phase 3 closes (same pattern as C1.5c's potential fallback).

### Open questions I'm holding (not yet in DECISIONS.md)

- **Slice D (`DockerExec` → new pane) design pass is blocking.** Under Decision #7 the layout is fixed with exactly one pty tile. "Open a new pane" in the tiled world is architecturally non-trivial — three options I've been weighing: (a) replace the current pty tile contents with the exec session, stash the original as a detach-able background pane; (b) treat the pty tile as a tab-strip of panes, with a thin tab bar at the top of the tile; (c) defer `DockerExec` to v1.1 entirely. None are obvious. **Do not let the engineer start Slice D coding off C3 momentum** — they must ping for a design pass first. I'd run that design pass the same way as Slice C (3–5 specific questions, engineer proposal, user signoff before code).
- **Phase 4 (Ports + Processes panels)** is the natural next phase once Phase 3 closes. Tiles already exist as placeholders in the god-view layout. The subscription pattern is mature (uniform `HashMap<id, AbortHandle>` model documented in `docs/ARCHITECTURE.md` §9); scope renderers take `(state, Frame, Rect)`; the toast + pending-action + focus-nav bus is in place. Should be the smoothest phase to date if we stick to the form.
- **Phase 3 polish candidates** (tracked in `docs/STATUS.md`): (1) bounded `tail_lines` default (`1000` instead of `0` with a "load more" affordance); (2) logs-tile zoom / temporary full-scope-row expansion if cramped; (3) color palette revisit if stderr-yellow or stdout-gray has readability issues on some themes. None blocking; pick up when signal says so.
- **OSC 0 title refresh on focus change** was left stubbed in C1.5b (`AppAction::FocusTile(TileId)` only debug-logs). Candidate future use: update `tepegoz · [PTY]` / `[Docker]` / etc. when focus moves. Don't force it; land if it genuinely helps the user distinguish focus externally.

### Observations about engineer patterns (load-bearing for future direction)

- Highly disciplined at diagnose-before-fixing. At C2 gate they caught a real daemon bug (pane_subs leak) while trying to build the vim-preservation test — refused to ship a test that didn't exercise the real mechanism, which surfaced an invisible zombie-task leak that would have shown up as "daemon feels slow" weeks later.
- Strong commit hygiene: messages capture *why* and blast-radius, not just *what*. Commit messages for `43b28eb`, `c7b336d`, and `4dd1208` are good reference models.
- Good at salvage logic during pivots: during the C1.5 tiling correction, explicitly called out what survives from C1/C2 and what goes, updated docs in the same commit as the pivot. Minimal rework churn.
- Executes cross-OS CI discipline (two-OS green on every push) without me asking. Caught the `printf \x1b` vs `\033` POSIX gotcha via CI, not local-only testing.
- Volunteers judgment calls at the right level: flags 3–5 tactical decisions per slice for review, doesn't flag every naming choice. Matches the `feedback_implementation_autonomy` model.
- Adopted defensive testing patterns without prompting: `push_toast_at(now, ...)` for time-travel-in-tests, 2-second sleep for status-counter-advance in restart round-trip, SIGSTOP-dockerd for timeout demo. Recognizes when a test would otherwise be flaky or misleading and fixes the test design, not just the assertion.

### Standing context (if you're the fresh CTO reading cold)

- You are the CTO / planner / architect on this project. User promoted you 2026-04-13. You don't write code; the engineer does. Your job is proposal review, architectural sign-off, ordering of work, and flagging product-level drift.
- The user relays between two Claude Code sessions (you + engineer). The engineer doesn't see your reasoning, only the directives the user relays. Write the engineer-facing messages as self-contained, unambiguous, and ordered — they should pick up cold from the relay.
- The project's spec hierarchy is `README.md` + mockup first, `docs/DECISIONS.md` second, other `docs/` third. Check README before signing off on UX proposals (see memory: `feedback_cross_check_vision_before_signoff.md`).
- Six locked architectural decisions in `docs/DECISIONS.md`; changing any of those requires user sign-off. #7 (tiled god view) was added 2026-04-14.
- Working memory: `~/.claude/projects/-Users-emin-Documents-projects-personal/memory/` — `MEMORY.md` is the index.

---

## Engineer section

**Last updated:** 2026-04-14, Phase 3 closed.

### Where I left off

**Phase 3 is closed.** User's 9-scenario manual demo signed off green on 2026-04-14 against a real terminal; docs flipped to ✅ in the close commit. 165 tests green on both OSes, `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` clean. Docker scope panel ships end-to-end: daemon-side container list + lifecycle actions + logs/stats streaming, client-side tiled god view + action keybinds + confirm modal + toast overlay + logs sub-state.

Scenario 9 (tile-sized logs sanity) was advisory per CTO directive; user did not flag readability gotchas, so the Phase 3 polish list in `docs/STATUS.md` stands as-is (bounded `tail_lines` default, logs-tile zoom if cramped, color palette revisit on feedback).

### What I'm mid-flight on

_Nothing._ Awaiting CTO direction on Slice D design pass vs. pivot-to-Phase-4.

### What I'm expecting from the CTO next

Either:

- **Slice D design-pass proposal direction.** Three candidate approaches are on the table in the CTO section open questions above: (a) replace pty tile contents with exec session, stash original as detach-able background pane; (b) treat pty tile as tab-strip with thin tab bar; (c) defer `DockerExec` to v1.1. Under Decision #7's fixed layout these are non-trivial architectural calls (pane lifecycle model, `tepegoz tui` re-attach target, `ListPanes` semantics, possibly a new daemon pane kind). I'd want the proposal-pass pattern Slice C1.5 used: 3–5 specific questions from CTO, engineer proposal, user sign-off, then code.
- **Or a pivot to Phase 4 (Ports + Processes panels)** if the user decides Slice D defers to v1.1. Phase 4 slots into existing placeholder tiles; the subscription pattern + `(state, Frame, Rect, focused)` scope-renderer contract + toast + pending-action + focus-nav buses are all in place. Would be the smoothest phase to date if we stick to the form.

### Anything that would surprise a fresh-me

- **Capital-discipline rule on action keybinds.** Lowercase = safe / navigation (`r` restart, `s` stop, `j`/`k` navigate, `l` logs); CAPITAL = destructive (`K` kill, `X` remove). Capital `R` and capital `S` are explicit no-ops on the Docker list view — *not* case-insensitive aliases. Rationale: caps-lock can't silently escalate a safe action into something unexpected, and the "caps = stop-to-think" muscle memory stays consistent when Slice D / Phase 4+ add more keybinds. Pinned by the `capital_r_with_docker_focus_is_noop` test. Adding a new destructive action? Bind it to a capital. New safe one? Lowercase only.
- **K/X during an already-open confirm are ABSORBED, not cancels.** Pressing K then K again keeps the modal showing Kill — does NOT toggle off, does NOT switch to a second Kill with a fresh deadline, does NOT switch target. Same for X during Kill-pending or K during Remove-pending. Rationale: a second K/X is most likely "still intending to confirm" muscle memory; cancelling on repeat keypress would surprise the user. Contract: `y`/`Y` confirms, K/X absorb, anything else cancels. Pinned by `second_k_during_kill_pending_is_absorbed` + `x_during_kill_pending_is_absorbed`.
- **`MAX_LOG_LINES = 10_000` is the LogsView buffer cap.** Ring-style `VecDeque<LogLine>` with drop-oldest on overflow. Memory ~1–2 MiB for typical lines; higher for long-line content. Pinned by `max_log_lines_drops_oldest`. If scrollback-beyond-cap ever matters (e.g., a "load more" affordance), reopen with a higher cap or bolt on pagination — don't just raise the cap without bounding total memory.
- **`tail_lines: 0` on `Subscribe(DockerLogs)` means "all history"**, not "no history" — Slice B wire contract. On a chatty long-lived container pressing `l` dumps megabytes on entry. Flagged in `docs/STATUS.md` Phase 3 polish candidates; not fixed in C3. If you implement the `tail_lines: 1000` default with a "load more" affordance, the wire is already there — daemon honors whatever value the client sends.
- **The 2-second sleep in the restart round-trip test is load-bearing.** `crates/tepegoz-core/tests/docker_scope.rs::restart_propagates_to_follow_up_container_list` snapshots pre-restart `state`/`status`, sleeps 2 s so "Up N seconds" advances visibly, THEN sends Restart. Without the sleep, a fast-enough restart lands with the "Up 1 second" counter unchanged → the post-Success `ContainerList` looks identical to the pre-restart one, the assertion fires on a shift that never happened, and the test ships flaky. If you add a similar timer-dependent integration test (Phase 4 Processes CPU% sampling has analogous dynamics), bake in the deliberate pre-event sleep rather than trusting the post-event timing to be distinguishable.

Carried forward from Slice C3 as still relevant:

- **`push_toast` has a `push_toast_at(now, ...)` variant for the sweep.** Toast `expires_at` is computed from an explicit `Instant` rather than always `Instant::now()` so the `sweep_expired(now)` code path doesn't evict a freshly-pushed timeout toast in the same pass. Tests pass synthetic "31 s in the future" nows to simulate time travel.
- **The `next_sub_id` allocator is shared across subscription ids and DockerAction request ids and the per-container DockerLogs sub id.** The daemon correlates by id embedded in the payload, so collision between namespaces is a non-issue. One monotonic counter keeps everything simple.
- **Focus-away cancellation fires *before* the focus mutation** in `handle_focus_direction`, so the old focus (`TileId::Docker`) is still observable when we clear `pending_confirm`. Order matters.
- **Confirm modal takes priority in `handle_scope_key` over the filter branch.** Logs view takes priority over *both* — if `view == Logs`, `handle_logs_key` runs and returns before confirm/filter are even checked. Handler order is the right defense even if these states can't actually coexist today.
- **Toast rendering lives in `toast.rs`, not `session.rs`.** Pulled into its own module so render tests verify overlay positioning against a real `TileLayout` without dragging the full runtime in. Same pattern should apply to Phase 4/5/7 scope panels.
- **`LogsView::container_id` is `#[allow(dead_code)]`-marked.** The renderer displays `container_name` instead (shorter + readable); the id is kept for tests and any future "reopen logs after reconnect" flow. Fully intentional.
- **CRLF handling in `LogsView::ingest`.** A trailing `\r` before the `\n` is stripped along with the `\n` so Windows-container logs render cleanly. Tested (`crlf_terminated_lines_strip_both_bytes`).
