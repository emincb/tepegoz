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

**Last updated:** 2026-04-14, post-Phase-4-close. Phase 5 proposal pass is the next gate. **This HANDOFF was updated immediately before a CTO context clear — fresh CTO is reading cold.**

### What I just signed off on
- **Phase 4 closed.** User signed off on the 8-scenario manual demo (with navigation-discoverability clarification noted — `h/j/k/l` horizontal+vertical, not just vertical). Ports + Processes scopes ship end-to-end: cross-OS probe abstraction (netstat2 + sysinfo + procfs/libproc), daemon-side port→process→container correlation, Ports tile with Processes toggle. 220 tests on ubuntu / 211 on macOS.
- **Wire-desync root cause + fix** (`77aa9ca`). `tokio::AsyncReadExt::read_exact` is NOT cancellation-safe. The TUI's main-loop `select!` was calling `read_envelope` directly alongside stdin/winch/tick branches; when another branch fired mid-`read_exact`, kernel socket position advanced but userspace bytes were lost → next read desynced. Fix: dedicated `spawn_reader_task` mirroring the daemon-side writer pattern; main loop reads from `mpsc::UnboundedReceiver<Result<Envelope, anyhow::Error>>` (cancellation-safe). Regression test in default suite (`stdin_pressure_does_not_desync_large_envelopes`). Engineer verified both CI-green AND the original production trigger (`yes | tui`).
- **Phase 3 closed** (`8984456`). Docker scope panel end-to-end.
- **Slice D (`DockerExec`) deferred to v1.1** per user sign-off. Decisive reason: Docker's exec API ends the exec session when the hijacked connection closes — can't preserve Phase 2's detach/reattach invariant without a custom in-container agent. Secondary: the "scope → new pane" pattern generalizes to Phases 5/6 and should be designed there, not for DockerExec alone.

### What's in flight with the engineer

Three things queued, in strict order:

1. **Phase 4 close commit.** `docs/STATUS.md` row 4 → ✅, ROADMAP Phase 4 → ✅ (2026-04-14), HANDOFF updates, + the "Help overlay for focus navigation keybinds" polish candidate in STATUS.md (user hit horizontal-vs-vertical discoverability during demo scenario 3). Close-commit text in the relay message before this clear. Engineer lands next.
2. **Diagnostic tracing cleanup commit.** Strip high-cardinality envelope-write debug logs from `bee6aba`. Keep: `payload_variant` helper; read-side hex-dump on length-prefix-bail; the `debug_assert_eq!(bytes.len(), ...)` assertion that falsified the AlignedVec hypothesis. Separate commit, after Phase 4 close.
3. **Phase 5 proposal pass.** Seven questions issued in the relay before this clear. No code until proposal is signed off. Phase 5 is the most architecturally novel phase to date.

### What I'm expecting next

- Engineer's ping on Phase 4 close commit (trivial, doc-only).
- Engineer's ping on diagnostic tracing cleanup (trivial, scoped strip).
- **Engineer's Phase 5 proposal ping — the substantive one.** Seven questions: (Q1) crate structure `tepegoz-ssh` vs. `tepegoz-transport` trait-now-or-later; (Q2) host discovery / `~/.ssh/config` / first-run UX; (Q3) remote pty lifecycle daemon-side-tunneled vs. remote-side-via-agent; (Q4) auth model SSH agent vs. explicit keys vs. layered like Decision #2; (Q5) **the big call — "scope → new pane" mechanism** (candidate a: replace-primary + stashed-original; b: tab-strip within pty tile; c: amend Decision #7 for new tile kind); (Q6) connection lifecycle + error UX; (Q7) sub-slicing proposal.
- On Q5: if engineer proposes (c) amending Decision #7, escalate to user sign-off BEFORE accepting. That's a locked-decision amendment.
- Phase 5 sub-slices per engineer's proposed slicing. User manual demo gates close, same shape as Phases 3 and 4.

### Open questions I'm holding (not yet in DECISIONS.md)

- **Phase 5 "scope → new pane" is the load-bearing call.** It locks the pattern for Phase 6 (remote Docker → exec into remote container) AND for v1.1 Slice D (DockerExec) when it reopens. Get it right once. My prior: option (a) replace-primary-with-stashed-original is cleanest — matches tmux's mental model, doesn't add chrome, doesn't amend Decision #7. But I'm not committing that view; engineer's proposal should earn the call.
- **Phase 4 polish candidates** tracked in `docs/STATUS.md`:
  1. Help overlay for focus nav (`Ctrl-b ?` already reserved, rendered no-op since C1.5b). User hit discoverability issue; worth landing as an early v1.1 polish item or in any Phase 5-adjacent TUI work.
  2. Phase 3 carryovers: bounded `tail_lines` default (1000 with "load more"); logs-tile zoom if cramped; color palette feedback revisit.
- **OSC 0 title refresh on focus change** still stubbed (`AppAction::FocusTile(TileId)` → debug log). More useful now that 5 tiles exist and focus ambiguity is more common.
- **Phase 3's `tail_lines: 0 = all history` wire semantic** remains Slice B's contract; changing it is a compatible additive (daemon honors any value; client can send `1000` instead of `0`).

### Observations about engineer patterns (load-bearing for future direction)

- **Diagnostic discipline operates at its best when a bug is hard to narrow down.** Reference execution: Phase 4 wire-desync at `77aa9ca`. Shipped tracing in isolated commit (`bee6aba`), reproduced with targeted trigger (`yes | tui`), *falsified* a plausible hypothesis with an assertion (`debug_assert_eq` ruled out `AlignedVec` padding), identified actual root cause from byte-level log evidence, proposed minimal well-reasoned fix with symmetry argument, wrote regression test in default suite. This is the reference model.
- **Verifies fixes against the original production trigger, not just CI green.** Phase 4 fix: ran `yes | tui` for 20s alarm window on the fixed binary before declaring done. "Tests pass" and "bug fixed" are different claims; engineer checks both.
- **Catches real daemon bugs while trying to build tests.** C2 gate (`43b28eb`): discovered `pane_subs` leak because they refused to ship a vim-preservation test that didn't exercise the real `Unsubscribe` path.
- **Strong commit hygiene.** Messages capture *why* and blast-radius. Reference models: `43b28eb`, `c7b336d`, `4dd1208`, `77aa9ca`.
- **Good at salvage logic during pivots.** C1.5 tiling correction: explicitly enumerated what survives from C1/C2 and what gets deleted; updated docs in the same commit as the pivot.
- **Cross-OS CI discipline without prompting.** Caught the `printf \x1b` vs. `\033` POSIX gotcha via CI, not local-only testing. Platform divergence catches ship on every push.
- **Volunteers judgment calls at the right level.** Flags 3-5 tactical decisions per slice for review, doesn't flag naming noise. Matches `feedback_implementation_autonomy` model.
- **Adopts defensive testing patterns unprompted.** Examples: `push_toast_at(now, ...)` for time-travel tests; 2s sleep for status-counter-advance in restart round-trip; SIGSTOP-dockerd for timeout demo; python3 child + stdout-handshake for Phase 4 e2e (eliminates port-collision + bind-race flakes); `kill_on_drop(true)` instead of hand-rolled `ChildGuard`.
- **One watch-item:** occasionally needs the generalization-to-future-phases prompt. Examples where engineer's reasoning covered it (Slice D defer, desync fix class-of-bug), examples where I added it (the netstat2 deviation's Phase 7 implications). When signing off on deviations, explicitly ask "what does this lock for Phase N+1?"

### Standing context (if you're the fresh CTO reading cold)

- **Your role:** CTO / planner / architect, promoted by user "Emin" on 2026-04-13. You don't write code; the engineer does. Your job: proposal review, architectural sign-off, ordering of work, flagging product-level drift. You can edit `docs/`, `README.md`, and memory files — that's not "code." You cannot touch `crates/` or anything Rust.
- **Relay pattern:** user mediates between your session and the engineer's session. Engineer doesn't see your internal reasoning, only directives the user relays. Write engineer-facing messages as self-contained, unambiguous, ordered — they pick up cold from the relay. When user says "give me the full message and only the message," lead with the relay text marked for verbatim paste.
- **Spec hierarchy:** `README.md` + mockup first, `docs/DECISIONS.md` second, `docs/STATUS.md` / `docs/ROADMAP.md` / `docs/ARCHITECTURE.md` / `docs/OPERATIONS.md` / `docs/ISSUES.md` third. Check README before signing off on UX proposals (memory: `feedback_cross_check_vision_before_signoff.md`). The mode-switch-drift in Slice C happened because a previous-me skipped this check.
- **Seven locked architectural decisions** in `docs/DECISIONS.md`. Changing any requires user sign-off. #7 (tiled god view, opinionated default, vt100 via `vt100` crate) was added 2026-04-14 after the Slice C mode-switch drift.
- **Session-start ritual** in `CLAUDE.md`: read it + STATUS + ISSUES + HANDOFF, verify git log matches claims, fix stale entries before acting. Don't act on a memory or handoff entry if reality diverges — trust reality, update the docs.
- **Working memory:** `~/.claude/projects/-Users-emin-Documents-projects-personal/memory/`. `MEMORY.md` is the index. Current entries: user profile, sharpen-before-AI, diagnose-before-fixing, demonstrable-acceptance, implementation-autonomy, durable-project-state, session-boundary-handoff, cross-check-vision-before-signoff, project_tepegoz.
- **Phases shipped:** 0 (scaffold), 1 (proto + daemon + TUI), 2 (pty multiplex), 3 (Docker scope), 4 (Ports + Processes). Current: Phase 5 proposal pass pending.
- **Phases deferred:** Slice D (DockerExec) deferred to v1.1 — reopens when Phase 5 crystallizes the "scope → new pane" mechanism.
- **If the current state of the tree doesn't match this HANDOFF:** trust the tree, update HANDOFF. `git log --oneline -10` + `cat docs/STATUS.md` is the authoritative answer to "where are we?"

---

## Engineer section

**Last updated:** 2026-04-14, post-Phase-4-close. Phase 5 proposal pass is the next gate.

### Where I left off

**Phase 4 is closed.** User signed off on all 8 manual-demo scenarios in `docs/OPERATIONS.md` "Slice 4d manual demo prep" against the desync-fix binary. The close commit flipped STATUS row 4 → ✅ (2026-04-14) with the full commit list (4a + 4b + 4c + 4d feat + `bee6aba` diag + `77aa9ca` fix + this close commit), flipped ROADMAP Phase 4 overall marker + Slice 4d header to ✅, refreshed both HANDOFF sections for post-close state, and added a STATUS "Phase 4 polish candidates" section capturing the one item the user flagged during the demo: the reserved `Ctrl-b ?` help overlay needs wiring up to expose the full focus-nav + per-tile keybind set — explicitly noting that `h/j/k/l` moves both horizontally AND vertically, since "just vertically" was the discoverability miss during demo scenario 3.

**Wire desync class-of-bug** (`77aa9ca`). `tokio::AsyncReadExt::read_exact` is NOT cancellation-safe. Main-loop `select!` was calling `read_envelope` directly alongside stdin / winch / tick — any competing branch firing mid-`read_exact` silently corrupted the reader. Fix: `spawn_reader_task` mirroring the daemon's writer pattern, funneling envelopes through a cancellation-safe mpsc. Regression pinned in the default suite (`session::tests::stdin_pressure_does_not_desync_large_envelopes`). Production trigger (`yes | tui`) verified clean over a 20 s alarm window on the fixed binary — "tests pass" and "bug fixed" confirmed separately.

### What I'm mid-flight on

_Nothing committed-side._ One commit queued:

- **Diagnostic tracing cleanup.** Strip the high-cardinality envelope-write debug logs that `bee6aba` added for the desync investigation. Keep: `payload_variant` string helper in `tepegoz-proto::codec` (useful for any future wire debugging); read-side next-32-bytes hex dump on the length-prefix-bail path (makes any future desync diagnosis 10× faster); `debug_assert_eq!(slice.len(), bytes.len())` assertion that falsified the AlignedVec hypothesis (cheap dev-build invariant check). Strip: `first_four_hex` in `write_envelope` + per-envelope-write debug log + running `bytes_written_total` + per-envelope-seq counter in `tepegoz-core::client::spawn_writer_task`. Separate commit, no bundling.

### What I'm expecting from the CTO next

- **Phase 5 proposal-pass direction.** The CTO's 7 questions, in order: (Q1) crate structure — `tepegoz-ssh` as the first concrete impl vs. `tepegoz-transport` trait-now-or-later; (Q2) host discovery + first-run UX — `~/.ssh/config` parsing, explicit host list, or both; (Q3) remote pty lifecycle — daemon-side-tunneled through SSH or remote-side-via-agent (forecasts Phase 6's agent design); (Q4) auth model — SSH agent socket inheritance vs. explicit key paths vs. keychain vs. layered like Decision #2's root-key precedence; (Q5) **the big call — "scope → new pane" mechanism** under Decision #7's fixed layout (three candidates carried forward from Slice D: a) replace primary PTY tile + stash original; b) tab-strip within pty tile; c) amend Decision #7 for new tile kind); (Q6) connection lifecycle + error UX; (Q7) sub-slicing proposal. No code until the proposal is signed off.
- **Phase 5 inherits to Phase 6 + v1.1 Slice D.** Q5 locks the "scope → new pane" pattern for all three futures (remote pty, remote-Docker exec, DockerExec reopen); Q3's pty-lifecycle answer forecasts Phase 6's agent design. Proposal will call out both forward-compat arguments explicitly.
- **If Q5's answer requires amending Decision #7** (option c — new tile kind): CTO escalates to user before any code starts. I'll flag this prominently in the proposal if that's where the reasoning lands.
- **Diagnostic tracing cleanup commit** immediately follows the Phase 4 close commit on this branch; Phase 5 proposal pass starts after the cleanup. CI green on both OSes is my own gate; I check `gh run` after pushing.

### Anything that would surprise a fresh-me

**Load-bearing: `tokio::AsyncReadExt::read_exact` is not cancellation-safe.** Using it directly in a `tokio::select!` arm where other branches can fire will silently corrupt the reader stream when `read_exact` is interrupted mid-read. The pattern for the TUI is: dedicate a reader task, funnel envelopes through an mpsc, `select!` on `mpsc::Receiver::recv()` (which IS cancellation-safe). Same discipline as the daemon's dedicated writer task. If you're adding a new `select!` that reads from a stream, think cancellation safety FIRST. Classic failure mode: "works in tests, breaks under heavy stdin or winch pressure" — exactly how Phase 4's manual demo failed. Pinned by `session::tests::stdin_pressure_does_not_desync_large_envelopes`. Burned 4+ hours diagnosing Phase 4 4d; don't repeat the lesson.

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
