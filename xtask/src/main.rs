//! Custom cargo tasks for Tepegöz: agent cross-compile, packaging,
//! release, and the per-phase manual-demo runners.

use clap::{Parser, Subcommand};

mod build_agents;
mod build_release;
mod demo;
mod demo_phase_6;
mod preflight;

#[derive(Parser)]
#[command(name = "xtask", about = "Tepegöz build tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Cross-compile the agent for all target triples into
    /// `target/agents/<triple>/tepegoz-agent`, with a `manifest.json`
    /// sidecar carrying the compiled-in `PROTOCOL_VERSION`.
    ///
    /// Consumed by `crates/tepegoz/build.rs`: any populated target
    /// is embedded via `include_bytes!`, and the manifest version is
    /// asserted against the proto `PROTOCOL_VERSION` text file at
    /// controller compile time. Mismatch is a hard build failure.
    ///
    /// Requires `zig` + `cargo-zigbuild` on PATH; plain cargo can't
    /// cross-link a Darwin SDK from a Linux host (or vice versa).
    BuildAgents,
    /// Cross-compile the `tepegoz` controller binary for all four
    /// Decision #3 target triples, produce a universal macOS binary
    /// via `lipo` (optional — warn-and-skip when absent), and emit a
    /// `SHA256SUMS` index across every artifact into
    /// `target/release-bundles/`. Feeds v1.0 Slice R2's GitHub
    /// Actions release workflow (upload glob) and R3's install
    /// script (checksum verification).
    ///
    /// Internally invokes `build-agents` first — the controller's
    /// `build.rs` drift check `include_bytes!`'s populated agent
    /// arches at compile time; skipping this step silently ships a
    /// binary that can't deploy agents on remote hosts.
    BuildRelease,
    /// Package release artifacts with checksums and minisign signatures.
    Package,
    /// One-command runner for the Phase 5 Slice 5e manual demo.
    ///
    /// `up` provisions an sshd container + throwaway tepegoz config +
    /// keypair, builds the workspace, spawns the daemon against isolated
    /// config/data dirs, waits for readiness, then blocks on Ctrl-C.
    /// `down` tears it all back down (idempotent).
    #[command(name = "demo-phase-5")]
    DemoPhase5 {
        #[command(subcommand)]
        action: DemoAction,
    },
    /// Phase 6 Slice 6a local handshake demo — builds `tepegoz-agent`
    /// for the host target, spawns it as a subprocess, drives a
    /// single `AgentHandshake` envelope, prints the response. With
    /// `--remote` (Slice 6b), spawns an sshd container, cross-
    /// compiles the agent for `x86_64-unknown-linux-musl`, deploys
    /// via tepegoz-ssh + verifies sha256 + handshakes over the
    /// exec channel.
    #[command(name = "demo-phase-6")]
    DemoPhase6 {
        #[command(subcommand)]
        action: Demo6Action,
    },
}

#[derive(Subcommand)]
enum DemoAction {
    /// Bring the demo fixture up and wait for Ctrl-C.
    Up,
    /// Tear the demo fixture down (idempotent).
    Down,
}

#[derive(Subcommand)]
enum Demo6Action {
    /// Bring the demo up (local subprocess handshake by default;
    /// use `--remote` for the Slice 6b SSH deploy + handshake flow).
    Up {
        /// Switch to Slice 6b's full remote deploy scenario:
        /// sshd container + cross-compile + tepegoz-ssh deploy +
        /// handshake over the exec channel. Requires docker,
        /// ssh-keygen, and either cargo-zigbuild (cross-platform)
        /// or a host that can natively build
        /// `x86_64-unknown-linux-musl` (Linux + musl target).
        #[arg(long)]
        remote: bool,
    },
    /// Tear the demo fixture down (idempotent).
    Down {
        /// Use Slice 6b's teardown: `docker rm -f` the sshd
        /// container in addition to removing the tempdir. Harmless
        /// if the container was never spawned.
        #[arg(long)]
        remote: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::BuildAgents => build_agents::run()?,
        Command::BuildRelease => build_release::run()?,
        Command::Package => {
            println!("xtask package: not yet implemented");
        }
        Command::DemoPhase5 { action } => match action {
            DemoAction::Up => demo::up()?,
            DemoAction::Down => demo::down()?,
        },
        Command::DemoPhase6 { action } => match action {
            Demo6Action::Up { remote } => demo_phase_6::up(remote)?,
            Demo6Action::Down { remote } => demo_phase_6::down(remote)?,
        },
    }
    Ok(())
}
