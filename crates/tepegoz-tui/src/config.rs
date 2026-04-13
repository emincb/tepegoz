//! TUI configuration.

use std::path::PathBuf;

pub struct TuiConfig {
    /// Override the daemon socket path.
    pub socket_path: Option<PathBuf>,
    /// Default tracing directive when `RUST_LOG` is unset.
    pub log_level: String,
}
