//! Session entry point + I/O glue.
//!
//! [`run`] connects to the daemon, performs the handshake, ensures a pty
//! pane exists, then hands off to [`AppRuntime::run`]. The runtime owns
//! the event loop: stdin → daemon → SIGWINCH → tick all funnel into
//! [`crate::app::App::handle_event`], whose [`crate::app::AppAction`]s are
//! executed here against real I/O.
//!
//! The runtime is intentionally thin — every interesting state transition
//! lives in [`crate::app`] and is unit-tested there. This file's job is to
//! correctly wire bytes between sockets, terminals, and ratatui.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use tepegoz_proto::{
    Envelope, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

use crate::app::{App, AppAction, AppEvent, DetachReason, ToastKind};
use crate::scope;
use crate::terminal;

/// Coalesced redraw cadence in scope mode (~30 Hz). The runtime emits
/// `AppEvent::Tick` at this rate; the App responds with `DrawScope` only
/// when the active view actually needs it.
const SCOPE_TICK_INTERVAL: Duration = Duration::from_millis(33);

pub(crate) async fn run(socket_path: PathBuf) -> anyhow::Result<()> {
    let stream = UnixStream::connect(&socket_path).await?;
    info!(socket = %socket_path.display(), "connected");
    let (mut reader, mut writer) = stream.into_split();

    handshake(&mut reader, &mut writer).await?;

    let (cols, rows) = terminal_size_fallback();
    let pane = ensure_pane(&mut reader, &mut writer, rows, cols).await?;
    info!(pane_id = pane.id, alive = pane.alive, "attaching to pane");

    terminal::enter_raw(&format!("tepegoz · pane {}", pane.id))?;
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

/// Reuse the first live pane if any; otherwise open a new one.
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

    // No live pane — open one. Pass the current working directory so the
    // shell starts where the user invoked `tepegoz tui` from, matching
    // tmux/screen expectations.
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

/// Owns the I/O machinery the App needs: socket halves, a writer mpsc, the
/// stdin reader, the SIGWINCH stream, and the ratatui Terminal.
struct AppRuntime {
    app: App,
    reader: tokio::net::unix::OwnedReadHalf,
    cmd_tx: mpsc::UnboundedSender<Envelope>,
    writer_handle: tokio::task::JoinHandle<()>,
    terminal: ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    /// `true` while the App is in scope mode; gates the redraw ticker so
    /// pane mode doesn't burn CPU on no-op ticks.
    in_scope_mode: bool,
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
            in_scope_mode: false,
        })
    }

    async fn run(mut self) -> ExitReason {
        // Bootstrap: AttachPane + ResizePane.
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
        let mut tick = tokio::time::interval(SCOPE_TICK_INTERVAL);
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
                _ = tick.tick(), if self.in_scope_mode => AppEvent::Tick,
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
                AppAction::WriteStdout(bytes) => {
                    let mut out = std::io::stdout();
                    if let Err(e) = out.write_all(&bytes) {
                        return Err(ExitReason::AppError(format!("stdout write: {e}")));
                    }
                    if let Err(e) = out.flush() {
                        return Err(ExitReason::AppError(format!("stdout flush: {e}")));
                    }
                }
                AppAction::EnterScopeMode => {
                    self.in_scope_mode = true;
                    if let Err(e) = self.terminal.clear() {
                        return Err(ExitReason::AppError(format!("terminal clear: {e}")));
                    }
                }
                AppAction::EnterPaneMode => {
                    self.in_scope_mode = false;
                    if let Err(e) = self.terminal.clear() {
                        return Err(ExitReason::AppError(format!("terminal clear: {e}")));
                    }
                }
                AppAction::DrawScope => {
                    let scope_state = &self.app.docker;
                    if let Err(e) = self
                        .terminal
                        .draw(|frame| scope::docker::render(scope_state, frame))
                    {
                        return Err(ExitReason::AppError(format!("ratatui draw: {e}")));
                    }
                }
                AppAction::Detach(reason) => {
                    return Err(match reason {
                        DetachReason::User => ExitReason::UserDetach,
                        DetachReason::PaneExited { exit_code } => {
                            ExitReason::PaneExited { exit_code }
                        }
                    });
                }
                AppAction::ShowToast { kind, message } => {
                    // C3 implements the actual overlay. For C2, log and
                    // move on — the user will at least see it in
                    // `tui.log`. Severity is kept wired through so C3
                    // can route colors / auto-dismiss off `kind`.
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
        // Closing cmd_tx makes the writer task exit. We don't await it
        // here because Drop is sync; the small leak window is harmless
        // (process is exiting too).
        self.writer_handle.abort();
    }
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
