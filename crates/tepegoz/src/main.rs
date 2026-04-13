//! Tepegöz — god view for your fleet.
//!
//! Single binary with subcommands for daemon, TUI, remote connect, agent, and doctor modes.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "tepegoz",
    about = "Tepegöz — god view for your fleet",
    version,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Default tracing directive when `RUST_LOG` is unset.
    #[arg(long, global = true, default_value = "info")]
    log_level: String,
}

#[derive(Subcommand)]
enum Command {
    /// Run the headless daemon.
    Daemon {
        /// Override the Unix socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Attach the TUI to a running daemon.
    Tui {
        /// Override the daemon socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Open an SSH pane to a host via the running daemon.
    Connect {
        /// Target, e.g. `user@host[:port]`.
        target: String,
    },

    /// Run as a remote agent (launched by the daemon over SSH).
    Agent {
        /// Speak the protocol over stdin/stdout.
        #[arg(long)]
        stdio: bool,
    },

    /// Diagnose environment, config, and connectivity.
    Doctor {
        /// Dump the detected Claude Code layout signature.
        #[arg(long)]
        claude_layout: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level);

    match cli.command {
        Command::Daemon { socket } => {
            tracing::info!(?socket, "daemon mode — scaffold only");
        }
        Command::Tui { socket } => {
            tracing::info!(?socket, "tui mode — scaffold only");
        }
        Command::Connect { target } => {
            tracing::info!(%target, "connect mode — scaffold only");
        }
        Command::Agent { stdio } => {
            tracing::info!(stdio, "agent mode — scaffold only");
        }
        Command::Doctor { claude_layout } => {
            tracing::info!(claude_layout, "doctor mode — scaffold only");
        }
    }

    Ok(())
}

fn init_tracing(default_level: &str) {
    use tracing_subscriber::EnvFilter;

    let default_directive = default_level
        .parse()
        .unwrap_or_else(|_| tracing::Level::INFO.into());

    let filter = EnvFilter::builder()
        .with_default_directive(default_directive)
        .with_env_var("RUST_LOG")
        .from_env_lossy();

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
