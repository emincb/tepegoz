# Tepegöz v1 roadmap

Target: **0.1.0 release** at end of Phase 10. Rough budget: 15–20 weeks full-time.

Status key: ✅ complete · 🟡 code+tests green, user acceptance pending · 🟠 in progress · ⚪ not started.

Per-phase: goal, delivered (or scope), acceptance test, explicit non-goals, risks.

---

## Phase 0 — Scaffold · ✅ · `81c7731`

**Goal.** A buildable, linted, CI-green Cargo workspace with stubbed subcommands.

**Delivered.**
- Workspace `Cargo.toml` with 12 crate members + `xtask`.
- `rust-toolchain.toml` pinned `stable`; `mise.toml` pinned to `1.94.1`.
- `.cargo/config.toml` with `cargo xtask` alias.
- `.github/workflows/ci.yml`: fmt, clippy `-D warnings`, native tests on ubuntu-latest + macos-latest, cross-build matrix (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin`) via `cargo-zigbuild`.
- `tepegoz` binary with clap-derived CLI stubs for `daemon`, `tui`, `connect`, `agent`, `doctor`.
- `tracing_subscriber` wired with `RUST_LOG` + `--log-level` fallback.

**Acceptance.** Local `cargo build && cargo test && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check` all pass; CI green on both OSes.

**Not in scope.** Any runtime behavior beyond `--help` text.

---

## Phase 1 — Proto + daemon + TUI round-trip · ✅ · `3715bf9`

**Goal.** Prove the daemon-as-source-of-truth pattern. Daemon holds state; TUI is a passive viewer; state survives client disconnect.

**Delivered.**
- `tepegoz-proto`: rkyv 0.8 `Envelope { version, payload }` with 4-byte big-endian length prefix and bytecheck validation on every read.
- Messages (v1): `Hello`, `Welcome`, `Ping`, `Pong`, `Subscribe(Status)`, `Unsubscribe`, `Event(EventFrame)`, `Error(ErrorInfo)`.
- `tepegoz-proto::socket::default_socket_path()` → `$XDG_RUNTIME_DIR/tepegoz-<uid>/daemon.sock` (fallback `$TMPDIR`, then `/tmp`).
- `tepegoz-core`: Unix socket listener (0700 parent when default, 0600 socket); stale-socket eviction; graceful SIGINT shutdown; `StatusSnapshot` streamed at 1 Hz to subscribers.
- Original `tepegoz-tui`: ratatui status panel (daemon pid, uptime, client counts).
- Atomic counters in `SharedState` — no lock contention on sampling.
- Single writer task per client draining an mpsc — outbound frames serialized without per-write locks.

**Acceptance test.** `crates/tepegoz-core/tests/daemon_persistence.rs` — client #1 connects, snapshots `clients_total=1`, disconnects. Client #2 reconnects, snapshots `clients_total=2`; `uptime_seconds` monotonic; `daemon_pid` identical — proving state survived.

**Not in scope.** PTYs. Scope panels. Authentication. Any subscription kind other than `Status`.

---

## Phase 2 — Local pty multiplex + persistence · 🟡

**Status.** Code + tests green at `f12d194`; one user-visible bug under diagnosis (immediate detach on attach) — see `docs/ISSUES.md#active`.

**Relevant commits.** `eab274c` (scrollback/broadcast race fix), `321ed5e` (cwd + `TEPEGOZ_PANE_ID` fixes), `f12d194` (diagnostic tracing).

**Goal.** "Kill the TUI mid-command, reopen, see where I left off, keep going." Daemon owns the shell; TUI is a window.

**Delivered (code).**
- `tepegoz-pty`: `PtyManager` owns `HashMap<PaneId, Arc<Pane>>`. Each `Pane` wraps a portable-pty master, a blocking reader thread (appends to a 2 MiB `VecDeque<Bytes>` ring buffer, broadcasts on a tokio channel), a waiter thread (records exit code, publishes `PaneUpdate::Exit`), and a size cell.
- Wire protocol v2: `OpenPane`, `AttachPane`, `ClosePane`, `ListPanes`, `SendInput`, `ResizePane`; responses `PaneOpened`, `PaneList`; events `PaneSnapshot`, `PaneOutput`, `PaneExit`, `PaneLagged`.
- Daemon client session: per-`AttachPane` forwarder task translates broadcast events into protocol events keyed by subscription id.
- TUI rewrite: raw-passthrough attacher — raw mode + alt screen, stdin → `SendInput`, `PaneOutput` → stdout, `SIGWINCH` → `ResizePane`. Detach via `Ctrl-b d` or `Ctrl-b q` (InputFilter state machine).
- Daemon stamps `TEPEGOZ_PANE_ID=<id>` into pty env; TUI refuses to run if its own env has that var (blocks recursive attach).
- Shell starts in TUI's `current_dir()` (portable-pty otherwise defaults to `$HOME`).

**Acceptance tests.**
- `crates/tepegoz-core/tests/pty_persistence.rs` — client opens pane, sends `echo MARKER\n`, verifies output; drops; second client reconnects, re-attaches, `PaneSnapshot` contains `MARKER` from the ring buffer.
- `tepegoz-pty::tests::subscribe_does_not_duplicate_bytes` — drives 50 markers mid-stream; asserts each appears exactly once across snapshot + live (regression for the scrollback/broadcast race).
- `tepegoz-pty::tests::pane_honors_cwd_and_exposes_pane_id_env` — `pwd` output contains requested cwd; `$TEPEGOZ_PANE_ID` matches pane id.

**Pending to mark ✅.** Resolve the active immediate-detach bug per `docs/ISSUES.md`.

**Not in scope.** Tiled layout. Multi-pane navigation. VT100 emulation for overlay chrome. (MVP is deliberately single-pane raw passthrough; chrome waits until we actually need it.)

**Risks.** Because the TUI is raw passthrough, anything the terminal or the shell does that coincidentally matches the detach prefix (`Ctrl-b d/q`) triggers detach. The active bug is suspected to live here.

---

## Phase 3 — Docker scope panel · ⚪ (blocked by Phase 2)

**Goal.** First scope panel. Lists containers; tails logs; execs into container (opens a new pane); lifecycle actions. Sets the UX template for Ports/Processes/Logs in Phase 4.

**Scope.**
- `tepegoz-docker`: `bollard` wrapper with socket discovery across Docker Desktop (`/var/run/docker.sock`), Colima (`~/.colima/default/docker.sock`), Rancher Desktop, `$DOCKER_HOST` env, native Linux socket. Graceful degradation when docker is unreachable.
- Wire protocol extension: `Subscribe(Docker)`; events `ContainerList`, `ContainerStats`, `ContainerLog`, `ContainerEvent` (from `/events`).
- Commands: `DockerAction(Restart|Stop|Start|Remove|Kill)`, `DockerLogs(container_id, follow)`, `DockerExec(container_id, cmd)`.
- TUI panel: table view (name, image, status, cpu%, mem, ports); keybinds for logs (new subscription), exec (new pty pane), restart/stop/rm, inspect.
- TUI must gain a scope-vs-pty view switch.

**Acceptance.** Integration test against a mocked or real Docker socket: open container, drive List/Log/Action, verify responses.

**Not in scope.** Docker Compose, swarm, multi-host. Cross-container networking visualization (Phase 4+).

**Risks.** Socket discovery across Docker runtimes is the main engineering risk. Graceful degradation when docker is absent is essential — don't let a missing docker break the daemon.

---

## Phase 4 — Ports + processes panels (local) · ⚪

**Goal.** Two more scope panels backed by native per-OS probes.

**Scope.**
- `tepegoz-probe`: `trait Probe` + per-OS implementations:
  - **Linux**: `procfs` for processes; netlink `NETLINK_SOCK_DIAG` for listening sockets (no parsing overhead).
  - **macOS**: `libproc-rs` for processes (`PROC_ALL_PIDS` + `PROC_PIDTASKALLINFO`); `libproc` `PROC_PIDFDSOCKETINFO` per pid for sockets.
  - Cross-OS fallback via `sysinfo` for CPU/mem/disk.
- Wire protocol: `Subscribe(Ports)`, `Subscribe(Processes)`; events for list + deltas.
- TUI panels: tabular views with sort, filter (by port, pid, process name), signal actions on processes.

**Acceptance.** Start a known process and bind a known port; see both in the panels. Kill the process; see it disappear.

**Not in scope.** Remote probes (that's Phase 6 with the agent).

**Risks.** netlink is fast but unfamiliar; libproc is older and less documented. Keep `sysinfo` as fallback.

---

## Phase 5 — SSH transport + remote pty · ⚪

**Goal.** `tepegoz connect user@host` opens a remote pty pane — same UX as local.

**Scope.**
- `tepegoz-ssh`: `russh` client with channel multiplexing. Host key TOFU (trust on first use) with warn on change.
- Wire protocol extension: pty commands/events gain a `host` qualifier (or the daemon tracks per-connection host association).
- Daemon: per-host connection pool; persistent channel carries protocol frames.
- TUI: host:pane identification in the UI.

**Acceptance.** Integration test with a test SSH server (via testcontainers or similar): open remote pane, send input, read output, disconnect, reattach, verify scrollback.

**Not in scope.** QUIC (Phase 10). Multi-host agent coordination (Phase 6).

**Risks.** Pure-SSH latency may feel sluggish for live telemetry; QUIC in Phase 10 is the relief valve. Acceptable for Phase 5 since the killer app here is remote pty.

---

## Phase 6 — Agent binary + remote scope panels · ⚪

**Goal.** Deploy a lightweight agent to remote hosts; the same scope panels work against remote as against local.

**Scope.**
- `tepegoz-agent` subcommand runs a stdio-framed protocol server. Targets: static musl Linux (x86_64 + aarch64), universal macOS. <5 MB per target.
- `xtask build-agents` cross-compiles all four targets into `target/agents/`.
- Controller `build.rs` reads `target/agents/` and `include_bytes!`s each arch.
- Daemon: detect remote OS + arch over SSH → `scp` the matching agent binary to `~/.cache/tepegoz/agent-<version>` → verify SHA256 → exec over SSH with stdio carrying the protocol.
- Remote scope panels: Docker, Ports, Processes — same wire protocol, agent-backed.

**Acceptance.** Full fleet test: deploy agent to a test VM via SSH, open a remote pane, verify docker panel works against remote docker, verify port scan finds a known open port on remote host.

**Not in scope.** Agent auto-update (agents are redeployed per controller version). Multi-user agents.

**Risks.** Cross-compiling the agent for 4 targets is real work. Protocol/library version drift between controller and embedded agents must be caught by CI.

---

## Phase 7 — Port scanner · ⚪

**Goal.** Port scanning as a first-class capability. TCP-connect in v1; SYN deferred to v1.1 (Linux first).

**Scope.**
- `tepegoz-scan`: port existing `pscan` tool logic into the crate. TCP-connect scan via `socket2` (SO_LINGER zero-timeout + RST close to skip TIME_WAIT on localhost sweeps); bounded concurrent fanout via tokio semaphore (default ~500).
- Wire protocol: `ScanTargets { targets, port_range, concurrency }` command; `ScanResult { target, port, state, banner }` event.
- Same code runs locally or on an agent host.

**Acceptance.** Scan `127.0.0.1` with a known listener on a known port; find it. Scan a known-dead port; see it closed.

**Not in scope.** SYN scan (v1.1 Linux-first; macOS BPF is a separate effort).

**Risks.** Some networks rate-limit outbound connections; default concurrency may need tuning.

---

## Phase 8 — Recording + replay · ⚪

**Goal.** Every pane keystroke/output is recorded, encrypted at rest, replayable offline.

**Scope.**
- `tepegoz-record`: per-pane append-only log at `~/.local/share/tepegoz/recordings/<pane-id>/<ts>.tpgr` (macOS equivalent under `~/Library/Application Support/...`).
- Encryption: `age` wrapper. Per-session key, wrapped with a root key from OS keychain (`keyring` crate), with fallback `TEPEGOZ_ROOT_KEY` inline env or `TEPEGOZ_ROOT_KEY_FILE=/path/` per `docs/DECISIONS.md#2`.
- Precedence: env > file > keychain. When any override is set, the daemon does **not** write back to the keychain.
- `tepegoz replay <pane-id>` subcommand: time-scrubbing playback with speed control + regex search.

**Acceptance.** Run a pane, produce known output, close pane. Replay; bytes match original.

**Not in scope.** Sharing recordings across machines. Multi-user access control.

**Risks.** Encryption/decryption throughput on a high-traffic pane — benchmark and tune.

---

## Phase 9 — Claude Code pane awareness · ⚪

**Goal.** Parse `~/.claude/projects/` state to augment pty pane metadata. TUI status line shows `● claude: editing foo.rs (42s)` without interrupting the agent.

**Scope.**
- Structural signature of `~/.claude/projects/` layout (set of dirnames + top-level JSON fields), compared against known-tested signatures baked into the binary.
- On unknown signature: yellow status notice, feature disabled; `tepegoz doctor --claude-layout` dumps the detected signature for bug reports. **Never crash the daemon.**
- Pure observation — no LLM calls.

**Acceptance.** Start 3 Claude Code sessions in distinct pty panes; status line for each correctly reflects current tool use and file touched.

**Not in scope.** Interaction with Claude sessions (that's v3).

**Risks.** Claude Code's state layout changes — structural signature tolerates benign additions; hard breakage requires a new signature.

---

## Phase 10 — QUIC hot path + release 0.1.0 · ⚪

**Goal.** Ship.

**Scope.**
- QUIC via `quinn` over SSH port-forward for hot-path streams (logs, high-volume pane output). Roaming survives wifi flap in ms.
- Release pipeline in `xtask package`:
  - Cross-compiled binaries (mac x86_64/arm64, linux x86_64/aarch64 musl).
  - SHA256 + **minisign signatures** on every artifact (checksums catch corruption; signatures catch tampering).
  - Homebrew tap `emincb/tap/tepegoz`.
  - `curl | sh` install script.
  - Optional `cargo install tepegoz` via crates.io.
- Agent embedding: `include_bytes!` all four arches into controller (~15 MB total), per `docs/DECISIONS.md#3`.
- Size/perf tuning: `opt-level="z"`, LTO, strip, feature-gate. Target controller <20 MB, agent <5 MB.

**Acceptance.** Download binary from GH releases, run it, full fleet demo works. Minisign signature validates against the published pubkey.

**Not in scope.** Team features. Web UI (v2). AI (v3). Auto-update.

**Risks.** minisign key management — lose the signing key and future releases can't be signed without rotation. Publish the pubkey in README + tap formula + docs.

---

## After v1 (not in this document's scope)

- **v2**: phone/web client over WSS + mTLS speaking the same wire protocol.
- **v3**: AI "god query" orchestrator layered on the v1 event stream.
