//! Error taxonomy for the tepegoz-ssh crate.
//!
//! The daemon's Fleet-tile rendering treats [`HostKeyMismatch`] and
//! [`AuthFailed`] as "loud" terminal states (⚠ red marker per Q6 of the
//! Phase 5 proposal); everything else is either a transient connect
//! failure or a config/path issue surfaced verbatim in a toast.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("host alias '{alias}' not found in host list (source: {source_label})")]
    UnknownAlias { alias: String, source_label: String },

    #[error("ssh_config parse error at {path}: {reason}", path = path.display())]
    ConfigParse { path: PathBuf, reason: String },

    #[error("tepegoz config.toml error at {path}: {reason}", path = path.display())]
    TepegozConfig { path: PathBuf, reason: String },

    #[error("known_hosts error at {path}: {reason}", path = path.display())]
    KnownHosts { path: PathBuf, reason: String },

    #[error(
        "host key mismatch for {alias} ({hostname}:{port}): stored key at \
         {path}:{line} differs from the presented key. TOFU rejected this \
         connection; recover with `tepegoz doctor --ssh-forget {alias}` \
         after confirming the change is legitimate.",
        path = path.display()
    )]
    HostKeyMismatch {
        alias: String,
        hostname: String,
        port: u16,
        path: PathBuf,
        line: usize,
    },

    #[error("authentication failed for {alias} ({user}@{hostname}:{port}): {reason}")]
    AuthFailed {
        alias: String,
        user: String,
        hostname: String,
        port: u16,
        reason: String,
    },

    #[error("connection failed to {alias} ({hostname}:{port}): {reason}")]
    ConnectFailed {
        alias: String,
        hostname: String,
        port: u16,
        reason: String,
    },

    #[error("path resolution failed: {0}")]
    PathResolution(String),

    #[error("i/o error: {0}")]
    Io(String),
}

impl From<std::io::Error> for SshError {
    fn from(e: std::io::Error) -> Self {
        SshError::Io(e.to_string())
    }
}
