# Tepegöz

**One eye. Total visibility.**

## The problem

You are running six tools that do not know each other exist. A terminal multiplexer, a Docker desktop, `lsof` in a scratch window, an SSH session to a staging VM, another SSH session to a dev box, and somewhere a `netstat` you left open an hour ago. Port conflicts surface as confusing connection refusals instead of a name collision on the screen. Containers die silently and you notice when something downstream times out. Every remote machine is a context switch that costs you the thread you were holding.

Tepegöz collapses all of it into one screen owned by one daemon.

## What it does

```
┌─ PTY ─────────────────────┬─ Docker ──────────────────┐
│ [1] claude ~/svc-api      │ api         up 2h    :8080│
│ [2] claude ~/svc-web  *   │ web         up 2h    :3000│
│ [3] cargo test            │ postgres    up 2d    :5432│
│ [4] ssh staging           │ redis       crashloop  ×3 │
├─ Ports ───────────────────┼─ SSH Fleet ───────────────┤
│ :3000  web (docker)       │ ● staging   14 procs  ok  │
│ :5432  postgres (docker)  │ ● dev-eu    22 procs  ok  │
│ :8080  api (docker)       │ ○ bench-01  unreachable   │
│ :9229  node (pty#2) ⚠ dup │ ● gpu-03     7 procs  ok  │
├─ Claude Code ─────────────┴───────────────────────────┤
│ 3 sessions active · svc-api idle · svc-web awaiting   │
│ input · local-tools running tool call (edit)          │
└───────────────────────────────────────────────────────┘
```

First-run layout. No configuration required.

One daemon owns the state. The TUI is a client. So is the phone app, later.

## Install

```sh
brew install emincb/tap/tepegoz     # placeholder — tap not live yet
curl -fsSL https://get.tepegoz.dev | sh   # placeholder — installer not live yet
cargo install tepegoz
```

## Quick start

```sh
tepegoz daemon
tepegoz tui
```

The daemon runs headless and owns all state. The TUI is a client. Kill the TUI and reopen it — nothing is lost.

## Roadmap

| Version | What |
|---|---|
| v1 (now) | Single binary, daemon + TUI, Docker + ports + processes + SSH fleet, macOS + Linux |
| v2 | Phone and web client over the same wire protocol — zero daemon changes |
| v3 | AI god query — one prompt orchestrates actions across the entire fleet |

## Name

Tepegöz is a cyclops from the *Book of Dede Korkut*, the foundational epic of the Oghuz Turks. *Tepe* is forehead, *göz* is eye — the eye at the top. One eye, total visibility. Invulnerable to conventional weapons and outsmarted, never overpowered. The name is the product.

## License

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT) [![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](LICENSE-APACHE)

Licensed under either of MIT or Apache-2.0 at your option.

Built in Rust. No telemetry. No auto-update. Everything local.
