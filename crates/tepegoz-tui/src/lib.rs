//! Tepegöz TUI client.
//!
//! Two view modes:
//!
//! - **Pane** (default): raw-passthrough attach to a pty pane. Stdin →
//!   `SendInput`, `PaneOutput` → stdout, `SIGWINCH` → `ResizePane`. The
//!   user's real terminal emulator does the ANSI rendering — no vt100
//!   parsing on our side.
//! - **Scope**: ratatui-rendered scope panel. Slice C ships the Docker
//!   panel (container list, lifecycle actions, logs streaming).
//!
//! View switching: `Ctrl-b s` enters scope view; `Ctrl-b a` returns to the
//! attached pane; `Ctrl-b d`/`Ctrl-b q` detaches from either view.
//!
//! Architecture: a pure [`app::App`] state machine handles every event
//! ([`app::AppEvent`]) and emits [`app::AppAction`]s; the
//! [`session::AppRuntime`] executes those actions against the daemon
//! socket, stdin/stdout, and ratatui's terminal. State-machine tests live
//! in `app::tests`; the runtime is exercised end-to-end by the scope
//! integration test in `tepegoz-core/tests/`.

use std::path::PathBuf;

mod app;
mod config;
mod input;
mod scope;
mod session;
mod terminal;
mod tracing_setup;

pub use config::TuiConfig;

use tepegoz_proto::socket::default_socket_path;

pub async fn run(config: TuiConfig) -> anyhow::Result<()> {
    // Refuse to recursively attach from inside a tepegoz-managed pane.
    // The daemon stamps `TEPEGOZ_PANE_ID` into every pty it spawns; if that
    // var is present, running another `tepegoz tui` here would feed the
    // pane's output back into itself via two subscribers on the same
    // broadcast — infinite loop, scrambled stdin, garbled terminal.
    if let Ok(pane_id) = std::env::var("TEPEGOZ_PANE_ID") {
        anyhow::bail!(
            "this shell is already inside tepegoz pane {pane_id}. \
             Detach first (Ctrl-b d) and run `tepegoz tui` from your outer terminal."
        );
    }

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
