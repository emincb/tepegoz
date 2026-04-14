//! OS-split paths per `docs/ARCHITECTURE.md §5`.
//!
//! - Linux: `$XDG_CONFIG_HOME` for config, `$XDG_DATA_HOME` for data.
//! - macOS: `~/Library/Application Support/tepegoz/` for both (Apple
//!   groups config + data under the same application bundle dir).
//!
//! The known_hosts file at `data_dir/tepegoz/known_hosts` is tepegoz-
//! owned — **never** the user's `~/.ssh/known_hosts`. Tepegöz's SSH is
//! additive to the user's OpenSSH state, not destructive to it.

use std::path::PathBuf;

use crate::error::SshError;

/// Path to the tepegoz `config.toml`. `None` when no home directory can
/// be resolved (headless environments without `$HOME`).
pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("tepegoz").join("config.toml"))
}

/// Path to the tepegoz-owned known_hosts file used for host-key TOFU.
pub fn known_hosts_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("tepegoz").join("known_hosts"))
}

/// Path to the user's `~/.ssh/config`.
pub fn ssh_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".ssh").join("config"))
}

pub(crate) fn require_known_hosts_path() -> Result<PathBuf, SshError> {
    known_hosts_path().ok_or_else(|| {
        SshError::PathResolution("could not resolve platform data_dir for known_hosts".into())
    })
}
