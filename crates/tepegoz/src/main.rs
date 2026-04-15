//! Tepegöz — god view for your fleet.
//!
//! Single binary with subcommands for daemon, TUI, remote connect, agent, and doctor modes.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

mod agents;

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

    /// Open an SSH pane to a host alias and attach immediately —
    /// short-lived TUI, same god-view tile grid. Stack contains only
    /// the remote pane; `Ctrl-b d` detaches and exits. Unknown
    /// aliases / connection failures print to stderr and return a
    /// non-zero exit status.
    Connect {
        /// Fleet alias to open (resolved through the daemon's host
        /// list — same precedence as `tepegoz doctor --ssh-hosts`).
        alias: String,
        /// Override the daemon socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
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
        /// Dump the resolved SSH host list + source label (Phase 5
        /// Slice 5b). Shows the precedence layer that won
        /// (tepegoz config.toml / TEPEGOZ_SSH_HOSTS env / ssh_config /
        /// none) alongside each alias's hostname, user, port, and
        /// IdentityFile list. Use this when `tepegoz connect <alias>`
        /// can't find a host, or to verify an override is active.
        #[arg(long)]
        ssh_hosts: bool,
        /// Forget the tepegoz-owned host-key entry for an alias.
        /// Resolves the alias through the current host list, then
        /// removes matching entries from `known_hosts` (tepegoz's
        /// own, NOT `~/.ssh/known_hosts`). Recovery path after a
        /// `HostKeyMismatch` rejection — use only after verifying
        /// the key change is legitimate.
        #[arg(long, value_name = "ALIAS")]
        ssh_forget: Option<String>,
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
        Command::Connect { alias, socket } => {
            // TUI sets up its own file-backed tracing to avoid corrupting the display.
            tepegoz_tui::run_connect(
                tepegoz_tui::TuiConfig {
                    socket_path: socket,
                    log_level: cli.log_level,
                },
                alias,
            )
            .await
        }
        Command::Agent { stdio } => {
            // --stdio is the only mode today. Kept as a flag so
            // future variants (e.g. a local-socket transport for
            // co-located agent experiments) don't need a breaking
            // CLI rewrite. Tracing goes to stderr so it doesn't
            // corrupt the wire on stdout.
            init_stderr_tracing(&cli.log_level);
            if !stdio {
                anyhow::bail!(
                    "tepegoz agent: only --stdio is supported in Phase 6 Slice 6a. \
                     Re-invoke with --stdio, or run the standalone `tepegoz-agent` \
                     binary directly (same code path, same behavior)."
                );
            }
            tepegoz_agent::run_stdio().await
        }
        Command::Doctor {
            claude_layout,
            ssh_hosts,
            ssh_forget,
        } => {
            init_stdout_tracing(&cli.log_level);
            if let Some(alias) = ssh_forget {
                forget_ssh_host(&alias)
            } else if ssh_hosts {
                dump_ssh_hosts()
            } else {
                tracing::info!(claude_layout, "doctor mode — not yet implemented");
                Ok(())
            }
        }
    }
}

fn dump_ssh_hosts() -> anyhow::Result<()> {
    use tepegoz_ssh::HostList;
    let list =
        HostList::discover().map_err(|e| anyhow::anyhow!("ssh host discovery failed: {e}"))?;
    println!("source: {}", list.source.label());
    println!("hosts ({}):", list.hosts.len());
    if list.hosts.is_empty() {
        println!(
            "  (none) — add entries to ~/.ssh/config or set \
             TEPEGOZ_SSH_HOSTS=<alias>,<alias>,..."
        );
        return Ok(());
    }
    for host in &list.hosts {
        println!(
            "  {alias}  {user}@{hostname}:{port}",
            alias = host.alias,
            user = host.user,
            hostname = host.hostname,
            port = host.port,
        );
        if !host.identity_files.is_empty() {
            println!("    IdentityFile: {}", host.identity_files.join(", "));
        }
        if let Some(jump) = &host.proxy_jump {
            println!("    ProxyJump: {jump} (not supported in v1 — Slice 5c surfaces this)");
        }
    }
    Ok(())
}

fn forget_ssh_host(alias: &str) -> anyhow::Result<()> {
    use tepegoz_ssh::{HostList, KnownHostsStore};
    let hosts =
        HostList::discover().map_err(|e| anyhow::anyhow!("ssh host discovery failed: {e}"))?;
    let entry = hosts.get(alias).ok_or_else(|| {
        anyhow::anyhow!(
            "alias '{alias}' not found in host list (source: {})",
            hosts.source.label()
        )
    })?;
    let store = KnownHostsStore::open().map_err(|e| anyhow::anyhow!("open known_hosts: {e}"))?;
    let removed = store
        .forget(&entry.hostname, entry.port)
        .map_err(|e| anyhow::anyhow!("forget: {e}"))?;
    if removed == 0 {
        println!(
            "no entries for {}:{} in {}",
            entry.hostname,
            entry.port,
            store.path().display()
        );
    } else {
        println!(
            "removed {removed} entry(ies) for {}:{} from {} — \
             next connection to '{alias}' will re-TOFU the new key",
            entry.hostname,
            entry.port,
            store.path().display()
        );
    }
    Ok(())
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

/// Like [`init_stdout_tracing`] but routes log output to stderr. Used
/// by `tepegoz agent`, where stdout is reserved for the wire
/// protocol — any log line on stdout would corrupt the envelope
/// stream.
fn init_stderr_tracing(default_level: &str) {
    use tracing_subscriber::EnvFilter;

    let default_directive = default_level
        .parse()
        .unwrap_or_else(|_| tracing::Level::WARN.into());

    let filter = EnvFilter::builder()
        .with_default_directive(default_directive)
        .with_env_var("RUST_LOG")
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
