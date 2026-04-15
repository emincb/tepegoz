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

    // --- Phase 6 Slice 6b: agent deploy errors -------------------------
    /// Remote host reported an OS / arch combination that doesn't map
    /// to one of the four tepegoz-agent target triples (Decision #3).
    /// The `os` + `arch` fields carry the raw `uname -sm` tokens so
    /// the diagnostic is traceable; the `supported` list enumerates
    /// what we DO recognise.
    #[error(
        "unsupported remote platform: os={os} arch={arch}. \
         Tepegöz supports: {}",
        supported.join(", ")
    )]
    UnsupportedPlatform {
        os: String,
        arch: String,
        supported: Vec<String>,
    },

    /// Remote platform resolved cleanly to a target triple, but the
    /// controller binary doesn't carry an embedded agent for it. Most
    /// likely cause: developer ran `cargo build` without first
    /// running `cargo xtask build-agents`, so `target/agents/<triple>/`
    /// was empty at controller compile time and `build.rs` populated
    /// the slot with `None`.
    #[error(
        "tepegoz-agent binary for target {triple} is not embedded in this controller build. \
         Rebuild with `cargo xtask build-agents` (requires zig + cargo-zigbuild) and retry."
    )]
    AgentNotEmbedded { triple: String },

    /// Something in the upload pipeline failed — `mkdir -p`, `cat >`
    /// write, `mv` atomic-rename, `chmod +x`, or `sha256sum` verify.
    /// `stage` names the step so the user can tell which command on
    /// the remote misbehaved.
    #[error("agent deploy failed at stage '{stage}': {reason}")]
    DeployFailed { stage: String, reason: String },

    /// Post-transfer checksum didn't match what we hashed locally.
    /// First occurrence triggers a single redeploy; a second mismatch
    /// surfaces as this error (CTO brief: one retry, then terminal).
    #[error(
        "agent checksum mismatch at {remote_path}: expected sha256 {expected}, \
         got {actual}. Partial transfer or corruption on the remote filesystem."
    )]
    ChecksumMismatch {
        remote_path: String,
        expected: String,
        actual: String,
    },

    /// Agent's runtime-reported `PROTOCOL_VERSION` differs from what
    /// the controller's build.rs embedded. Terminal by design —
    /// version drift means the wire contract is out of sync and no
    /// retry will recover (CTO brief: don't retry protocol drift).
    #[error(
        "agent reported wire protocol v{reported}, controller expects v{embedded}. \
         Redeploy with `cargo xtask build-agents` + rebuild the controller, \
         or confirm no stale `~/.cache/tepegoz/agent-v*` binaries are shadowing the new deploy."
    )]
    AgentVersionMismatch { embedded: u32, reported: u32 },
}

impl From<std::io::Error> for SshError {
    fn from(e: std::io::Error) -> Self {
        SshError::Io(e.to_string())
    }
}
