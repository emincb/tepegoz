//! Daemon main loop: socket binding, accept loop, graceful shutdown.

use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::signal;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::config::{AgentResolver, DaemonConfig};
use crate::state::SharedState;

/// Entry point for the default daemon path — no remote-agent
/// deployment. Tests + tooling that don't need Phase 6's remote
/// scopes call this; the controller's main.rs calls
/// [`run_daemon_with_resolver`] with its compile-time-embedded agent
/// binaries.
pub async fn run_daemon(config: DaemonConfig) -> anyhow::Result<()> {
    run_daemon_with_resolver(config, None).await
}

/// Entry point wired from `tepegoz::main` with the compile-time-
/// embedded `agents::embedded_agents::for_target` resolver. Populating
/// a resolver enables the Fleet supervisor's Phase 6 Slice 6c-proper
/// agent-deploy-on-Connect path; `None` is the test / tooling
/// fallback (remote subscriptions surface DockerUnavailable).
pub async fn run_daemon_with_resolver(
    config: DaemonConfig,
    agent_resolver: Option<AgentResolver>,
) -> anyhow::Result<()> {
    let is_default_path = config.socket_path.is_none();
    let socket_path = config
        .socket_path
        .unwrap_or_else(tepegoz_proto::socket::default_socket_path);

    prepare_socket_parent(&socket_path, is_default_path)?;
    evict_stale_socket(&socket_path).await?;

    let listener = UnixListener::bind(&socket_path)?;
    set_socket_perms(&socket_path)?;

    let state = Arc::new(SharedState::new(socket_path.clone(), agent_resolver));

    info!(
        pid = state.daemon_pid,
        version = crate::state::DAEMON_VERSION,
        socket = %socket_path.display(),
        "tepegoz daemon ready"
    );

    let mut clients: JoinSet<()> = JoinSet::new();

    let shutdown = async {
        let _ = signal::ctrl_c().await;
        info!("ctrl-c received, shutting down");
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let state = Arc::clone(&state);
                        clients.spawn(run_client(stream, state));
                    }
                    Err(e) => {
                        warn!(error = %e, "accept failed");
                    }
                }
            }
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&socket_path);

    // Brief grace period for clients to finish.
    let grace = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    if grace.is_err() {
        clients.abort_all();
    }

    info!("daemon stopped");
    Ok(())
}

async fn run_client(stream: UnixStream, state: Arc<SharedState>) {
    if let Err(e) = crate::client::handle_client(stream, state).await {
        if !is_expected_disconnect(&e) {
            warn!(error = %e, "client ended with error");
        }
    }
}

fn is_expected_disconnect(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::{BrokenPipe, ConnectionReset, UnexpectedEof};
            if matches!(io.kind(), UnexpectedEof | ConnectionReset | BrokenPipe) {
                return true;
            }
        }
    }
    false
}

fn prepare_socket_parent(socket_path: &std::path::Path, secure_parent: bool) -> anyhow::Result<()> {
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;

    // Only chmod the parent when it's the default, tepegoz-owned subdir.
    // Override paths may live in shared dirs we don't own (e.g. /tmp, /var/run).
    if secure_parent {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(parent)?.permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(parent, perms)?;
        }
    }
    Ok(())
}

async fn evict_stale_socket(socket_path: &std::path::Path) -> anyhow::Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }
    // If another daemon is listening, refuse to start.
    match tokio::net::UnixStream::connect(socket_path).await {
        Ok(_) => {
            anyhow::bail!(
                "another tepegoz daemon is already running at {}",
                socket_path.display()
            );
        }
        Err(_) => {
            std::fs::remove_file(socket_path)?;
            warn!(socket = %socket_path.display(), "removed stale socket");
            Ok(())
        }
    }
}

fn set_socket_perms(socket_path: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(socket_path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(socket_path, perms)?;
    }
    Ok(())
}
