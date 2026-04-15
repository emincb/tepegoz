//! Tepegöz TUI client — god-view tiled layout.
//!
//! Per `docs/DECISIONS.md#7`: fixed tiled layout, all scopes visible
//! simultaneously, focus moves between tiles via `Ctrl-b h/j/k/l` +
//! arrow keys. The pty tile renders through a [`vt100::Parser`]; scope
//! tiles render via per-kind ratatui renderers with a
//! `(state, Frame, Rect, focused)` signature. All subscriptions
//! (`AttachPane`, `Subscribe(Docker)`, later Ports / Fleet / Claude)
//! live concurrently for the life of the TUI process.
//!
//! Architecture: a pure [`app::App`] state machine handles every event
//! ([`app::AppEvent`]) and emits [`app::AppAction`]s; the
//! [`session::AppRuntime`] executes those actions against the daemon
//! socket, stdin, and ratatui's terminal. State-machine tests live in
//! `app::tests`; headless render tests in `scope::docker::tests`,
//! `scope::placeholder::tests`, and `pty_tile::tests`; end-to-end
//! integration in `tepegoz-core/tests/`.

use std::path::PathBuf;

mod app;
mod config;
mod help;
mod host_picker;
mod input;
mod mouse;
mod pty_tile;
mod scope;
mod session;
mod terminal;
mod tile;
mod toast;
mod tracing_setup;

pub use config::TuiConfig;

use tepegoz_proto::socket::default_socket_path;

pub async fn run(config: TuiConfig) -> anyhow::Result<()> {
    let socket_path = prepare_and_init(&config)?;
    session::run(socket_path).await
}

/// Launch the TUI with an initial `OpenPane { target: Remote { alias } }`
/// instead of the default local root — the implementation of
/// `tepegoz connect <alias>`. Stack contains only the remote pane at
/// startup; `Ctrl-b d` detaches and exits.
pub async fn run_connect(config: TuiConfig, alias: String) -> anyhow::Result<()> {
    let socket_path = prepare_and_init(&config)?;
    session::run_connect(socket_path, alias).await
}

fn prepare_and_init(config: &TuiConfig) -> anyhow::Result<PathBuf> {
    // Refuse to recursively attach from inside a tepegoz-managed pane.
    // The daemon stamps `TEPEGOZ_PANE_ID` into every pty it spawns; if
    // that var is present, running another `tepegoz tui` here would
    // feed the pane's output back into itself via two subscribers on
    // the same broadcast — infinite loop, scrambled stdin.
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

    Ok(socket_path)
}

/// Compat re-export so callers can import `tepegoz_tui::resolve_socket_path`.
pub fn resolve_socket_path(override_path: Option<PathBuf>) -> PathBuf {
    override_path.unwrap_or_else(default_socket_path)
}
