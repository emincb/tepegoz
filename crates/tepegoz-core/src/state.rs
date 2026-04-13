//! Shared daemon state.
//!
//! Kept atomic so client tasks sample without lock contention.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tepegoz_proto::StatusSnapshot;

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct SharedState {
    pub started_at: Instant,
    pub started_at_unix_millis: u64,
    pub clients_now: AtomicU32,
    pub clients_total: AtomicU64,
    pub events_sent: AtomicU64,
    pub daemon_pid: u32,
    pub socket_path: PathBuf,
}

impl SharedState {
    pub fn new(socket_path: PathBuf) -> Self {
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
        }
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            daemon_pid: self.daemon_pid,
            daemon_version: DAEMON_VERSION.to_string(),
            started_at_unix_millis: self.started_at_unix_millis,
            uptime_seconds: self.started_at.elapsed().as_secs(),
            clients_now: self.clients_now.load(Ordering::Relaxed),
            clients_total: self.clients_total.load(Ordering::Relaxed),
            events_sent: self.events_sent.load(Ordering::Relaxed),
            socket_path: self.socket_path.to_string_lossy().into_owned(),
        }
    }
}
