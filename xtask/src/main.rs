//! Custom cargo tasks for Tepegöz: agent cross-compile, packaging, release.

use clap::{Parser, Subcommand};

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
    }
    Ok(())
}
