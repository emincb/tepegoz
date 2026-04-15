//! Custom cargo tasks for Tepegöz: agent cross-compile, packaging,
//! release, and the per-phase manual-demo runners.

use clap::{Parser, Subcommand};

mod build_agents;
mod demo;
mod demo_phase_6;

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
    /// Phase 6 Slice 6a local handshake demo. Builds `tepegoz-agent`
    /// for the host target, spawns it as a subprocess, drives a
    /// single `AgentHandshake` envelope, prints the response.
    #[command(name = "demo-phase-6")]
    DemoPhase6 {
        #[command(subcommand)]
        action: DemoAction,
    },
}

#[derive(Subcommand)]
enum DemoAction {
    /// Bring the demo fixture up and wait for Ctrl-C.
    Up,
    /// Tear the demo fixture down (idempotent).
    Down,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::BuildAgents => build_agents::run()?,
        Command::Package => {
            println!("xtask package: not yet implemented");
        }
        Command::DemoPhase5 { action } => match action {
            DemoAction::Up => demo::up()?,
            DemoAction::Down => demo::down()?,
        },
        Command::DemoPhase6 { action } => match action {
            DemoAction::Up => demo_phase_6::up()?,
            DemoAction::Down => demo_phase_6::down()?,
        },
    }
    Ok(())
}
