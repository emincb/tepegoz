//! `cargo xtask demo-phase-6 {up,down} [--remote]` — one-command
//! runners for the Phase 6 Slice 6a local handshake demo + the
//! Slice 6b remote deploy demo.
//!
//! ## Local mode (default)
//!
//! `up` builds `tepegoz-agent` for the host target, spawns it as a
//! subprocess with piped stdio, drives a single `AgentHandshake`
//! envelope, prints the response. Fast, no docker required. Slice
//! 6a's original flow.
//!
//! ## Remote mode (`--remote`, Slice 6b)
//!
//! `up --remote` provisions an sshd container (same linuxserver
//! image as `demo-phase-5` but under a separate container name
//! so the two demos don't collide), generates a throwaway ed25519
//! keypair, cross-builds `tepegoz-agent` for
//! `x86_64-unknown-linux-musl`, connects via `tepegoz-ssh`, runs
//! `deploy_agent` + a handshake round-trip over the exec channel,
//! and prints what the agent reported. `down --remote` removes the
//! container + tempdir.
//!
//! **Cross-compile preflight is step 0** (Phase 6 Slice 6e-prep-3).
//! Before any keypair / container / cargo-download side effect, the
//! runner asserts we have a working cross-compile path at both layers
//! rustc cares about:
//!
//! 1. **Rust std-lib for the target** (`rustup target add
//!    x86_64-unknown-linux-musl`) — rustc needs a precompiled
//!    libstd/libcore for the target. OS-independent.
//! 2. **A linker that can produce ELF for musl** — on macOS,
//!    `cargo-zigbuild` on PATH (ld64 can't produce ELF); on Linux,
//!    either `cargo-zigbuild` or `musl-gcc` on PATH.
//!
//! Prior versions conflated the two layers (accepting "zigbuild OR
//! rustup target") which green-lit a macOS host with zigbuild but no
//! musl std, trap-dooring through a mid-build failure that left an
//! sshd container behind (round-2 regression, 2026-04-16). The
//! preflight now asserts BOTH layers and emits a composite install
//! hint listing every missing piece.
//!
//! **Side-effect ordering** (Slice 6e-prep-3): agent cross-build runs
//! BEFORE any keypair generation, tempdir creation, or container
//! start. A future-unknown cross-build failure leaves zero artifacts
//! on the user's host — no dangling fixture state to clean up.
//!
//! Per the standing demo-tooling rule: cold-start ≤ 60 s. The
//! remote path's critical path is docker pull (one-shot) + sshd
//! boot (~2 s) + agent cross-build (~15 s fresh, cached
//! thereafter) + connect/deploy/handshake (~1 s).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use tepegoz_proto::{
    Envelope, PROTOCOL_VERSION, Payload,
    codec::{read_envelope, write_envelope},
};
use tokio::process::Command as AsyncCommand;

/// Container name for Slice 6b's sshd fixture. Distinct from
/// demo-phase-5's so the two demos don't race on the same
/// container handle.
const REMOTE_CONTAINER_NAME: &str = "tepegoz-demo-phase-6-sshd";
const REMOTE_IMAGE: &str = "lscr.io/linuxserver/openssh-server:latest";
// Container's internal sshd port is 2222; the docker host binding
// picks a random ephemeral port published in `docker port`. We
// reference `2222/tcp` as a literal in the port-query command
// rather than through this const, but naming it here makes the
// relationship self-documenting; `#[allow(dead_code)]` prevents the
// unused-const lint from firing.
#[allow(dead_code)]
const SSHD_INTERNAL_PORT: u16 = 2222;
const REMOTE_AGENT_TRIPLE: &str = "x86_64-unknown-linux-musl";
const TCP_READY_BUDGET: Duration = Duration::from_secs(30);
const SSHD_BANNER_GRACE: Duration = Duration::from_millis(500);

/// Stable demo root so `down` can clean up after a crashed `up`.
fn demo_root() -> PathBuf {
    std::env::temp_dir().join("tepegoz-demo-phase-6")
}

// --------------------------------------------------------------------
// Entry points (called from xtask/src/main.rs)
// --------------------------------------------------------------------

pub(crate) fn up(remote: bool) -> Result<()> {
    if remote { remote_up() } else { local_up() }
}

pub(crate) fn down(remote: bool) -> Result<()> {
    if remote { remote_down() } else { local_down() }
}

// --------------------------------------------------------------------
// Local mode (Slice 6a)
// --------------------------------------------------------------------

fn local_up() -> Result<()> {
    preflight_cargo()?;
    let root = demo_root();
    fs::create_dir_all(&root)
        .with_context(|| format!("creating demo root at {}", root.display()))?;

    println!("[demo-phase-6] building tepegoz-agent for the host target…");
    cargo_build_host_agent()?;
    let bin = PathBuf::from("target/debug/tepegoz-agent");
    if !bin.exists() {
        bail!(
            "expected `{}` after cargo build — invoke `cargo xtask demo-phase-6 up` from the workspace root",
            bin.display()
        );
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(drive_local_handshake(&bin))
}

async fn drive_local_handshake(bin: &Path) -> Result<()> {
    println!("[demo-phase-6] spawning agent subprocess…");
    let mut child = AsyncCommand::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let mut stdin = child.stdin.take().context("agent stdin piped by spawn()")?;
    let mut stdout = child
        .stdout
        .take()
        .context("agent stdout piped by spawn()")?;

    let request_id = 1u64;
    println!("[demo-phase-6] sending AgentHandshake {{ request_id: {request_id} }}…");
    write_envelope(
        &mut stdin,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AgentHandshake { request_id },
        },
    )
    .await
    .context("write handshake envelope")?;

    println!("[demo-phase-6] awaiting AgentHandshakeResponse…");
    let response = tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut stdout))
        .await
        .context("timeout: agent didn't respond within 5 s")?
        .context("decode response envelope")?;

    print_handshake(response)?;

    drop(stdin);
    drop(stdout);
    let status = child.wait().await.context("wait on agent subprocess")?;
    if !status.success() {
        bail!("agent exited non-zero after handshake: {status}");
    }
    println!("[demo-phase-6] agent exited cleanly. Done.");
    Ok(())
}

fn local_down() -> Result<()> {
    let root = demo_root();
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("removing demo root {}", root.display()))?;
        println!("[demo-phase-6] removed {}", root.display());
    } else {
        println!("[demo-phase-6] nothing to tear down (no fixture present).");
    }
    Ok(())
}

// --------------------------------------------------------------------
// Remote mode (Slice 6b)
// --------------------------------------------------------------------

fn remote_up() -> Result<()> {
    // Ordering is load-bearing: preflight the environment, THEN build
    // the agent (which can fail if a toolchain assumption surprises
    // us), THEN create any side-effect state (keypair, tempdir,
    // container). A cross-build failure leaves zero artifacts on the
    // user's host — no dangling sshd container, no tempdir. The
    // reordering sidesteps the whole class of "preflight missed a
    // precondition and the rebuild-midflight failure left orphans"
    // bugs (round-2 regression, 2026-04-16).
    preflight_remote()?;

    println!("[demo-phase-6] building tepegoz-agent for {REMOTE_AGENT_TRIPLE}…");
    let agent_bin = cargo_build_linux_musl_agent()?;
    println!("[demo-phase-6]   → {}", agent_bin.display());

    let paths = RemotePaths::resolve();
    paths.ensure_dirs()?;
    generate_keypair(&paths)?;

    remove_container_if_present(REMOTE_CONTAINER_NAME);
    let port = start_sshd_container(&paths)?;
    wait_for_tcp(port, TCP_READY_BUDGET)?;
    std::thread::sleep(SSHD_BANNER_GRACE);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    // Container + tempdir live through the remote-down path; we don't
    // tear down here so a user can `ssh` into the fixture for
    // investigation if the deploy flow surprised them. `remote_down`
    // handles cleanup.
    runtime.block_on(drive_remote_deploy(&paths, port, &agent_bin))
}

async fn drive_remote_deploy(paths: &RemotePaths, port: u16, agent_bin: &Path) -> Result<()> {
    use tepegoz_ssh::{
        HostEntry, HostList, HostSource, KnownHostsStore, connect_host, deploy_agent,
        handshake_agent, spawn_agent_channel,
    };

    // Construct a HostList inline — no config.toml roundtrip needed.
    let host = HostEntry {
        alias: "demo6b".into(),
        hostname: "127.0.0.1".into(),
        port,
        user: "tepegoz".into(),
        identity_files: vec![paths.key_private.to_string_lossy().into_owned()],
        proxy_jump: None,
    };
    // HostSource::None suffices — the demo doesn't render the
    // source label anywhere user-visible. A synthetic source
    // variant would be over-engineered for a single demo caller.
    let list = HostList {
        source: HostSource::None,
        hosts: vec![host],
        autoconnect: std::collections::HashSet::new(),
    };

    // Isolated known_hosts so the demo never clobbers the user's
    // tepegoz TOFU database.
    let known_hosts_path = paths.root.join("known_hosts");
    let known_hosts = KnownHostsStore::open_at(&known_hosts_path);

    println!("[demo-phase-6] connecting via SSH (TOFU → isolated known_hosts)…");
    let session = connect_host("demo6b", &list, &known_hosts)
        .await
        .context("connect_host for demo6b failed — SSH auth or TOFU error")?;

    println!(
        "[demo-phase-6] reading agent bytes from {}…",
        agent_bin.display()
    );
    let bytes = fs::read(agent_bin).with_context(|| format!("reading {}", agent_bin.display()))?;
    println!("[demo-phase-6]   → {} bytes", bytes.len());

    println!("[demo-phase-6] deploying agent (idempotent — cache hit if already matching)…");
    let outcome = deploy_agent(&session, &bytes, PROTOCOL_VERSION)
        .await
        .context("deploy_agent")?;
    println!(
        "[demo-phase-6]   target: {} ({} {})",
        outcome.target.target_triple, outcome.target.os, outcome.target.arch
    );
    println!(
        "[demo-phase-6]   path:   {} ({})",
        outcome.remote_path,
        if outcome.deployed_now {
            "uploaded now"
        } else {
            "cache hit"
        }
    );
    println!("[demo-phase-6]   sha256: {}", &outcome.sha256_hex[..16]);

    println!("[demo-phase-6] spawning remote agent + handshake…");
    let mut channel = spawn_agent_channel(&session, &outcome.remote_path)
        .await
        .context("spawn_agent_channel")?;
    let info = handshake_agent(&mut channel, PROTOCOL_VERSION)
        .await
        .context("handshake_agent")?;

    println!();
    println!("  remote agent handshake ✓");
    println!("    host:         demo6b (127.0.0.1:{port})");
    println!("    version:      {}", info.version);
    println!("    os:           {}", info.os);
    println!("    arch:         {}", info.arch);
    if info.capabilities.is_empty() {
        println!("    capabilities: (none — agent couldn't reach a docker socket)");
    } else {
        println!("    capabilities: {}", info.capabilities.join(", "));
    }
    println!();

    // Phase 6 Slice 6c-iii: subscribe the remote agent to Docker +
    // wait for the first event. Exercises the agent's new
    // subscription-capable server path end-to-end over SSH. Emits
    // `ContainerList` iff the agent has a reachable docker socket
    // (e.g. host's /var/run/docker.sock bind-mounted), otherwise
    // `DockerUnavailable` — either path proves the round-trip.
    drive_remote_docker_subscribe(&mut channel).await?;

    // Phase 6 Slice 6d-ii: same shape for Ports + Processes — the
    // agent always advertises both capabilities on supported
    // platforms, so these arms emit real `PortList` / `ProcessList`
    // events from the remote host.
    drive_remote_ports_subscribe(&mut channel).await?;
    drive_remote_processes_subscribe(&mut channel).await?;

    let _ = session.disconnect().await;
    println!("[demo-phase-6] remote deploy + handshake + Docker/Ports/Processes subs complete.");
    println!(
        "[demo-phase-6] fixture left running; tear down with `cargo xtask demo-phase-6 down --remote`."
    );
    Ok(())
}

/// Drive a one-shot `Subscribe(Docker)` against an already-
/// handshaked agent channel + print the first Event that arrives.
/// Slice 6c-iii extension to the 6b demo: the visual proof that
/// remote Docker subscriptions actually round-trip.
async fn drive_remote_docker_subscribe(channel: &mut tepegoz_ssh::SshChannel) -> Result<()> {
    use tepegoz_proto::{
        Envelope, Event, EventFrame, PROTOCOL_VERSION, Payload, ScopeTarget, Subscription,
        codec::read_envelope,
    };

    // Serialize + write a Subscribe(Docker) envelope, same inline
    // serialize pattern `handshake_agent` uses (russh::Channel lacks
    // an AsyncWrite impl, so the codec's write path doesn't apply).
    let sub_id: u64 = 424242;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Subscribe(Subscription::Docker {
            id: sub_id,
            target: ScopeTarget::Local,
        }),
    };
    let body = rkyv::to_bytes::<rkyv::rancor::Error>(&env)
        .map_err(|e| anyhow::anyhow!("serialize Subscribe(Docker): {e}"))?;
    let len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("Subscribe envelope too large: {}", body.len()))?;

    let inner = channel.channel_mut();
    inner
        .data(len.to_be_bytes().to_vec().as_slice())
        .await
        .map_err(|e| anyhow::anyhow!("write length prefix: {e}"))?;
    inner
        .data(body.as_ref())
        .await
        .map_err(|e| anyhow::anyhow!("write body: {e}"))?;

    println!("[demo-phase-6] subscribed Docker on the agent; awaiting first event…");
    let mut reader = inner.make_reader();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_envelope(&mut reader),
    )
    .await
    .map_err(|_| anyhow::anyhow!("agent didn't respond to Subscribe(Docker) within 10s"))?
    .map_err(|e| anyhow::anyhow!("read Event: {e}"))?;

    match response.payload {
        Payload::Event(EventFrame {
            subscription_id,
            event,
        }) => {
            assert_eq!(
                subscription_id, sub_id,
                "agent must echo the subscription id"
            );
            match event {
                Event::ContainerList {
                    containers,
                    engine_source,
                } => {
                    println!();
                    println!("  remote Docker subscribe ✓");
                    println!("    event:       ContainerList");
                    println!("    containers:  {}", containers.len());
                    println!("    source:      {engine_source}");
                    for c in containers.iter().take(3) {
                        let name = c
                            .names
                            .first()
                            .map(|n| n.trim_start_matches('/'))
                            .unwrap_or("(no name)");
                        println!("      - {name}  ({})", c.image);
                    }
                    if containers.len() > 3 {
                        println!("      … (+{} more)", containers.len() - 3);
                    }
                    println!();
                }
                Event::DockerUnavailable { reason } => {
                    println!();
                    println!("  remote Docker subscribe ✓ (DockerUnavailable path)");
                    println!("    event:       DockerUnavailable");
                    println!("    reason:      {reason}");
                    println!(
                        "    note:        bind-mount /var/run/docker.sock into the sshd \
                         container (and add tepegoz to the docker group) for a ContainerList."
                    );
                    println!();
                }
                other => {
                    anyhow::bail!("expected ContainerList or DockerUnavailable, got {other:?}");
                }
            }
        }
        other => {
            anyhow::bail!("expected Event envelope, got {other:?}");
        }
    }
    Ok(())
}

/// Phase 6 Slice 6d-ii: drive a one-shot `Subscribe(Ports)` against
/// the agent + print the first event. Same shape as
/// `drive_remote_docker_subscribe`.
async fn drive_remote_ports_subscribe(channel: &mut tepegoz_ssh::SshChannel) -> Result<()> {
    use tepegoz_proto::{
        Envelope, Event, EventFrame, PROTOCOL_VERSION, Payload, ScopeTarget, Subscription,
        codec::read_envelope,
    };

    let sub_id: u64 = 525252;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Subscribe(Subscription::Ports {
            id: sub_id,
            target: ScopeTarget::Local,
        }),
    };
    let body = rkyv::to_bytes::<rkyv::rancor::Error>(&env)
        .map_err(|e| anyhow::anyhow!("serialize Subscribe(Ports): {e}"))?;
    let len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("Subscribe envelope too large: {}", body.len()))?;

    let inner = channel.channel_mut();
    inner
        .data(len.to_be_bytes().to_vec().as_slice())
        .await
        .map_err(|e| anyhow::anyhow!("write length prefix: {e}"))?;
    inner
        .data(body.as_ref())
        .await
        .map_err(|e| anyhow::anyhow!("write body: {e}"))?;

    println!("[demo-phase-6] subscribed Ports on the agent; awaiting first event…");
    let mut reader = inner.make_reader();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_envelope(&mut reader),
    )
    .await
    .map_err(|_| anyhow::anyhow!("agent didn't respond to Subscribe(Ports) within 10s"))?
    .map_err(|e| anyhow::anyhow!("read Event: {e}"))?;

    match response.payload {
        Payload::Event(EventFrame {
            subscription_id,
            event,
        }) => {
            assert_eq!(subscription_id, sub_id);
            match event {
                Event::PortList { ports, source } => {
                    println!();
                    println!("  remote Ports subscribe ✓");
                    println!("    event:       PortList");
                    println!("    ports:       {}", ports.len());
                    println!("    source:      {source}");
                    for p in ports.iter().take(3) {
                        println!(
                            "      - {}/{}  pid={} {}",
                            p.protocol, p.local_port, p.pid, p.process_name
                        );
                    }
                    if ports.len() > 3 {
                        println!("      … (+{} more)", ports.len() - 3);
                    }
                    println!();
                }
                Event::PortsUnavailable { reason } => {
                    println!();
                    println!("  remote Ports subscribe ✓ (PortsUnavailable path)");
                    println!("    reason:      {reason}");
                    println!();
                }
                other => {
                    anyhow::bail!("expected PortList or PortsUnavailable, got {other:?}");
                }
            }
        }
        other => anyhow::bail!("expected Event envelope, got {other:?}"),
    }
    Ok(())
}

/// Phase 6 Slice 6d-ii: drive a one-shot `Subscribe(Processes)`
/// against the agent + print the first event. Same shape as
/// `drive_remote_ports_subscribe`.
async fn drive_remote_processes_subscribe(channel: &mut tepegoz_ssh::SshChannel) -> Result<()> {
    use tepegoz_proto::{
        Envelope, Event, EventFrame, PROTOCOL_VERSION, Payload, ScopeTarget, Subscription,
        codec::read_envelope,
    };

    let sub_id: u64 = 626262;
    let env = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Subscribe(Subscription::Processes {
            id: sub_id,
            target: ScopeTarget::Local,
        }),
    };
    let body = rkyv::to_bytes::<rkyv::rancor::Error>(&env)
        .map_err(|e| anyhow::anyhow!("serialize Subscribe(Processes): {e}"))?;
    let len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("Subscribe envelope too large: {}", body.len()))?;

    let inner = channel.channel_mut();
    inner
        .data(len.to_be_bytes().to_vec().as_slice())
        .await
        .map_err(|e| anyhow::anyhow!("write length prefix: {e}"))?;
    inner
        .data(body.as_ref())
        .await
        .map_err(|e| anyhow::anyhow!("write body: {e}"))?;

    println!("[demo-phase-6] subscribed Processes on the agent; awaiting first event…");
    let mut reader = inner.make_reader();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_envelope(&mut reader),
    )
    .await
    .map_err(|_| anyhow::anyhow!("agent didn't respond to Subscribe(Processes) within 10s"))?
    .map_err(|e| anyhow::anyhow!("read Event: {e}"))?;

    match response.payload {
        Payload::Event(EventFrame {
            subscription_id,
            event,
        }) => {
            assert_eq!(subscription_id, sub_id);
            match event {
                Event::ProcessList { rows, source } => {
                    println!();
                    println!("  remote Processes subscribe ✓");
                    println!("    event:       ProcessList");
                    println!("    rows:        {}", rows.len());
                    println!("    source:      {source}");
                    for r in rows.iter().take(3) {
                        println!(
                            "      - pid={} {} ({} bytes)",
                            r.pid, r.command, r.mem_bytes
                        );
                    }
                    if rows.len() > 3 {
                        println!("      … (+{} more)", rows.len() - 3);
                    }
                    println!();
                }
                Event::ProcessesUnavailable { reason } => {
                    println!();
                    println!("  remote Processes subscribe ✓ (ProcessesUnavailable path)");
                    println!("    reason:      {reason}");
                    println!();
                }
                other => {
                    anyhow::bail!("expected ProcessList or ProcessesUnavailable, got {other:?}");
                }
            }
        }
        other => anyhow::bail!("expected Event envelope, got {other:?}"),
    }
    Ok(())
}

fn remote_down() -> Result<()> {
    remove_container_if_present(REMOTE_CONTAINER_NAME);
    let root = demo_root();
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("removing demo root {}", root.display()))?;
        println!("[demo-phase-6] removed {}", root.display());
    } else {
        println!("[demo-phase-6] tempdir already absent.");
    }
    Ok(())
}

// --------------------------------------------------------------------
// Remote fixture helpers (localized — not shared with demo-phase-5's
// fixture code to keep the blast radius on Phase 6 polish small)
// --------------------------------------------------------------------

struct RemotePaths {
    root: PathBuf,
    key_private: PathBuf,
    key_public: PathBuf,
}

impl RemotePaths {
    fn resolve() -> Self {
        let root = demo_root();
        Self {
            key_private: root.join("id_ed25519"),
            key_public: root.join("id_ed25519.pub"),
            root,
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating demo root at {}", self.root.display()))?;
        Ok(())
    }
}

fn preflight_cargo() -> Result<()> {
    ensure_on_path(
        "cargo",
        "cargo is required — install Rust via https://rustup.rs and retry",
    )
}

fn preflight_remote() -> Result<()> {
    // Order is load-bearing: the cross-build toolchain check MUST run
    // before we generate keypairs, spawn containers, or do any other
    // side-effect-bearing setup. The failure mode this guards against
    // is a macOS host with cargo on PATH but no zigbuild + no musl
    // linker — plain `cargo build --target x86_64-unknown-linux-musl`
    // trap-doors through 5+ downloaded crates before failing with
    // "can't find crate for `core`", leaving an sshd container + tempdir
    // behind. Catching it up-front is the demo-tooling invariant.
    preflight_cargo()?;
    ensure_on_path(
        "docker",
        "Docker is required for the --remote demo — install Docker Desktop or Colima and retry",
    )?;
    ensure_on_path(
        "ssh-keygen",
        "ssh-keygen is required for the --remote demo — part of standard OpenSSH client tools",
    )?;
    preflight_cross_build_toolchain()?;
    let status = std::process::Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning docker info")?;
    if !status.success() {
        bail!(
            "`docker info` failed — is the Docker daemon running? Start Docker Desktop / Colima / `systemctl start docker`."
        );
    }
    Ok(())
}

/// Ensure we have a working cross-compile path to
/// `x86_64-unknown-linux-musl` before touching any side effects.
///
/// Two independent layers must BOTH be present:
///
/// 1. **Rust std-lib for the target.** rustc needs a precompiled
///    `libstd`/`libcore` for the musl target to link against. Installed
///    via `rustup target add x86_64-unknown-linux-musl`. OS-independent
///    — macOS and Linux hosts both need it.
/// 2. **A linker that can produce ELF for musl.** On macOS, ld64 can't
///    produce ELF, so `cargo-zigbuild` is effectively required (it
///    slots zig's cross-linker in place of the host linker). On Linux,
///    either `cargo-zigbuild` OR `musl-gcc` (or equivalent) on PATH is
///    sufficient.
///
/// The prior version of this preflight conflated the two layers —
/// accepting "zigbuild OR rustup-target" as an OR, which green-lit a
/// host with zigbuild but no musl std, then failed mid-build with
/// `can't find crate for 'core'` and left an sshd container behind
/// (round-2 regression, CTO-flagged 2026-04-16).
///
/// Emits a single composite install hint covering every missing
/// layer; exits non-zero on any layer's absence; zero side effects
/// before returning.
fn preflight_cross_build_toolchain() -> Result<()> {
    let rust_target_missing = !rustup_has_target(REMOTE_AGENT_TRIPLE);
    let zigbuild_missing = !is_on_path("cargo-zigbuild");
    let musl_linker_missing = !is_on_path("musl-gcc");

    let linker_missing = if cfg!(target_os = "macos") {
        // macOS: only zigbuild is a supported linker path.
        zigbuild_missing
    } else {
        // Linux: either zigbuild OR a system musl linker works.
        zigbuild_missing && musl_linker_missing
    };

    if !rust_target_missing && !linker_missing {
        return Ok(());
    }

    let mut install_lines: Vec<String> = Vec::new();
    if zigbuild_missing && cfg!(target_os = "macos") {
        install_lines.push("cargo install --locked cargo-zigbuild".into());
        install_lines.push("brew install zig".into());
    } else if zigbuild_missing && linker_missing {
        // Linux without any linker: recommend zigbuild (cross-platform,
        // single install shape) over musl-gcc (distro-specific).
        install_lines.push("cargo install --locked cargo-zigbuild".into());
        install_lines.push("# then your package manager's zig: apt|dnf|pacman install zig".into());
    }
    if rust_target_missing {
        install_lines.push(format!("rustup target add {REMOTE_AGENT_TRIPLE}"));
    }
    let install_block = install_lines.join("\n  && ");

    bail!(
        "cross-compiling the Linux agent needs Rust std for {REMOTE_AGENT_TRIPLE} AND a musl-capable linker.\n\
         Missing on this host:{missing_summary}\n\
         \n\
         Install:\n  {install_block}\n\
         \n\
         Then re-run:\n  cargo xtask demo-phase-6 up --remote",
        missing_summary = summarize_missing(
            rust_target_missing,
            zigbuild_missing,
            musl_linker_missing,
            cfg!(target_os = "macos"),
        ),
    )
}

fn summarize_missing(
    rust_target: bool,
    zigbuild: bool,
    musl_linker: bool,
    is_macos: bool,
) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if rust_target {
        parts.push("\n  - rustup target (std-lib for the target)");
    }
    if is_macos && zigbuild {
        parts.push("\n  - cargo-zigbuild (linker)");
    } else if !is_macos && zigbuild && musl_linker {
        parts.push("\n  - cargo-zigbuild or musl-gcc (linker)");
    }
    parts.concat()
}

/// Returns true iff `rustup target list --installed` lists `target`.
/// Returns false if rustup isn't on PATH or the subcommand fails —
/// conservative: treat "can't tell" as "not installed" so the user
/// gets the actionable install hint rather than a silent fall-through.
fn rustup_has_target(target: &str) -> bool {
    let output = std::process::Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.trim() == target),
        _ => false,
    }
}

/// Non-bailing variant of `ensure_on_path` — returns a bool rather
/// than propagating the install-hint error. Used by preflight paths
/// that want to branch on "is X installed?" before deciding the
/// right diagnostic to emit.
fn is_on_path(binary: &str) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn ensure_on_path(binary: &str, hint: &str) -> Result<()> {
    let result = std::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("`{binary}` not found on PATH. {hint}")
        }
        Err(e) => bail!("failed to probe `{binary}`: {e}. {hint}"),
    }
}

fn generate_keypair(paths: &RemotePaths) -> Result<()> {
    if paths.key_private.exists() && paths.key_public.exists() {
        return Ok(());
    }
    println!("[demo-phase-6] generating ed25519 keypair…");
    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-q",
            "-C",
            "tepegoz-demo-phase-6",
        ])
        .arg("-f")
        .arg(&paths.key_private)
        .status()
        .context("spawning ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed (exit: {status}) — key generation aborted");
    }
    Ok(())
}

fn remove_container_if_present(name: &str) {
    let _ = std::process::Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn start_sshd_container(paths: &RemotePaths) -> Result<u16> {
    let public_key = fs::read_to_string(&paths.key_public)
        .with_context(|| format!("reading public key at {}", paths.key_public.display()))?
        .trim()
        .to_string();

    println!("[demo-phase-6] starting sshd container `{REMOTE_CONTAINER_NAME}`…");
    let run = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            REMOTE_CONTAINER_NAME,
            "-e",
            "PUID=1000",
            "-e",
            "PGID=1000",
            "-e",
            "USER_NAME=tepegoz",
            "-e",
            "SUDO_ACCESS=false",
            "-e",
            "PASSWORD_ACCESS=false",
            "-e",
            &format!("PUBLIC_KEY={public_key}"),
            "-p",
            "0:2222",
            REMOTE_IMAGE,
        ])
        .stderr(Stdio::inherit())
        .output()
        .context("spawning docker run")?;
    if !run.status.success() {
        bail!(
            "docker run failed (exit {}): {}",
            run.status,
            String::from_utf8_lossy(&run.stderr)
        );
    }

    let port_out = std::process::Command::new("docker")
        .args(["port", REMOTE_CONTAINER_NAME, "2222/tcp"])
        .output()
        .context("spawning docker port")?;
    if !port_out.status.success() {
        bail!(
            "docker port {REMOTE_CONTAINER_NAME} failed: {}",
            String::from_utf8_lossy(&port_out.stderr)
        );
    }
    let port_line = String::from_utf8_lossy(&port_out.stdout);
    // Each mapping line is `0.0.0.0:<port>` (docker may add tcp6 too;
    // take the first).
    let port: u16 = port_line
        .lines()
        .next()
        .and_then(|l| l.split(':').next_back())
        .and_then(|p| p.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("couldn't parse port from `{port_line}`"))?;
    println!("[demo-phase-6] sshd listening at 127.0.0.1:{port}");
    Ok(port)
}

fn wait_for_tcp(port: u16, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("sshd never bound 127.0.0.1:{port} within {budget:?}")
}

fn cargo_build_host_agent() -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--package",
            "tepegoz-agent",
            "--bin",
            "tepegoz-agent",
        ])
        .status()
        .context("spawning cargo build")?;
    if !status.success() {
        bail!("cargo build for tepegoz-agent failed — fix the compile error and retry");
    }
    Ok(())
}

/// Cross-compile the agent for the sshd container's target
/// (`x86_64-unknown-linux-musl`). `preflight_cross_build_toolchain`
/// guarantees one of the two paths below is viable before any
/// side-effect-bearing setup has run; here we just pick whichever
/// tool is on PATH with no fallback-with-warning shape.
fn cargo_build_linux_musl_agent() -> Result<PathBuf> {
    let subcmd = if is_on_path("cargo-zigbuild") {
        "zigbuild"
    } else {
        // Preflight has verified the rustup musl target exists on
        // Linux hosts; on macOS we bailed before reaching here.
        "build"
    };
    run_cargo_build(&[subcmd]).with_context(|| {
        format!("cross-build for {REMOTE_AGENT_TRIPLE} failed — inspect the cargo output above.")
    })?;

    let bin = PathBuf::from("target")
        .join(REMOTE_AGENT_TRIPLE)
        .join("release")
        .join("tepegoz-agent");
    if !bin.exists() {
        bail!(
            "expected {} after cross-build — did the profile or bin name change?",
            bin.display()
        );
    }
    Ok(bin)
}

fn run_cargo_build(cmd: &[&str]) -> Result<()> {
    let mut c = std::process::Command::new("cargo");
    c.args(cmd);
    c.args([
        "--release",
        "--package",
        "tepegoz-agent",
        "--bin",
        "tepegoz-agent",
        "--target",
        REMOTE_AGENT_TRIPLE,
    ]);
    let status = c.status().context("spawning cargo")?;
    if !status.success() {
        bail!("cargo {:?} exited {}", cmd, status);
    }
    Ok(())
}

fn print_handshake(response: Envelope) -> Result<()> {
    match response.payload {
        Payload::AgentHandshakeResponse {
            request_id: echoed,
            version,
            os,
            arch,
            capabilities,
        } => {
            println!();
            println!("  agent handshake ✓");
            println!("    request_id:   {echoed}");
            println!("    version:      {version}");
            println!("    os:           {os}");
            println!("    arch:         {arch}");
            if capabilities.is_empty() {
                println!(
                    "    capabilities: (none — unexpected; 6d-ii populates ports + processes always)"
                );
            } else {
                println!("    capabilities: {}", capabilities.join(", "));
            }
            println!();
        }
        Payload::Error(info) => {
            bail!("agent returned Error({:?}): {}", info.kind, info.message);
        }
        other => bail!("expected AgentHandshakeResponse, got {other:?}"),
    }
    let _ = std::io::stdout().flush();
    Ok(())
}
