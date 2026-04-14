//! SSH transport for Tepegöz — a russh-backed client with host
//! discovery, SSH agent / IdentityFile auth, and tepegoz-owned host-key
//! TOFU.
//!
//! Phase 5 Slice 5a deliverable: concrete API consumed by 5c's per-host
//! connection supervisor and 5d's remote-pty channel. No daemon
//! integration, no wire-protocol change. Daemon wiring lands in 5b
//! alongside the Fleet-tile + `tepegoz doctor --ssh-hosts` diagnostic.
//!
//! ## Typical flow
//!
//! ```ignore
//! use tepegoz_ssh::{HostList, KnownHostsStore, connect_host, open_session};
//!
//! let hosts = HostList::discover()?;
//! let known_hosts = KnownHostsStore::open()?;
//! let session = connect_host("staging", &hosts, &known_hosts).await?;
//! let channel = open_session(&session).await?;
//! // 5d: request_pty + request_shell on channel.into_inner()
//! ```
//!
//! ## Host-key TOFU
//!
//! Tepegöz maintains its own known_hosts file at
//! `data_dir/tepegoz/known_hosts` (Linux: `$XDG_DATA_HOME`, macOS:
//! `~/Library/Application Support`). **We do not touch
//! `~/.ssh/known_hosts`** — tepegöz's SSH is additive to the user's
//! OpenSSH state, not destructive to it. First-contact hosts are auto-
//! trusted and persisted; a subsequent mismatch rejects the connection
//! and surfaces
//! [`SshError::HostKeyMismatch`](crate::error::SshError::HostKeyMismatch).

pub mod config;
pub mod error;
pub mod known_hosts;
pub mod paths;
pub mod session;

pub use config::{HostEntry, HostList, HostSource};
pub use error::SshError;
pub use known_hosts::{HostKeyVerdict, KnownHostsStore};
pub use session::{SshChannel, SshSession, connect_host, open_session};
