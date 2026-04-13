# Tepegöz

Single Rust binary that fuses pty multiplexing, SSH fleet management, Docker inspection, and port scanning into one screen. Target user runs many parallel Claude Code agents across local + remote VMs and needs a unified god view.

## Roadmap
- **v1** (current): daemon + TUI + scope panels, macOS + Linux, local + SSH agents, no AI, no web UI
- **v2**: phone/web client over WSS + mTLS speaking the same wire protocol
- **v3**: AI "god query" orchestrator layered on the v1 event stream

## Architecture
- Single binary, subcommands: `daemon`, `tui`, `connect <host>`, `agent`, `doctor`
- Headless daemon owns all state; TUI is one of many clients. Same wire protocol across all transports.
- Wire protocol: rkyv + bytecheck (validated on network boundaries, optional on trusted local Unix socket)
- Controller↔agent: SSH bootstrap; QUIC-over-SSH for hot paths (Phase 10)
- Per-pane encrypted recording with OS keychain or env/file-backed root key

## Don't in v1
- Any LLM/AI features (reserved for v3)
- Web/mobile UI (reserved for v2)
- Plugin SDK, multi-user, teams, sharing
- Hot-reload of transport / storage paths / key sources (tier 3 — restart-required)
- Raw SYN scanning (TCP-connect in v1, SYN in v1.1 Linux-first)

## Commands
- Rust pinned to 1.94.1 via `mise.toml`; mise auto-activates on `cd` into the repo
- Build: `cargo build`
- Lint: `cargo fmt --all` · `cargo clippy --workspace --all-targets -- -D warnings`
- Test: `cargo test --workspace`
- Demo (Phase 1):
  - Terminal 1: `./target/debug/tepegoz daemon`
  - Terminal 2: `./target/debug/tepegoz tui`

## Crate layout

```
crates/
  tepegoz/           binary, subcommand dispatch
  tepegoz-proto/     rkyv wire types, codec, default socket path
  tepegoz-core/      daemon engine: state, event bus, client handlers
  tepegoz-tui/       ratatui client
  tepegoz-agent/     remote agent (stdio + probes)
  tepegoz-probe/     cross-platform probes (linux/macos/common)
  tepegoz-pty/       pty session manager
  tepegoz-docker/    bollard wrapper + socket discovery
  tepegoz-scan/      port scanner (pscan evolution)
  tepegoz-ssh/       russh client + channel mux
  tepegoz-transport/ SSH + QUIC abstraction
  tepegoz-record/    encrypted append-only pane recording
xtask/               build tasks (agent cross-compile, release packaging)
```

## References
- Locked decisions with reasoning: `docs/DECISIONS.md`
- Current phase + demonstrable state: `docs/STATUS.md`
