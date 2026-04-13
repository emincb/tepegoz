# Tepegöz

Single Rust binary that fuses pty multiplexing, SSH fleet management, Docker inspection, and port scanning into one screen. Target user runs many parallel Claude Code agents across local + remote VMs and needs a unified god view.

**Current state.** Phase 2 (pty multiplex) — tests green; one user-visible acceptance bug under diagnosis (immediate detach on attach). Phase 3 blocked on Phase 2 clearance. Full state in `docs/STATUS.md`; active issues in `docs/ISSUES.md`.

## Repo

- Remote: https://github.com/emincb/tepegoz
- Local: `/Users/emin/Documents/projects/personal/tepegoz`
- Toolchain: Rust 1.94.1 pinned via `mise.toml` (auto-activates on `cd`)
- License: MIT OR Apache-2.0

## Roadmap

- **v1** (active): daemon + TUI + scope panels + SSH agent, macOS + Linux × x86_64 + aarch64. No AI, no web UI.
- **v2**: phone/web client over WSS + mTLS speaking the same wire protocol.
- **v3**: AI "god query" orchestrator over the v1 event stream.

Full phase-by-phase plan in `docs/ROADMAP.md`.

## Architecture (one-line summary)

Single binary with subcommands (`daemon`, `tui`, `connect <host>`, `agent`, `doctor`). Headless daemon owns all state; TUI is one of many clients speaking a rkyv-archived, length-prefix-framed wire protocol over Unix socket. Controller↔agent: SSH bootstrap, QUIC-over-SSH hot path (Phase 10).

Full detail in `docs/ARCHITECTURE.md`.

## Don't in v1

- Any LLM/AI features (reserved for v3)
- Web/mobile UI (reserved for v2)
- Plugin SDK, multi-user, teams, sharing
- Hot-reload of transport, storage paths, or key sources (restart-required in v1)
- Raw SYN port scanning (TCP-connect in v1, SYN in v1.1 Linux-first)

## Commands

```sh
cargo build
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Demo (two terminals from the project dir):
```sh
./target/debug/tepegoz daemon      # terminal 1
./target/debug/tepegoz tui         # terminal 2 — detach with Ctrl-b d
```

More in `docs/OPERATIONS.md`.

## Crate layout

```
crates/
  tepegoz/           binary, subcommand dispatch (clap)
  tepegoz-proto/     rkyv wire types, codec, default socket path
  tepegoz-core/      daemon engine: state, event bus, client handlers
  tepegoz-tui/       raw-passthrough TUI attacher
  tepegoz-agent/     remote agent (stdio + probes)         [Phase 6]
  tepegoz-probe/     cross-platform probes                  [Phase 4]
  tepegoz-pty/       pty session manager + ring buffer
  tepegoz-docker/    bollard wrapper + socket discovery     [Phase 3]
  tepegoz-scan/      port scanner                            [Phase 7]
  tepegoz-ssh/       russh client + channel mux             [Phase 5]
  tepegoz-transport/ SSH + QUIC abstraction                 [Phase 10]
  tepegoz-record/    encrypted append-only pane recording   [Phase 8]
xtask/               build tasks (agent cross-compile, release packaging)
```

## Working discipline

- **Diagnose before fixing.** Read logs first; reproduce locally if possible; state the root cause before writing code. Don't paper over symptoms.
- **Machine-verify acceptance.** Every phase lands with an integration test that exercises the new behavior end-to-end. "Looks right" is not enough; the user has observed tests passing while real-world behavior diverged.
- **Sharpen the blade first.** No AI shortcuts in v1. No features that compromise the substrate.
- **Implementation autonomy, architectural guardrails.** Tactical calls (local functions, crate plumbing, logging, test shape) are yours. Anything that changes the locked commitments in `docs/DECISIONS.md` needs the user's sign-off.
- **Docs are authoritative.** When docs and reality conflict, trust reality and update the docs in the same commit. Keep `docs/STATUS.md` and `docs/ISSUES.md` current as phases land.

## Detailed docs (the authoritative written state)

- `docs/STATUS.md` — current phase state, last commit, acceptance test coverage
- `docs/ROADMAP.md` — full 10-phase plan with per-phase goals, scope, acceptance criteria
- `docs/ARCHITECTURE.md` — protocol spec, crate layout, platform matrix, concurrency model, security posture
- `docs/DECISIONS.md` — the six locked architectural decisions with reasoning
- `docs/OPERATIONS.md` — build/test/run/debug runbook, log locations, common issues
- `docs/ISSUES.md` — active bugs with diagnostic state, resolved issues archive
