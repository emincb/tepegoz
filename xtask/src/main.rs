//! Custom cargo tasks for Tepegöz: agent cross-compile, packaging,
//! release, and the per-phase manual-demo runners.

use clap::{Parser, Subcommand};

mod demo;

#[derive(Parser)]
#[command(name = "xtask", about = "Tepegöz build tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Cross-compile the agent for all target triples into `target/agents/`.
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
        Command::BuildAgents => {
            println!("xtask build-agents: not yet implemented");
        }
        Command::Package => {
            println!("xtask package: not yet implemented");
        }
        Command::DemoPhase5 { action } => match action {
            DemoAction::Up => demo::up()?,
            DemoAction::Down => demo::down()?,
        },
    }
    Ok(())
}
