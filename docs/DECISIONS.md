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

---

## Durable working rules

- Any v1 code that couples TUI rendering to daemon internals without going through the wire protocol is a bug.
- No cloud, no phone-home, no auto-update, no outbound network outside user-authorized SSH.
- Sharpen the blade first — no AI features in v1 under any circumstances.
