# Locked Architectural Decisions

These are binding. Phase work that conflicts with any of these is a bug. Revisit only if the constraint that justified the call is gone.

## 1. Wire protocol: rkyv zero-copy, versioned envelope

**Decision.** `rkyv` 0.8 with bytecheck validation on all network boundaries (agent channel, remote clients). The local Unix socket between daemon and TUI may skip validation for perf if profiling demands — pattern via `access()` everywhere for now.

**Envelope.** `{ version: u32, payload: Payload }` at the root. Generated migration handlers wrap cross-version reads.

**v2 web clients.** Browser decoders for rkyv are a tarpit — plan a JSON/CBOR edge at the daemon's WSS listener rather than exposing raw rkyv outside Rust.

## 2. At-rest key source: keychain + env/file override, day one

**Decision.** Three sources, one precedence: `env > file > keychain`.
- `TEPEGOZ_ROOT_KEY=<hex>` — inline
- `TEPEGOZ_ROOT_KEY_FILE=/path` — k8s/docker secrets, systemd `LoadCredential`
- OS keychain via `keyring` — default

When any override is set, the daemon does **not** write back to the keychain. Headless boxes without a desktop keyring must work out of the box; requiring manual keychain unlock is a v1 defect.

## 3. Agent embedding + full CI, day one

**Decision.** All four agent arches (linux x86_64/aarch64 musl, mac x86_64/arm64) are embedded in the controller binary via `include_bytes!`. Daemon detects remote arch and `scp`s the correct one; both sides verify SHA256.

**Two-stage build.** `cargo xtask build-agents` produces `target/agents/*`. Controller's build.rs reads that dir and embeds. Dev builds use a feature flag for stub agents so single-target rebuilds stay fast.

**Releases.** Minisign signatures on all artifacts, not just SHA256. Checksums catch corruption; signatures catch tampering. Cheap to add at release time, expensive to retrofit trust.

## 4. Port scanning: TCP-connect v1, SYN v1.1 (Linux-first)

**Decision.** v1 ships TCP-connect only. SYN scanning is v1.1, Linux first — do not gate Linux SYN on macOS parity. BPF on macOS is an integration risk (buffer semantics, SIP restrictions) that deserves its own dedicated work.

**TCP-connect implementation.** `socket2` for portable option control (SO_LINGER zero-timeout + RST close to avoid TIME_WAIT backlog on localhost sweeps). Bounded concurrent fanout via tokio semaphore — default ~500, tunable.

## 5. Hot-reload: tier 1 + 2 in v1, tier 3 restart-required

**Decision.** Live reload for UI/behavior settings only. Transport, storage, and key-source changes are documented as restart-required in v1.

- **Tier 1 (live):** keybindings, theme, scrollback size, log levels, default shell, refresh rates — backed by `Arc<ArcSwap<Config>>`
- **Tier 2 (live):** filter defaults, panel settings — event dispatch to TUI clients on change
- **Tier 3 (restart):** socket path, listen addr, root key source, redb path — requires SCM_RIGHTS pty handoff (multi-week engineering) to hot-swap safely

Full fork-exec graceful restart with pty inheritance is revisited in v2 if real pain emerges. Not in v1.

## 6. Claude Code state parsing: structural signature, graceful degrade

**Decision.** On parser boot, compute a structural signature from `~/.claude/projects/` (set of directory names + top-level JSON fields present) — **not** a content hash. Strict content hashes break on benign field additions; structural signatures tolerate them.

**On unknown signature:** yellow status notice in TUI ("Claude Code session awareness unavailable — detected signature 0xABC not recognized"). **Never** crash the daemon. The feature disables; everything else keeps working.

**Diagnostic:** `tepegoz doctor --claude-layout` dumps detected signature + known signatures for bug reports.

## 7. UI substrate: tiled god view, opinionated default, vt100 via `vt100` crate

**Decision.** The v1 TUI is a fixed tiled layout. All scopes are visible
simultaneously; there is no mode switching. Focus moves between tiles;
content in unfocused tiles continues to update live.

**Default layout (non-configurable in v1).** Rendered on first run of
`tepegoz tui` with no setup:

- PTY tile: top row, full width
- Docker tile: bottom-left
- Ports tile: bottom-middle
- Fleet tile: bottom-right
- Claude Code tile: bottom-full-width strip under the scope row

Scopes not yet implemented render a labeled placeholder tile ("Ports —
Phase 4", etc.) that the user can see but not interact with. As each
phase lands, the placeholder is replaced by the live tile with no layout
change.

**vt100 emulation.** The pty tile is rendered via the `vt100` crate: pty
bytes feed the parser; the parser's screen buffer renders as a ratatui
widget within the tile's Rect. Raw passthrough is gone. The Slice
5d-ii tab strip (1 row of clickable pane labels) sits above the vt100
render area inside the same Rect; this is interior tile layout, not a
Decision #7 amendment.

**Input / interaction (amended 2026-04-15, Slice 6.0).** Focus moves
between tiles via mouse click OR keyboard `Tab` / `Shift-Tab`.
**Tab always cycles tiles in a fixed order** (PTY → Docker → Ports
→ Fleet → ClaudeCode → PTY); it never cycles within a tile's
contents regardless of which tile is focused. Within a focused tile,
keyboard navigation is tile-specific (`j` / `k` / arrow keys for row
nav, `/` to filter, etc.). Mouse click on an interactive element
(tile, row, pane-strip tab) selects or acts per the tile's contract
— every visually-interactive element must respond to click, and
hover states (border highlight, color shift, pointer cursor on
supporting terminals via OSC 22) indicate click-ability.

Documented keyboard surface — the five bindings help and docs teach:
`Tab` / `Shift-Tab` (tile focus), arrow keys / `j` / `k` (row nav),
`Enter` (primary action on selected row), `Esc` (cancel / back),
`Ctrl-b d` (detach).

Kept as undocumented power-user aliases (muscle-memory continuity,
not taught to new users): `Ctrl-b h` / `j` / `k` / `l` for
directional tile focus (faster than Tab when crossing the 5-tile
spatial layout), `Ctrl-b &` for close-active-pane.

Removed entirely (obsoleted by clickable tab strip + the unified
Enter/Esc keybinds): `Ctrl-b 1..9`, `Ctrl-b n`, `Ctrl-b p`,
`Ctrl-b w`, `Ctrl-b q`.

**Mouse bus.** `AppEvent::MouseClick { x, y }` and
`AppEvent::MouseHover { x, y }` flow through the existing AppEvent
bus. Each tile's renderer owns hit-testing within its `Rect` —
converts coordinates into the appropriate row / tab / element and
emits the matching `AppAction`. Crossterm mouse capture is enabled
in `terminal::enter_raw` and disabled on detach.

**Configuration.** Zero. The user does not choose a layout, does not
enable tiles, does not opt in. `tepegoz tui` → god view. User
configurability of the layout is explicitly deferred; revisit in v2.

**What this supersedes.** The C1 `View::{Pane, Scope}` mode enum and the
`switch_to_scope`/`switch_to_pane` synthetic-re-attach pattern are
removed as part of C1.5. The `AppEvent`/`AppAction` bus is retained;
`View` is redefined as `{ layout: TileLayout, focused: TileId }`.

---

## Durable working rules

- Any v1 code that couples TUI rendering to daemon internals without going through the wire protocol is a bug.
- No cloud, no phone-home, no auto-update, no outbound network outside user-authorized SSH.
- Sharpen the blade first — no AI features in v1 under any circumstances.
