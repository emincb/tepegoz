//! Shared daemon state.
//!
//! Counters are atomic so client tasks sample without lock contention. The
//! pty manager owns any live ptys and their scrollback buffers.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tepegoz_proto::StatusSnapshot;
use tepegoz_pty::PtyManager;
use tokio::sync::Mutex as AsyncMutex;

use crate::agent::AgentConnection;
use crate::config::AgentResolver;
use crate::remote_pane::RemotePaneManager;

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct SharedState {
    pub started_at: Instant,
    pub started_at_unix_millis: u64,
    pub clients_now: AtomicU32,
    pub clients_total: AtomicU64,
    pub events_sent: AtomicU64,
    pub daemon_pid: u32,
    pub socket_path: PathBuf,
    pub pty: PtyManager,
    /// Phase 6 Slice 6c-proper: resolver for embedded agent binaries.
    /// `None` when no `cargo xtask build-agents` step ran — Fleet
    /// supervisor's deploy step short-circuits with a single warning,
    /// heartbeat still runs, remote scopes surface DockerUnavailable.
    pub agent_resolver: Option<AgentResolver>,
    /// Phase 5 Slice 5d-i: remote pty panes (SSH-backed). Parallel to
    /// `pty` so existing local-pty code paths stay unchanged; the
    /// daemon's command handlers check `remote_pty` first for a given
    /// `PaneId` and fall through to `pty` when not found.
    pub remote_pty: RemotePaneManager,
    /// Phase 6 Slice 6c-proper: pool of live remote agent connections,
    /// keyed by Fleet alias. Populated by the Fleet supervisor on
    /// `HostState::Connected` after deploy + handshake succeed;
    /// removed on any transition out of Connected. Client-side
    /// `Subscribe(Docker { Remote { alias } })` handling looks up the
    /// Arc here, registers a routing entry, and forwards the
    /// subscription through the agent's stdio tunnel.
    pub agent_conns: AsyncMutex<HashMap<String, Arc<AgentConnection>>>,
}

impl SharedState {
    pub fn new(socket_path: PathBuf, agent_resolver: Option<AgentResolver>) -> Self {
        let started_at_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();

        Self {
            started_at: Instant::now(),
            started_at_unix_millis,
            clients_now: AtomicU32::new(0),
            clients_total: AtomicU64::new(0),
            events_sent: AtomicU64::new(0),
            daemon_pid: std::process::id(),
            socket_path,
            pty: PtyManager::new(),
            remote_pty: RemotePaneManager::new(),
            agent_resolver,
            agent_conns: AsyncMutex::new(HashMap::new()),
        }
    }

    pub async fn snapshot(&self) -> StatusSnapshot {
        let local = u32::try_from(self.pty.count().await).unwrap_or(u32::MAX);
        let remote = u32::try_from(self.remote_pty.count().await).unwrap_or(u32::MAX);
        let panes_open = local.saturating_add(remote);
        StatusSnapshot {
            daemon_pid: self.daemon_pid,
            daemon_version: DAEMON_VERSION.to_string(),
            started_at_unix_millis: self.started_at_unix_millis,
            uptime_seconds: self.started_at.elapsed().as_secs(),
            clients_now: self.clients_now.load(Ordering::Relaxed),
            clients_total: self.clients_total.load(Ordering::Relaxed),
            events_sent: self.events_sent.load(Ordering::Relaxed),
            socket_path: self.socket_path.to_string_lossy().into_owned(),
            panes_open,
        }
    }
}
