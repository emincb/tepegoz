//! Tepegöz daemon engine — state store, event bus, and command dispatch.
//!
//! The daemon binds a Unix socket, accepts clients, and serves the
//! [`tepegoz-proto`] wire protocol. State lives here; the TUI (and later,
//! web/phone clients and the AI orchestrator) are clients that observe and
//! act on this state through the protocol.

mod agent;
mod client;
mod config;
mod daemon;
mod remote_pane;
mod state;

pub use config::{AgentResolver, DaemonConfig};
pub use daemon::{run_daemon, run_daemon_with_resolver};
