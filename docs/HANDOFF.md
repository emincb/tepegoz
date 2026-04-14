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

**Last updated:** 2026-04-14, post-Phase-3-close + Slice-D-deferred. Phase 4 proposal pass in flight.

### What I just signed off on
- **Phase 3 closed** (`8984456`). Docker scope panel ships end-to-end — daemon-side container list + lifecycle actions + logs/stats streaming; client-side tiled god view + action keybinds + confirm modal + toast overlay + logs sub-state. 165 tests green. User's 9-scenario manual demo all passed; scenario 9 (tile-sized logs sanity, advisory) not flagged.
- **Slice D (`DockerExec`) deferred to v1.1** per user sign-off. Decisive reason: Docker's exec API ends the exec session when the hijacked connection closes — there's no server-side session persistence — so `DockerExec` can't preserve Phase 2's detach/reattach invariant without a custom in-container agent, out of scope for v1. Secondary reason: the "scope view triggers new pane" pattern generalizes to Phase 5 (SSH Fleet → remote pty) and Phase 6 (remote Docker → exec). Designing the mechanism for DockerExec in isolation would lock in a shape that may not fit. Escape hatch: users retain `docker exec -it <container> sh` in the local pty tile.

### What's in flight with the engineer
- **Phase 4 proposal pass.** Ports + Processes panels. I directed 5 specific questions in proposal-pass format (same discipline as Slice C1.5). Engineer is preparing the proposal; no code yet.
- The five questions, in priority order: (1) where does "Processes" live in the tiled layout — drilldown inside Ports tile, toggle-mode within Ports tile, or daemon-only (no v1 UI)? Decision #7 doesn't reserve a Processes tile; a new tile would require amending Decision #7; (2) data source + platform matrix — `/proc/net/*` vs `procfs` vs `socket2` on Linux; `lsof` vs `libproc` on macOS; (3) port → process → container correlation daemon-side or client-side; (4) refresh cadence; (5) sub-slicing.

### What I'm expecting next
- **Engineer's Phase 4 proposal ping** (no code, answers to the five questions). I review, sign off or push back, escalate to user if anything requires Decision #7 amendment.
- **Then Phase 4 sub-slices** per the engineer's proposed slicing (rough sketch in my direction: 4a daemon-side Ports probe + `Subscribe(Ports)`, 4b Ports tile renderer + subscribe-on-startup, 4c port→container correlation, 4d Processes per Q1's answer, 4e end-to-end test + manual demo).
- **User manual demo** gates Phase 4 close, same shape as C1.5c and C3.

### Open questions I'm holding (not yet in DECISIONS.md)

- **Processes tile placement.** Decision #7 locks five tiles (PTY, Docker, Ports, Fleet, Claude Code) — no Processes tile. Phase 4's name includes Processes. Engineer's proposal must pick a placement approach; if it's "add a 6th tile," that's a Decision #7 amendment requiring user sign-off.
- **Phase 5 (SSH transport + remote pty)** is the next phase after Phase 4. It's also the forcing function for the "scope → new pane" mechanism that Slice D deferred on. Design pass for that mechanism likely lands at Phase 5's proposal pass rather than before.
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

**Last updated:** 2026-04-14, Phase 4 Slice 4c landed (Ports tile TUI with Processes toggle).

### Where I left off

Phase 4 Slice 4c shipped. Awaiting CTO review + cross-OS CI green. 207 tests on macOS / 216 on ubuntu-latest, `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` clean. All four CTO-flagged 4c notes landed — details in `docs/ROADMAP.md` Slice 4c section.

4c covers:
- New `ScopeKind::Ports`; Ports tile in the god view flipped from `Placeholder` to `Scope(ScopeKind::Ports)`. Render dispatch in `session.rs` gains a `Scope(Ports)` arm routing to `scope::ports::render`.
- `PortsScope` state struct in `app.rs` wraps two coequal views (`PortsView` + `ProcessesView`), each with its own three-state lifecycle (`Connecting` / `Available { rows, source }` / `Unavailable { reason }`), filter, and selection. `active: PortsActiveView` toggles which view renders; both subscriptions stay live regardless.
- `PortKey { protocol, local_port, pid }` and `ProcessKey { pid, start_time_unix_secs }` are the stable identities. `reanchor_selection(old_key)` on every state change places the cursor on the matching key when present; clamps into the new visible range otherwise. Pid-reuse with different `start_time` doesn't silently retarget selection (state-machine test pins it).
- Input routing: `handle_forward_bytes` is now a two-arm dispatch by `routes_to_scope(focused)`. `handle_ports_key` absorbs `p` as the outer toggle (unless filter is active, where `p` is a filter character), then delegates to `handle_ports_list_key` or `handle_processes_list_key` by `active`.
- Renderer in `scope::ports::render` mirrors `scope::docker::render`'s shape. Processes table renders `cpu_percent: None` as em-dash (`—`), `Some(x)` as `x` formatted to one decimal. Ports status bar carries the `UDP coming v1.1` footer hint per the 4c UDP-resolution (option c: defer to v1.1 with explicit user-visible indication).
- Tests: 13 new app state-machine + 14 new scope::ports render = 27 total 4c additions on top of 180 from 4b.

No deviations from the CTO's 4c sign-off beyond the optional 5th (cmdline-truncation visual hint) — skipped because it would require either a wire flag (protocol bump for a cosmetic hint) or a heuristic (false-positive prone). `docs/OPERATIONS.md` Common Issues already documents the macOS-truncation behavior so users have an answer.

### What I'm mid-flight on

_Nothing._ Awaiting CTO review of 4c before starting 4d. Don't start 4d code until sign-off.

### What I'm expecting from the CTO next

- **Review + sign-off on 4c**, or redirect. Specific things I flagged as tactical calls: (a) UDP resolution (c) with a footer hint — CTO said "pick one and I'll sign off in review," so this is the reviewable call. (b) Skipping the optional 5th cmdline-truncation hint — defensible on wire-cost grounds, but CTO may prefer the hint even at protocol-bump cost. (c) The two-arm `handle_forward_bytes` dispatch is a small refactor of the existing `Some(ScopeKind::Docker)` arm; it scales cleanly to Phase 5+ but could grow unwieldy if we end up with 6+ scope kinds — worth revisiting shape if scope growth accelerates.
- **Go-ahead for 4d** (Phase 4 e2e + manual demo script). 4d is the Phase 4 close artifact: 6-scenario pass/fail matrix in `docs/OPERATIONS.md`, end-to-end integration test driving a scripted `App` through the wire. Acceptance scenarios are already staked out in `docs/ROADMAP.md` Slice 4d section.
- CI green on both OSes is my own gate; I'll check `gh run` after pushing.

### Anything that would surprise a fresh-me

**Load-bearing: `ProcessesProbe::sample(&mut self)` is stateful by design.** (Elevated from the 4b surprises list per CTO directive — mis-refactoring this silently kills CPU% in production, and tests would still pass.) sysinfo computes CPU% as a delta between consecutive `refresh()` calls, so the probe must persist across sampling cycles. The daemon's `forward_processes` task moves the probe into `spawn_blocking` each iteration and receives it back via the closure return tuple, keeping sysinfo's internal process map + CPU baseline alive while the tokio runtime stays unblocked. On `JoinError` (spawn_blocking panic), the task resets to a fresh probe — the next emit correctly carries `cpu_percent: None` for every row because we can't compute a delta across a crash boundary. **Do not refactor `sample` into a stateless free function or a `fn sample(&self)` method; tests would still pass but every sample would carry `None` and CPU% would be silently dead in production.**

4c-era items:

- **Selection persistence uses stable keys, not positional indices.** `PortKey { protocol, local_port, pid }` and `ProcessKey { pid, start_time_unix_secs }` are computed from the currently-selected row BEFORE a state transition, then used to re-anchor the cursor AFTER the transition via `reanchor_selection(old_key)`. Three invariants (all pinned by state-machine tests): (1) rows reorder → cursor follows the key to its new index; (2) selected entity disappears → cursor clamps into the new visible range (doesn't crash, doesn't stick on a removed row); (3) pid reuse under a different `start_time` never silently retargets. If you add filter-change handling, filter-narrowing, or similar state shifts, use `reanchor_selection` — don't hand-clamp.
- **`p` toggle is absorbed at the outer scope BEFORE filter/nav dispatch.** `handle_ports_key` checks `ScopeKey::Char(b'p')` at the top and only flips `active` if the current view's `filter_active` is false. While filter-typing, `p` falls through as a normal filter character. Without this carve-out, typing `postgres` into the filter would toggle views mid-word. Tested by `p_does_not_toggle_while_filter_is_active_on_ports_view`.
- **Render-layer em-dash is `"   —"`, not `"-"` or `"n/a"`.** The `cpu_text` match in `render_processes_table` emits a 4-char field starting with 3 spaces + the unicode em-dash (U+2014). Width matches the right-aligned `Some(x)` format (`"{:>5.1}"`). If the table columns ever change width, update both arms in lockstep — rendering tests pin em-dash presence (`rows.contains("—")`) but not exact column alignment.
- **`UDP coming v1.1` footer hint is an explicit-defer cue.** The 4c UDP resolution was option (c): TCP-only, user-visible indication. If you later ship UDP: remove the hint, flip the probe's `ProtocolFlags::TCP` to `ProtocolFlags::TCP | ProtocolFlags::UDP`, add a `protocol` column to the table (or keep it in the row header — it's already the first column), and add a render test that both TCP and UDP rows appear distinguishably.
- **Ports tile dispatch is a two-arm match, not a chain of `Some(X) = ...`.** In `handle_forward_bytes`, I converted the previous `if let Some(ScopeKind::Docker) = ...` to a `match` on `Some(Docker)` / `Some(Ports)` / `None`. This scales linearly — Phase 5 (`ScopeKind::Fleet`) adds one more arm. If scope count ever hits 6+, consider a method on `ScopeKind` that returns a `&dyn ScopeHandler`, but don't pre-optimize for now.

4b-era items:

- **`ProcessesProbe` is stateful; `list_ports()` is stateless.** Keep this as orientation even though the load-bearing warning above supersedes it. `list_ports()` is `fn() -> Result<Vec<ProbePort>>` (stateless). `ProcessesProbe::sample(&mut self) -> Result<Vec<ProbeProcess>>` (stateful). If you add a third probe kind, ask: is there cross-sample state (deltas, rate averages, cached sockets) → struct; otherwise → function.
- **`cpu_percent: Option<f32>` on the wire carries semantic meaning `f32` alone can't.** `None` = "first sample, sysinfo had no prior delta, TUI renders em-dash". `Some(0.0)` = "measured as idle". `Some(x)` = "measured as x%". Docker stats uses `f32` with `0.0` as sentinel for the same situation; Processes made a different call per CTO's explicit 4b note. Roundtrip test `process_list_event_roundtrip_preserves_first_sample_cpu_none` pins that `None` doesn't collapse to `Some(0.0)` through rkyv.
- **`forward_processes` uses move-into-spawn-blocking + return-back-via-tuple** to persist `ProcessesProbe` state across iterations while still running the sync refresh on tokio's blocking pool. On `JoinError` (probe task panicked), the task resets to a fresh probe; this means the NEXT emit after a panic will again carry `cpu_percent: None` for every row — intentional (can't compute a delta across a crash boundary), but worth knowing when debugging "why did CPU% disappear?" events.
- **macOS cmdline resolution is degraded.** sysinfo's `Process::cmd()` on macOS sometimes returns only `["sleep"]` when the real cmdline was `sleep 30`. The opt-in integration test asserts `command.contains("sleep")` not exact match for this reason. If 4c renders cmdlines and users complain about "truncated args," check sysinfo's macOS libproc backing and whether reads require different privileges.
- **`ChildGuard` pattern for test-spawned children.** If you spawn a child process in an integration test, wrap it in a `struct ChildGuard(std::process::Child)` with a Drop impl that calls `kill()` + `wait()`. Without it, a panic mid-test leaks the child into the test runner's parent shell. Use this pattern in every probe-ish test going forward (Phase 5 SSH tests, Phase 7 scanner tests).
- **sysinfo 0.31's `refresh_processes_specifics` takes 2 args, not 3.** Older sysinfo tutorials show `(processes_to_update, remove_dead, refresh_kind)` but 0.31 dropped the middle bool. First compile attempt will surface this — don't spend time on the error, just drop the bool.

4a-era items:

- **Linux correlates port → container in the probe (via cgroup); macOS correlates in the daemon (via bollard port match).** Not a mistake or asymmetry — macOS pids can't carry a cgroup reference (Docker Desktop runs containers inside a Linux VM, so macOS-visible pids are VM host pids, not in-container pids). The only workable correlation on macOS is `local_port` → `HostConfig.PortBindings`. Linux has both options; we picked cgroup because it needs no Docker engine connection. If you change this, flag it — the two-layer split is easy to miss.
- **`forward_ports` skips Docker entirely on Linux.** The whole correlation block is `#[cfg(target_os = "macos")]` inside `forward_ports`. This is an optimization: on Linux with non-containerized processes, `container_id == None` is the correct final answer (no container to correlate to), and without the cfg guard the daemon would do a pointless `Engine::connect` on every poll. If you ever need Linux-side Docker correlation for a different reason, remove the cfg guard BUT then avoid triggering it for non-containerized processes.
- **`netstat2` is the listening-socket backend, not raw netlink.** Deviates from the Phase 4 proposal. The Cargo.toml comment explains why; the wire contract didn't change so an upgrade to `NETLINK_SOCK_DIAG` can happen later without touching the daemon or clients.
- **`tokio::task::spawn_blocking` wraps every `list_ports` call.** The probe does filesystem reads + libc calls on macOS and is not async. Calling it directly from the tokio runtime would block whatever reactor thread the `forward_ports` task landed on. Mirror this pattern in 4b's processes probe — `sysinfo` has the same blocking shape.
- **`partial: bool` on ProbePort is the graceful-degradation signal.** When the probe sees a socket but can't attribute it (pid unknown, permission denied, process vanished mid-read), the row comes through with `pid == 0`, empty `process_name`, and `partial: true`. The TUI in 4c renders these with a visual cue (e.g., dimmed or with a `?` prefix) so users know to elevate. Non-partial rows have complete data; don't emit partial rows from newly-added fields without checking this invariant.
- **Pid reuse across a 2 s refresh is a Phase 4 risk.** Short-lived processes can get a pid that's later reused for another binary. For Ports this is rare (listening ports imply long-lived processes). For Processes (4b) it matters — CTO's selection-persistence note specifically called for `(pid, process_start_time)` tuples. sysinfo exposes `Process::start_time()`; use it for selection persistence, not just pid.

Phase-3-era items still relevant:

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
