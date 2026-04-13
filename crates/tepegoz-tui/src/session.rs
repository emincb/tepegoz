//! Session entry point + I/O runtime for the god-view TUI.
//!
//! [`run`] connects to the daemon, performs the handshake, ensures a
//! pty pane exists, then hands off to [`AppRuntime::run`]. The runtime
//! owns the event loop (stdin, daemon envelopes, SIGWINCH, 30 Hz tick)
//! and executes whatever [`AppAction`]s [`App::handle_event`] emits.
//!
//! The runtime is intentionally thin — every interesting state
//! transition lives in [`crate::app`] and is unit-tested there. This
//! file's job is to correctly wire bytes between sockets, terminals,
//! and ratatui.
//!
//! Per Decision #7: always-on ratatui. No mode switching. The runtime
//! always draws the tile grid on every `DrawFrame` and clears on exit.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use tepegoz_proto::{
    Envelope, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

use crate::app::{App, AppAction, AppEvent, DetachReason, ScopeKind, ToastKind};
use crate::pty_tile;
use crate::scope;
use crate::terminal;
use crate::tile::{TileDef, TileId, TileKind};

/// Redraw cadence. `DrawFrame` actions from the App coalesce through
/// ratatui's buffer diff, but the tick also drives the pty tile
/// redraw when bytes arrive between ticks — without it a steady
/// stream of small PaneOutputs would drive the screen at whatever
/// pace the daemon produces them.
const TICK_INTERVAL: Duration = Duration::from_millis(33);

pub(crate) async fn run(socket_path: PathBuf) -> anyhow::Result<()> {
    let stream = UnixStream::connect(&socket_path).await?;
    info!(socket = %socket_path.display(), "connected");
    let (mut reader, mut writer) = stream.into_split();

    handshake(&mut reader, &mut writer).await?;

    let (cols, rows) = terminal_size_fallback();
    let pane = ensure_pane(&mut reader, &mut writer, rows, cols).await?;
    info!(pane_id = pane.id, alive = pane.alive, "attaching to pane");

    terminal::enter_raw(&format!("tepegoz · god view (pane {})", pane.id))?;
    let _guard = terminal::TerminalGuard;

    let exit_reason = AppRuntime::new(reader, writer, pane.id, (rows, cols))?
        .run()
        .await;

    drop(_guard);
    print_exit_message(pane.id, exit_reason);
    Ok(())
}

async fn handshake(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> anyhow::Result<()> {
    let hello = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Hello(Hello {
            client_version: PROTOCOL_VERSION,
            client_name: "tepegoz-tui".to_string(),
        }),
    };
    write_envelope(writer, &hello).await?;

    let welcome = read_envelope(reader).await?;
    match welcome.payload {
        Payload::Welcome(w) => {
            debug!(
                daemon_pid = w.daemon_pid,
                daemon_version = %w.daemon_version,
                protocol = w.protocol_version,
                "handshake complete"
            );
            Ok(())
        }
        Payload::Error(e) => {
            anyhow::bail!("daemon refused handshake: {} ({:?})", e.message, e.kind)
        }
        other => anyhow::bail!("expected Welcome, got {other:?}"),
    }
}

async fn ensure_pane(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    rows: u16,
    cols: u16,
) -> anyhow::Result<PaneInfo> {
    write_envelope(
        writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::ListPanes,
        },
    )
    .await?;

    let env = read_envelope(reader).await?;
    let panes = match env.payload {
        Payload::PaneList { panes } => panes,
        other => anyhow::bail!("expected PaneList, got {other:?}"),
    };

    if let Some(alive) = panes.into_iter().find(|p| p.alive) {
        return Ok(alive);
    }

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok());
    write_envelope(
        writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(OpenPaneSpec {
                shell: None,
                cwd,
                env: Vec::new(),
                rows,
                cols,
            }),
        },
    )
    .await?;

    let env = read_envelope(reader).await?;
    match env.payload {
        Payload::PaneOpened(info) => Ok(info),
        Payload::Error(e) => anyhow::bail!("open pane failed: {} ({:?})", e.message, e.kind),
        other => anyhow::bail!("expected PaneOpened, got {other:?}"),
    }
}

/// Owns the I/O machinery the App needs: socket halves, a writer
/// mpsc, the stdin reader, the SIGWINCH stream, and the ratatui
/// Terminal.
struct AppRuntime {
    app: App,
    reader: tokio::net::unix::OwnedReadHalf,
    cmd_tx: mpsc::UnboundedSender<Envelope>,
    writer_handle: tokio::task::JoinHandle<()>,
    terminal: ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
}

#[derive(Debug)]
enum ExitReason {
    UserDetach,
    PaneExited { exit_code: Option<i32> },
    DaemonClosed(String),
    DaemonError(String),
    StdinClosed,
    StdinError(String),
    AppError(String),
}

impl AppRuntime {
    fn new(
        reader: tokio::net::unix::OwnedReadHalf,
        writer: tokio::net::unix::OwnedWriteHalf,
        pane_id: tepegoz_proto::PaneId,
        terminal_size: (u16, u16),
    ) -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Envelope>();
        let writer_handle = spawn_writer_task(writer, cmd_rx);

        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;

        Ok(Self {
            app: App::new(pane_id, terminal_size),
            reader,
            cmd_tx,
            writer_handle,
            terminal,
        })
    }

    async fn run(mut self) -> ExitReason {
        let bootstrap = self.app.initial_actions();
        if let Err(reason) = self.dispatch(bootstrap) {
            return reason;
        }

        let mut stdin = tokio::io::stdin();
        let mut stdin_buf = vec![0u8; 4096];
        let mut winch = match signal(SignalKind::window_change()) {
            Ok(s) => s,
            Err(e) => return ExitReason::AppError(format!("signal install: {e}")),
        };
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            let event = tokio::select! {
                n = stdin.read(&mut stdin_buf) => match n {
                    Ok(0) => return ExitReason::StdinClosed,
                    Ok(n) => AppEvent::StdinChunk(stdin_buf[..n].to_vec()),
                    Err(e) => return ExitReason::StdinError(e.to_string()),
                },
                env = read_envelope(&mut self.reader) => match env {
                    Ok(e) => AppEvent::DaemonEnvelope(e),
                    Err(e) => return ExitReason::DaemonClosed(e.to_string()),
                },
                _ = winch.recv() => {
                    let (cols, rows) = terminal_size_fallback();
                    AppEvent::Resize { rows, cols }
                },
                _ = tick.tick() => AppEvent::Tick,
            };

            let actions = self.app.handle_event(event);
            if let Err(reason) = self.dispatch(actions) {
                return reason;
            }
        }
    }

    fn dispatch(&mut self, actions: Vec<AppAction>) -> Result<(), ExitReason> {
        for action in actions {
            match action {
                AppAction::SendEnvelope(env) => {
                    if self.cmd_tx.send(env).is_err() {
                        return Err(ExitReason::DaemonError("writer closed".into()));
                    }
                }
                AppAction::DrawFrame => {
                    let app = &self.app;
                    if let Err(e) = self.terminal.draw(|frame| render_tiles(app, frame)) {
                        return Err(ExitReason::AppError(format!("ratatui draw: {e}")));
                    }
                }
                AppAction::FocusTile(id) => {
                    debug!(tile = ?id, "focus");
                }
                AppAction::Detach(reason) => {
                    // Flush the terminal so the last draw is visible
                    // before we leave the alt-screen.
                    let _ = std::io::stdout().flush();
                    return Err(match reason {
                        DetachReason::User => ExitReason::UserDetach,
                        DetachReason::PaneExited { exit_code } => {
                            ExitReason::PaneExited { exit_code }
                        }
                    });
                }
                AppAction::ShowToast { kind, message } => {
                    // C3 implements the actual overlay. For now, log
                    // and move on — the user will at least see it in
                    // `tui.log`.
                    match kind {
                        ToastKind::Error => warn!(%message, "toast"),
                        ToastKind::Success | ToastKind::Info => info!(%message, "toast"),
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for AppRuntime {
    fn drop(&mut self) {
        // Closing cmd_tx makes the writer task exit. We don't await
        // because Drop is sync; the small leak window is harmless
        // (process exiting).
        self.writer_handle.abort();
    }
}

/// Walk the tile layout and render each tile into its `Rect`.
fn render_tiles(app: &App, frame: &mut Frame<'_>) {
    // If the layout is the too-small fallback, render just that.
    if app.view.layout.tiles.len() == 1 && app.view.layout.tiles[0].id == TileId::TooSmall {
        render_too_small(frame, app.view.layout.tiles[0].rect);
        return;
    }

    // Collect tile defs by value so we don't hold an immutable borrow
    // on `app.view.layout.tiles` while also calling render functions
    // that borrow `app` immutably (Rust-level convenience, not
    // correctness — the iterators would just make borrow-checker
    // output noisier).
    let tiles: Vec<TileDef> = app.view.layout.tiles.clone();
    for tile in tiles {
        let focused = tile.id == app.view.focused;
        match &tile.kind {
            TileKind::Pty => {
                pty_tile::render(&app.pty_parser, frame, tile.rect, focused);
            }
            TileKind::Scope(ScopeKind::Docker) => {
                scope::docker::render(&app.docker, frame, tile.rect, focused);
            }
            TileKind::Placeholder { label, eta_phase } => {
                scope::placeholder::render(label, *eta_phase, frame, tile.rect, focused);
            }
            TileKind::TooSmall => {
                // Shouldn't appear outside the fallback layout; guard
                // anyway so a stray variant doesn't panic.
                render_too_small(frame, tile.rect);
            }
        }
    }
}

fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Terminal too small for god view",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Resize to at least 80×24.",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(body, inner);
}

fn spawn_writer_task(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut cmd_rx: mpsc::UnboundedReceiver<Envelope>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(env) = cmd_rx.recv().await {
            if let Err(e) = write_envelope(&mut writer, &env).await {
                debug!(error = %e, "TUI writer task ending");
                break;
            }
        }
        let _ = writer.shutdown().await;
    })
}

fn print_exit_message(pane_id: tepegoz_proto::PaneId, reason: ExitReason) {
    match reason {
        ExitReason::UserDetach => {
            println!("\n[detached — daemon and pane {pane_id} still running]");
        }
        ExitReason::PaneExited { exit_code } => {
            println!("\n[pane {pane_id} exited (code={exit_code:?})]");
        }
        ExitReason::DaemonClosed(msg) => {
            eprintln!("\n[daemon closed: {msg}]");
        }
        ExitReason::DaemonError(msg) => {
            eprintln!("\n[daemon write failed: {msg}]");
        }
        ExitReason::StdinClosed => {
            println!("\n[stdin closed — detaching]");
        }
        ExitReason::StdinError(msg) => {
            eprintln!("\n[stdin error: {msg}]");
        }
        ExitReason::AppError(msg) => {
            eprintln!("\n[runtime error: {msg}]");
        }
    }
}

fn terminal_size_fallback() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((120, 40))
}
