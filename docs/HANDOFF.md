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

**Last updated:** 2026-04-14, post-C3a land.

### Where I left off

C3a shipped. One commit, 143 tests green, fmt + clippy clean. Committed and pushed to `main`. Covers:

- `r`/`R` → `DockerAction::Restart`; `s`/`S` → `Stop`. Immediate dispatch on the focused Docker tile, each inserting a `PendingAction { deadline: now + 30s, description }` keyed by `request_id`.
- `K` → pending-confirm for `Kill`; `X` → pending-confirm for `Remove`. `y`/`Y` dispatches; any other key cancels. Focus-away from Docker cancels. 10 s idle auto-cancel.
- Inline confirm modal rendered inside the Docker tile's `Rect` (not full-screen; preserves Decision #7 all-scopes-visible). Other tiles keep rendering + receiving input throughout.
- Toast overlay in a new `toast.rs` module, rendered as a 1-line strip directly above the Claude Code tile. `App::toasts: VecDeque<Toast>`, bounded to `MAX_TOASTS = 3`, drop-oldest on overflow. Per-kind auto-dismiss (Success 3 s, Error 8 s, Info 4 s). Never blocks keystrokes.
- Pending-action 30 s timeout sweep runs on every 30 Hz Tick via `App::sweep_expired(Instant::now(), ...)`. Expired entries emit an "`<verb> <name>` timed out — check engine" Error toast and are removed from `pending_actions`.
- `DockerActionResult::Success`/`Failure` now correlate against `pending_actions` via `request_id`; Success emits green "`<verb> <name>` — succeeded" toast, Failure emits red "`<verb> <name>` failed: `<reason>`" (reason verbatim from dockerd). Stale results (no matching pending) fall back to `<verb> <container_id>`.
- `Payload::Error` daemon error lands in the toast overlay queue (previously log-only).
- `AppEvent::PendingActionTimeout(id)` wire retained on the event surface for a future dedicated sweeper. Exercised by tests.

Test count: 143 (up from 114). Lib tests in `tepegoz-tui`: 109 (app 51, tile 13, input 22, pty_tile 3, scope::docker 11, scope::placeholder 3, toast 5, helpers 1).

### What I'm mid-flight on

_Nothing._ Waiting on CTO review of C3a before starting C3b.

### What I'm expecting from the CTO next

- Sign-off on C3a or redirect.
- If signed off: authorize C3b (logs panel as Docker-tile sub-state per UX clarification #1). My implementation sketch:
  - Add a `LogsView { container_id, sub_id, log_lines: Vec<LogLine>, scroll_offset: usize, at_tail: bool }` variant to `DockerScope` alongside the current container-list view (introduce a `DockerView` enum: `List | Logs(LogsView)`).
  - `l` on list view → allocate sub id, send `Subscribe(DockerLogs { id, container_id, follow: true, tail_lines: 0 })`, transition to `Logs(...)`. Help bar swaps to logs-mode hint.
  - `ContainerLog { stream, data }` events on the logs sub id append to the transcript with stream-colored formatting (stdout neutral, stderr yellow). Each newline is a `LogLine`; chunked writes buffer until newline or tile redraw.
  - `j`/`k`/`PgUp`/`PgDn` scroll; `G` jumps to bottom + sets `at_tail = true`. `at_tail` stays true until the user scrolls up, which sets it false; new lines only auto-scroll when `at_tail`.
  - `Esc` or `q` → `Unsubscribe { id }`, drop back to `List`.
  - `DockerStreamEnded { reason }` renders a terminal "— log stream ended: `<reason>` —" Line and disables auto-tail.
- For C3c: end-to-end test in `crates/tepegoz-core/tests/` (opt-in `TEPEGOZ_DOCKER_TEST=1`) that provisions alpine, drives Restart through the daemon wire, asserts Success + follow-up `ContainerList` reflects the change. Plus `docs/OPERATIONS.md` manual demo script.

### Anything that would surprise a fresh-me

- **`push_toast` has a `push_toast_at(now, ...)` variant for the sweep.** Toast `expires_at` is computed from an explicit `Instant` rather than always `Instant::now()` so the `sweep_expired(now)` code path doesn't evict a freshly-pushed timeout toast in the same pass. Tests pass synthetic "31 s in the future" nows to simulate time travel; the renderer + state don't care, they just respect the stored `expires_at`.
- **The `next_sub_id` allocator is shared across subscription ids and DockerAction request ids.** The daemon correlates each response by the id embedded in the payload, so collision between namespaces is a non-issue. One monotonic counter keeps `alloc_sub_id` simple.
- **Focus-away cancellation fires *before* the focus mutation** in `handle_focus_direction`, so the old focus (`TileId::Docker`) is still observable when we clear `pending_confirm`. Order matters.
- **Confirm modal takes priority in `handle_scope_key` over the filter branch.** If both could be active simultaneously (they can't today — begin_confirm no-ops when list is empty + filter-active means the user was narrowing the list, but a pending_confirm can still stick around if the filter is typed mid-prompt), confirm wins.
- **Toast rendering lives in `toast.rs`, not `session.rs`.** Pulled into its own module so render tests can verify overlay positioning against a real `TileLayout` without dragging the full runtime in. `session::render_tiles` calls it after all normal tiles render.
- **CI will need `TEPEGOZ_DOCKER_TEST=1` off by default** — all C3a tests are hermetic unit tests (no daemon, no docker engine). The opt-in integration tests in `docker_scope.rs` are unchanged and still `#[ignore]` behind the env var.
- **The `docker_action_result_success_does_not_toast_yet` test was rewritten in place**, not deleted, as `docker_action_result_success_toasts_with_description`. Old semantics (no toast yet) are gone.
