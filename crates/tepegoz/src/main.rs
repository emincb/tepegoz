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

    match cli.command {
        Command::Daemon { socket } => {
            init_stdout_tracing(&cli.log_level);
            tepegoz_core::run_daemon(tepegoz_core::DaemonConfig {
                socket_path: socket,
            })
            .await
        }
        Command::Tui { socket } => {
            // TUI sets up its own file-backed tracing to avoid corrupting the display.
            tepegoz_tui::run(tepegoz_tui::TuiConfig {
                socket_path: socket,
                log_level: cli.log_level,
            })
            .await
        }
        Command::Connect { target } => {
            init_stdout_tracing(&cli.log_level);
            tracing::info!(%target, "connect mode — not yet implemented (Phase 5)");
            Ok(())
        }
        Command::Agent { stdio } => {
            init_stdout_tracing(&cli.log_level);
            tracing::info!(stdio, "agent mode — not yet implemented (Phase 6)");
            Ok(())
        }
        Command::Doctor { claude_layout } => {
            init_stdout_tracing(&cli.log_level);
            tracing::info!(claude_layout, "doctor mode — not yet implemented");
            Ok(())
        }
    }
}

fn init_stdout_tracing(default_level: &str) {
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
