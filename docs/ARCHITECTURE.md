# Architecture

The authoritative technical spec. Kept in sync with code as phases land.

## 1. Single binary, subcommand dispatch

`tepegoz` dispatches five modes via clap:
- `daemon` — headless background process owning all state
- `tui` — interactive client (raw-passthrough pty attacher in v1; also scope-panel views once Phase 3+ lands)
- `connect user@host` — convenience; delegates to the running daemon to open an SSH pane
- `agent` — remote-side mode, launched over SSH stdio by the daemon
- `doctor` — diagnostics (env, config, Claude Code layout signature)

All modes live in one binary. Feature-gating (Phase 10) will strip unused code for specific build targets (e.g. the agent binary can omit TUI and core).

## 2. Daemon / client split

The daemon owns:
- PTY sessions (portable-pty masters + blocking reader/waiter threads + per-pane ring buffer + broadcast channel)
- Agent connections (SSH + future QUIC tunnels)
- Probe caches (docker, processes, sockets) — Phase 3+
- Per-pane encrypted recording sinks — Phase 8
- Client connections (Unix socket; WSS+mTLS for v2)

Clients (TUI, later web/mobile, later AI orchestrator) connect to the daemon and subscribe. The daemon does not render — it publishes events. This split is what lets v2 and v3 happen without rewriting the substrate.

## 3. Wire protocol

### Frame format

```
[4-byte big-endian u32 length] [rkyv-archived Envelope]
```

### Envelope

```rust
pub struct Envelope {
    pub version: u32,
    pub payload: Payload,
}

pub const PROTOCOL_VERSION: u32 = 2;  // bumped on breaking change
```

### Payload taxonomy (v2)

Client → daemon (commands):
- `Hello(Hello { client_version, client_name })` — handshake
- `Ping` — keepalive
- `Subscribe(Subscription::Status { id })` — subscribe to the daemon status stream
- `Unsubscribe { id }` — cancel a subscription
- `OpenPane(OpenPaneSpec { shell, cwd, env, rows, cols })`
- `AttachPane { pane_id, subscription_id }`
- `ClosePane { pane_id }`
- `ListPanes`
- `SendInput { pane_id, data }`
- `ResizePane { pane_id, rows, cols }`

Daemon → client (responses and events):
- `Welcome(Welcome { daemon_version, protocol_version, daemon_pid })` — response to Hello
- `Pong` — response to Ping
- `Event(EventFrame { subscription_id, event })` — subscription-keyed event stream
- `PaneOpened(PaneInfo)` — response to OpenPane
- `PaneList { panes }` — response to ListPanes
- `Error(ErrorInfo { kind, message })` — protocol or daemon error

Events (inside `Event(EventFrame)`):
- `Status(StatusSnapshot)` — daemon heartbeat (pid, uptime, client counts, event counter, panes_open, ...)
- `PaneSnapshot { scrollback, rows, cols }` — initial replay after AttachPane
- `PaneOutput { data }` — live output chunk
- `PaneExit { exit_code }` — pane's child exited; subscription is closed
- `PaneLagged { dropped_bytes }` — subscriber fell behind; broadcast dropped events

### Validation

`rkyv::from_bytes::<Envelope, rkyv::rancor::Error>(&aligned_bytes)` — bytecheck validates the archive on every read. The trusted-local fast path (skipping validation on the Unix socket for perf) is not yet active; revisit if/when profiling demands.

### Versioning policy

- `PROTOCOL_VERSION` bumped on breaking changes.
- Peers currently reject mismatches (future: generated migration handlers between compatible versions per `docs/DECISIONS.md#1`).

### Transports (all carry the same envelope)

- **Local.** Unix socket at the default path (see §5).
- **Remote bootstrap (Phase 5).** SSH channel via `russh`.
- **Hot path (Phase 10).** QUIC via `quinn` over SSH port-forward; later direct QUIC + mTLS.
- **v2 remote client.** Same envelope over WSS + mTLS; likely with a JSON edge at the daemon for non-Rust clients (rkyv in browsers is a tarpit — see `docs/DECISIONS.md#1`).

## 4. Crate dependency graph

```
tepegoz (binary)
├── tepegoz-core (daemon engine)
│   ├── tepegoz-proto (wire types + codec + socket path)
│   └── tepegoz-pty (pty sessions)
│       └── tepegoz-proto
├── tepegoz-tui (client)
│   └── tepegoz-proto
├── tepegoz-agent (remote-side)            [Phase 6]
│   └── tepegoz-proto
├── tepegoz-docker (Docker scope)          [Phase 3]
│   └── tepegoz-proto
├── tepegoz-probe (OS probes)              [Phase 4]
├── tepegoz-scan (port scanner)            [Phase 7]
├── tepegoz-ssh (SSH transport)            [Phase 5]
├── tepegoz-transport (SSH+QUIC abstraction) [Phase 10]
└── tepegoz-record (encrypted recording)   [Phase 8]
```

`tepegoz-proto` is the spine; all client-facing crates depend on it. No cycles.

## 5. Default paths

| Concern | Linux | macOS |
|---|---|---|
| Daemon socket | `$XDG_RUNTIME_DIR/tepegoz-<uid>/daemon.sock` | `$TMPDIR/tepegoz-<uid>/daemon.sock` (falls through to `/tmp/...`) |
| TUI log | `${XDG_CACHE_HOME:-$HOME/.cache}/tepegoz/tui.log` | same |
| Config | `$XDG_CONFIG_HOME/tepegoz/config.toml` (planned) | `~/Library/Application Support/tepegoz/config.toml` |
| State DB (Phase 8+) | `$XDG_DATA_HOME/tepegoz/state.redb` | `~/Library/Application Support/tepegoz/state.redb` |
| Recordings (Phase 8) | `$XDG_DATA_HOME/tepegoz/recordings/` | `~/Library/Application Support/tepegoz/recordings/` |

Overrides: `TEPEGOZ_LOG_FILE` env for TUI log; `--socket` flag for daemon socket path.

Permissions: parent dir `0700` and socket `0600` **when default path** (daemon is confident it owns the parent). For `--socket` overrides, parent perms are left alone — the user chose the path, don't second-guess.

## 6. Platform probe matrix (Phase 4+)

| Probe | Linux | macOS |
|---|---|---|
| Process list | `procfs` (procfs crate, raw /proc) | `libproc-rs` |
| Per-pid details | `/proc/{pid}/{stat,status,cmdline}` | `libproc` + `sysctl` |
| Listening sockets | netlink `NETLINK_SOCK_DIAG` (direct kernel, no parsing) | `libproc` `PROC_PIDFDSOCKETINFO` per pid |
| CPU/mem/disk | `sysinfo` (procfs-backed) | `sysinfo` (sysctl/host_statistics) |
| FS events | inotify via `notify` | FSEvents via `notify` |
| Docker socket | `/var/run/docker.sock` | `~/.docker/run/docker.sock`, Colima, Rancher, `$DOCKER_HOST` |
| PTY | `openpty` via `portable-pty` | `openpty` via `portable-pty` |
| Raw scan (Phase 7.1) | AF_PACKET / raw socket | raw socket / BPF device |
| Keychain | secret-service / kwallet via `keyring` | macOS Keychain via `keyring` |

`tepegoz-probe` exposes `trait Probe` with `cfg(target_os)` modules `linux`, `macos`, and a common `sysinfo`-backed fallback.

## 7. PTY lifecycle

```
PtyManager::open(OpenSpec) → Arc<Pane>

1. id = self.next_id.fetch_add(1)
2. portable-pty openpty(rows, cols) → (master, slave)
3. CommandBuilder::new(shell)
   - cwd(spec.cwd)
   - env(TERM, TEPEGOZ_PANE_ID=id, spec.env...)
4. child = slave.spawn_command(cmd)
5. drop(slave) — release our copy so child sees EOF when IT exits
6. reader = master.try_clone_reader(); writer = master.take_writer()
7. output_tx = broadcast::channel::<PaneUpdate>(1024)
8. scrollback = Mutex<Scrollback::new(2 MiB)>
9. std::thread::spawn(reader_loop):
     loop: read → Bytes → LOCK { scrollback.append; output_tx.send }
10. std::thread::spawn(waiter):
     child.wait() → record exit_code, alive=false
     output_tx.send(PaneUpdate::Exit { exit_code })
     drop(tx clone)
11. pane = Arc::new(Pane { ... })
12. panes.write().insert(id, pane.clone())
```

### Lock discipline

The reader holds the scrollback mutex **across both** the append and the broadcast `send`. Releasing between them lets subscribers observe bytes in both the snapshot and the live stream — see the bug fix at `eab274c` and the regression test `subscribe_does_not_duplicate_bytes`.

### Subscriber attach flow

```rust
Pane::subscribe() -> (Bytes, broadcast::Receiver<PaneUpdate>):
  LOCK scrollback {
    snapshot = scrollback.snapshot()
    rx = output_tx.subscribe()
  }
  return (snapshot, rx)
```

The subscriber then receives `Event::PaneSnapshot { scrollback: snapshot }` first, followed by live `Event::PaneOutput` and eventually `Event::PaneExit`.

### Backpressure

Broadcast channel capacity is 1024 `PaneUpdate`s. Slow subscribers get `RecvError::Lagged(n)` — forwarder translates to `Event::PaneLagged { dropped_bytes: n }`. TUI currently just logs a warn; visual indicator is future work.

## 8. Security posture

- **Socket.** Mode `0600`; parent dir `0700` (default path only). Override paths don't get parent chmod.
- **Pane env.** `TEPEGOZ_PANE_ID=<id>` stamped into every pty's environment so clients (notably `tepegoz tui`) can detect and refuse recursive attach.
- **No cloud, no phone-home.** No auto-update. No outbound network outside user-authorized SSH.
- **Keychain root key (Phase 8).** `keyring` crate + env/file override; precedence `env > file > keychain`. Daemon does not write back to keychain when an override is set — see `docs/DECISIONS.md#2`.
- **Agent auth (Phase 6).** ed25519 host key TOFU, warn on key change. Hash-verified agent binary on deploy.
- **Audit log (Phase 6+).** Every agent RPC + every TUI command appended to a separate file; not part of the main tracing stream.

## 9. Concurrency model

- Single tokio multi-thread runtime per binary.
- Daemon:
  - Accept loop task → per-client task.
  - Per-pane reader thread (`std::thread` — NOT `spawn_blocking`, pty reads are unbounded-duration).
  - Per-pane waiter thread (`std::thread`, blocks on `child.wait()`).
  - Per-agent connection task (Phase 6+).
  - Per-client writer task owning the socket's write half, drains an unbounded mpsc of `Envelope`s.
  - Per-subscription forwarder task for each active AttachPane.
- TUI: single task with `tokio::select!` over `stdin.read`, `read_envelope`, `winch.recv`. No ratatui rendering in v1 (raw passthrough).

### Backpressure summary

- **Broadcast** (pane → subscribers): drops under sustained slow consumer; `Lagged(n)` surfaces the loss.
- **Mpsc** (forwarders → writer task): unbounded; short-lived frames, low risk.
- **Socket** (reader ↔ daemon): daemon reads as fast as it dispatches; no backpressure propagated upstream.

## 10. Build / release (Phase 10)

- `cargo-zigbuild` for cross-compile (musl + universal mac).
- `release-agent` profile with `opt-level="z"` + LTO + strip for agent binary (<5 MB).
- `release` profile with LTO + `codegen-units=1` + strip for controller (<20 MB).
- CI matrix builds all four targets; release pipeline creates minisign-signed artifacts + SHA256 manifests.
