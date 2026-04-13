//! Tepegöz TUI client.
//!
//! Single pane, raw-passthrough attach. The TUI connects to the daemon,
//! ensures a pane exists (creating a default shell if needed), attaches to
//! it, and thereafter:
//!
//! - pipes user keystrokes from stdin → daemon (`SendInput`)
//! - pipes `PaneOutput` events from the daemon → stdout directly
//! - handles SIGWINCH → `ResizePane`
//! - detects `Ctrl-b d` → detach (exit TUI, leave pane running)
//!
//! The user's real terminal emulator does the ANSI rendering. No vt100
//! parsing on our side — that's deferred until we need overlay chrome
//! (e.g. tiled layout).

use std::path::PathBuf;

mod config;
mod input;
mod session;
mod terminal;
mod tracing_setup;

pub use config::TuiConfig;

use tepegoz_proto::socket::default_socket_path;

pub async fn run(config: TuiConfig) -> anyhow::Result<()> {
    tracing_setup::init(&config.log_level)?;

    let socket_path = config
        .socket_path
        .clone()
        .unwrap_or_else(default_socket_path);

    if !socket_path.exists() {
        anyhow::bail!(
            "no daemon socket at {} — is `tepegoz daemon` running?",
            socket_path.display()
        );
    }

    session::run(socket_path).await
}

/// Compat re-export so callers can import `tepegoz_tui::resolve_socket_path`.
pub fn resolve_socket_path(override_path: Option<PathBuf>) -> PathBuf {
    override_path.unwrap_or_else(default_socket_path)
}
