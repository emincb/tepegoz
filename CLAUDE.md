# Tepegöz

Single Rust binary that fuses pty multiplexing, SSH fleet management, Docker inspection, and port scanning into one screen. Target user runs many parallel Claude Code agents across local + remote VMs and needs a unified god view.

**Current state.** Phases 1–6 closed (2026-04-16). v1.0 release work (renamed Phase 10 — install packaging) is the active scope; Slice R1 (release binary cross-build xtask) is up next. Phases 7/8/9 deferred to v1.1 candidates per the v1 scope trim (Decision #8). Full state in `docs/STATUS.md`; active issues in `docs/ISSUES.md`; roadmap in `docs/ROADMAP.md`.

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
- **Machine-verify acceptance.** Every phase lands with an integration test that exercises the new behavior end-to-end. "Looks right" is not enough; tests passing have diverged from real-world behavior before.
- **Sharpen the blade first.** No AI shortcuts in v1. No features that compromise the substrate.
- **Docs are authoritative.** When docs and reality conflict, trust reality and update the docs in the same commit. Keep `docs/STATUS.md` and `docs/ISSUES.md` current as slices land.

## Operating model — single-session autonomy (from 2026-04-17)

Prior "CTO + engineer in separate sessions, user relays" discipline is retired. One Claude session runs planning + implementation + verification + commit + push end-to-end. User involvement gates on product decisions only.

- **Autonomy default.** Plan → implement → verify → commit → push → next slice. No pauses between slices for approval.
- **Escalate when** changing a locked `DECISIONS.md` entry · rescoping v1 / v1.1 / v2 · real product-direction tradeoffs (feel, UX, feature shape) · destructive ops on shared state (force-push main, prod data, credential ops, third-party service spending) · genuine "I don't know what you want" scope call.
- **Self-verification before every commit (no exceptions):**
  - `cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo test --workspace` (count holds or rises with expected delta)
  - Touched `cargo xtask demo-*`? Cold-walk both failure AND success paths before declaring ready (per `feedback_demo_tooling_cold_walk`).
  - Touched >1 crate or a load-bearing path? Run the `simplify` skill (or spawn a Review subagent) before commit.
  - TUI behavior changed? Real-terminal eyeball OR explicit "state-machine pinned only, not eyeballed" in commit body.
  - Cross-check against the README mockup / product vision (per `feedback_slice_vision_crosscheck`).
- **Plan mode for non-trivial slices** — 3+ commits, wire protocol bumps, new modules, cross-crate refactors. Skip for bug fixes, polish, docs-only.
- **Subagents freely.** Explore for research · general-purpose for parallel independent work · Plan agent for scoping big slices · Loop for self-paced multi-slice drives. Parallel tool calls when independent.
- **Destructive ops still gate.** `git push --force` on main, `rm -rf` outside workspace tmp, DB drops, credential ops, third-party publishes — ask first. Everything reversible (reset local, rewrite local commit, delete local file) is free.
- **End-of-slice signal.** Commit + push + STATUS/ISSUES/DECISIONS updates in the same commit where relevant. Tell user what landed in ≤2 lines.

## Session start ritual

1. Read this file (auto-loaded) + `docs/STATUS.md` + `docs/ISSUES.md`.
2. `git log --oneline -10` — verify recent commits match what STATUS.md claims.
3. `cargo test --workspace` if touching code; green before starting new work.
4. If STATUS is stale vs. reality, fix it before acting.

## Detailed docs (the authoritative written state)

- `docs/STATUS.md` — current phase state, last commit, acceptance test coverage
- `docs/ROADMAP.md` — full phase plan with per-phase goals, scope, acceptance criteria
- `docs/ARCHITECTURE.md` — protocol spec, crate layout, platform matrix, concurrency model, security posture
- `docs/DECISIONS.md` — locked architectural decisions with reasoning
- `docs/OPERATIONS.md` — build/test/run/debug runbook, log locations, common issues
- `docs/ISSUES.md` — active bugs with diagnostic state, resolved issues archive
