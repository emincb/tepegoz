//! Daemon configuration.

use std::path::PathBuf;

pub struct DaemonConfig {
    /// Override the default Unix socket path.
    pub socket_path: Option<PathBuf>,
}
