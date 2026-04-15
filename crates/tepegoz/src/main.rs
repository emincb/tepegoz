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
        /// Observation-only report of the remote-agent deploy state
        /// across every Fleet host (Phase 6 Slice 6b). For each
        /// host: connect via SSH → detect OS/arch over `uname -sm` →
        /// look up the matching `embedded_agents` blob → inspect
        /// `~/.cache/tepegoz/agent-v<N>` on the remote → report
        /// present/absent + SHA256 match against the embedded bytes.
        /// Does NOT deploy; reports what a fresh `tepegoz connect`
        /// (or Phase 6 Slice 6c+'s remote subscription) would do.
        /// Per-host errors are collected and logged — the command
        /// keeps going through the rest of the fleet.
        #[arg(long)]
        agents: bool,
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
            agents,
        } => {
            init_stdout_tracing(&cli.log_level);
            if let Some(alias) = ssh_forget {
                forget_ssh_host(&alias)
            } else if ssh_hosts {
                dump_ssh_hosts()
            } else if agents {
                dump_agents().await
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

/// `tepegoz doctor --agents` — Phase 6 Slice 6b observation of the
/// remote-agent deploy state across the Fleet. For each host:
/// connect → detect OS/arch → look up embedded blob for that triple
/// → resolve the remote deploy path → inspect + compare SHA256
/// against embedded bytes → print one row. Non-fatal per-host errors
/// print inline and iteration continues (a single unreachable host
/// doesn't void the rest of the report — same `--ssh-hosts` philosophy
/// of showing what it can show and flagging what it can't).
async fn dump_agents() -> anyhow::Result<()> {
    let list = tepegoz_ssh::HostList::discover()
        .map_err(|e| anyhow::anyhow!("ssh host discovery failed: {e}"))?;
    let store = tepegoz_ssh::KnownHostsStore::open()
        .map_err(|e| anyhow::anyhow!("open known_hosts: {e}"))?;

    println!("source: {}", list.source.label());
    println!("agents ({} host(s)):", list.hosts.len());
    if list.hosts.is_empty() {
        println!(
            "  (none) — add entries to ~/.ssh/config or set \
             TEPEGOZ_SSH_HOSTS=<alias>,<alias>,..."
        );
        return Ok(());
    }

    let protocol_version = tepegoz_proto::PROTOCOL_VERSION;

    for host in &list.hosts {
        if let Err(e) = report_one_host(host, &list, &store, protocol_version).await {
            println!("  {alias:<20}  ✗ {e}", alias = host.alias);
        }
    }
    Ok(())
}

async fn report_one_host(
    host: &tepegoz_ssh::HostEntry,
    list: &tepegoz_ssh::HostList,
    store: &tepegoz_ssh::KnownHostsStore,
    protocol_version: u32,
) -> anyhow::Result<()> {
    let session = tepegoz_ssh::connect_host(&host.alias, list, store)
        .await
        .map_err(|e| anyhow::anyhow!("connect failed: {}", summarize_ssh_error(&e)))?;

    // Whatever we learn, we always disconnect cleanly before
    // returning — RAII is fine here because disconnect is fire-
    // and-forget.
    let outcome = report_one_host_inner(host, &session, protocol_version).await;
    let _ = session.disconnect().await;
    outcome
}

async fn report_one_host_inner(
    host: &tepegoz_ssh::HostEntry,
    session: &tepegoz_ssh::SshSession,
    protocol_version: u32,
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};

    let target = tepegoz_ssh::detect_target(session)
        .await
        .map_err(|e| anyhow::anyhow!("detect failed: {e}"))?;

    let bytes = match agents::embedded_agents::for_target(&target.target_triple) {
        Some(b) => b,
        None => {
            println!(
                "  {alias:<20}  {triple:<32}  ⚠ no embedded agent — run `cargo xtask build-agents`",
                alias = host.alias,
                triple = target.target_triple,
            );
            return Ok(());
        }
    };

    let local_sha = hex::encode(Sha256::digest(bytes));
    let path = tepegoz_ssh::remote_agent_path(session, protocol_version)
        .await
        .map_err(|e| anyhow::anyhow!("remote path resolution failed: {e}"))?;
    let status = tepegoz_ssh::inspect_remote_agent(session, &path, &local_sha, &target)
        .await
        .map_err(|e| anyhow::anyhow!("inspect failed: {e}"))?;

    match status {
        tepegoz_ssh::RemoteAgentStatus::Absent => {
            println!(
                "  {alias:<20}  {triple:<32}  ✗ absent — would deploy on next connect",
                alias = host.alias,
                triple = target.target_triple,
            );
        }
        tepegoz_ssh::RemoteAgentStatus::Present {
            sha256_hex,
            matches_expected,
            size_bytes,
            mtime_unix_secs,
        } => {
            let (glyph, fate) = if matches_expected {
                ("✓", "matches embedded")
            } else {
                ("⚠", "drift — redeploy needed")
            };
            println!(
                "  {alias:<20}  {triple:<32}  {glyph} {fate}",
                alias = host.alias,
                triple = target.target_triple,
            );
            println!("    {path} ({size_bytes} bytes, mtime {mtime_unix_secs})");
            println!(
                "    remote   sha256: {sha}",
                sha = &sha256_hex[..16.min(sha256_hex.len())]
            );
            if !matches_expected {
                println!("    embedded sha256: {sha}", sha = &local_sha[..16]);
            }
        }
    }
    Ok(())
}

fn summarize_ssh_error(e: &tepegoz_ssh::SshError) -> String {
    match e {
        tepegoz_ssh::SshError::ConnectFailed { reason, .. } => reason.clone(),
        tepegoz_ssh::SshError::AuthFailed { reason, .. } => format!("auth: {reason}"),
        tepegoz_ssh::SshError::HostKeyMismatch { .. } => "host key mismatch".into(),
        other => format!("{other}"),
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
