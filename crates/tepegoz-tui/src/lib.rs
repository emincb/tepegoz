//! Tepegöz TUI client.
//!
//! Connects to the daemon over its local Unix socket, subscribes to the
//! status stream, and renders a live "daemon heartbeat" panel. The TUI owns
//! no domain state — on reconnect, everything comes from the daemon.

use std::path::PathBuf;

use tokio::sync::mpsc;

mod app;
mod config;
mod net;
mod terminal;
mod ui;

pub use config::TuiConfig;

use app::{App, AppEvent, ConnectionState};

pub async fn run(config: TuiConfig) -> anyhow::Result<()> {
    init_tui_tracing(&config.log_level)?;

    let socket_path = config
        .socket_path
        .clone()
        .unwrap_or_else(tepegoz_proto::socket::default_socket_path);

    if !socket_path.exists() {
        anyhow::bail!(
            "no daemon socket at {} — is `tepegoz daemon` running?",
            socket_path.display()
        );
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    let conn_handle = {
        let conn_tx = tx.clone();
        let conn_path = socket_path.clone();
        tokio::spawn(async move {
            net::run_connection(conn_path, conn_tx).await;
        })
    };
    let input_handle = {
        let input_tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            net::input_loop(&input_tx);
        })
    };

    // Drop our root tx so rx.recv() ends once both task clones drop.
    drop(tx);

    let mut terminal = terminal::setup()?;
    let _guard = terminal::TerminalGuard;
    let mut app = App::new();

    loop {
        terminal.draw(|f| ui::render(f, &app))?;
        match rx.recv().await {
            Some(AppEvent::Key(code)) => {
                use crossterm::event::KeyCode;
                if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                    break;
                }
            }
            Some(AppEvent::ConnectionState(s)) => app.connection = s,
            Some(AppEvent::Status(snap)) => app.last_status = Some(snap),
            Some(AppEvent::ConnectionLost(reason)) => {
                app.connection = ConnectionState::Disconnected(reason);
                terminal.draw(|f| ui::render(f, &app))?;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                break;
            }
            Some(AppEvent::Redraw) => {}
            None => break,
        }
    }

    conn_handle.abort();
    let _ = conn_handle.await;
    input_handle.abort();
    let _ = input_handle.await;

    Ok(())
}

fn init_tui_tracing(default_level: &str) -> anyhow::Result<()> {
    use tracing_subscriber::EnvFilter;

    let log_path = resolve_log_path();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Open once up front so we fail loudly if the path is unwritable.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let writer_path = log_path.clone();
    let default_directive = default_level
        .parse()
        .unwrap_or_else(|_| tracing::Level::INFO.into());
    let filter = EnvFilter::builder()
        .with_default_directive(default_directive)
        .with_env_var("RUST_LOG")
        .from_env_lossy();

    tracing_subscriber::fmt()
        .with_writer(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&writer_path)
                .unwrap_or_else(|_| std::fs::File::create("/dev/null").expect("null sink"))
        })
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    tracing::info!(log_path = %log_path.display(), "tepegoz tui starting");
    Ok(())
}

fn resolve_log_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TEPEGOZ_LOG_FILE") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("tepegoz").join("tui.log")
}
