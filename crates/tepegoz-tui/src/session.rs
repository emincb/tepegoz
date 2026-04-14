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
use crate::toast;

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

/// Owns the I/O machinery the App needs: reader + writer tasks, an
/// inbound envelope mpsc, the stdin reader, the SIGWINCH stream, and
/// the ratatui Terminal.
///
/// The reader task is the key piece for wire-correctness: pulling
/// `read_envelope` out of the main-loop `tokio::select!` keeps us
/// clear of `AsyncReadExt::read_exact`'s cancellation-unsafety. See
/// the HANDOFF surprises-a-fresh-me note for the full rationale; the
/// short version is that `select!` can cancel `read_envelope`
/// mid-read, after which the kernel's socket position has advanced
/// but the already-read bytes are gone from userspace — the next
/// `read_envelope` starts mid-payload and treats payload bytes as a
/// length prefix. Phase 4 4d burned 4+ hours on this class of bug.
struct AppRuntime {
    app: App,
    cmd_tx: mpsc::UnboundedSender<Envelope>,
    writer_handle: tokio::task::JoinHandle<()>,
    /// Envelopes parsed by the dedicated reader task. `Ok(env)` on a
    /// successful decode; `Err(e)` on a terminal read failure (after
    /// which the sender is dropped and the next `recv()` returns
    /// `None`).
    inbox_rx: mpsc::UnboundedReceiver<Result<Envelope, anyhow::Error>>,
    reader_handle: tokio::task::JoinHandle<()>,
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

        // Dedicated reader task. The handshake + ensure_pane phases
        // already ran through `read_envelope` directly (sequential
        // request/response, no `select!` around them — cancellation-
        // safe by construction). Only the main-loop select! is at
        // risk, so we spawn the reader task HERE at the transition
        // from sequential startup to concurrent event loop.
        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel();
        let reader_handle = spawn_reader_task(reader, inbox_tx);

        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;

        Ok(Self {
            app: App::new(pane_id, terminal_size),
            cmd_tx,
            writer_handle,
            inbox_rx,
            reader_handle,
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
                // mpsc::Receiver::recv IS cancellation-safe: if the
                // select cancels this branch, the envelope we were
                // polling for stays in the channel for the next
                // iteration. Contrast with the pre-fix branch that
                // called read_envelope directly — `read_exact` is
                // documented NOT cancellation-safe, which was the
                // Phase 4 4d desync.
                inbox = self.inbox_rx.recv() => match inbox {
                    Some(Ok(e)) => AppEvent::DaemonEnvelope(e),
                    Some(Err(e)) => return ExitReason::DaemonClosed(e.to_string()),
                    None => return ExitReason::DaemonClosed("reader task ended".into()),
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
        // (process exiting). Same for the reader task — abort is
        // idempotent if the task already exited on its own.
        self.reader_handle.abort();
        self.writer_handle.abort();
    }
}

/// Walk the tile layout and render each tile into its `Rect`, then
/// overlay the toast strip on top so it paints above any tile content.
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
            TileKind::Scope(ScopeKind::Ports) => {
                scope::ports::render(&app.ports, frame, tile.rect, focused);
            }
            TileKind::Scope(ScopeKind::Fleet) => {
                scope::fleet::render(&app.fleet, frame, tile.rect, focused);
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

    let toasts: Vec<_> = app.toasts.iter().cloned().collect();
    toast::render(&toasts, &app.view.layout, frame);
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

/// Per-connection reader task for the TUI. Loops `read_envelope` and
/// forwards each successful decode through the mpsc; on a terminal
/// read error, sends a final `Err(_)` and exits (the main loop maps
/// that to `ExitReason::DaemonClosed`). After the final Err, the
/// sender is dropped so the next `recv()` returns `None` instead of
/// hanging.
///
/// This is the cancellation-safety fix for the Phase 4 4d desync:
/// pulling `read_exact` out of the main-loop `tokio::select!` means
/// the select only polls `mpsc::Receiver::recv()`, which IS
/// cancellation-safe (any pending envelope stays in the channel if
/// the branch gets cancelled). Mirrors the daemon's
/// `spawn_writer_task` pattern — one task per direction, funneled
/// through an mpsc, selected against a cancellation-safe primitive.
fn spawn_reader_task(
    mut reader: tokio::net::unix::OwnedReadHalf,
    inbox_tx: mpsc::UnboundedSender<Result<Envelope, anyhow::Error>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match read_envelope(&mut reader).await {
                Ok(env) => {
                    if inbox_tx.send(Ok(env)).is_err() {
                        // Main loop dropped rx (shutting down); exit
                        // cleanly so the kernel reaps us promptly.
                        debug!("TUI reader task: mpsc closed by consumer; exiting");
                        return;
                    }
                }
                Err(e) => {
                    debug!(error = %e, "TUI reader task: terminal read error");
                    // Best-effort: forward the error so the main loop
                    // can surface a DaemonClosed with the reason.
                    // Drop the sender implicitly by returning — the
                    // next `recv()` will yield `None`.
                    let _ = inbox_tx.send(Err(e));
                    return;
                }
            }
        }
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

#[cfg(test)]
mod tests {
    //! Phase 4 4d regression test. Exercises the reader-task fix under
    //! exactly the conditions that bit the user: large multi-poll
    //! envelope reads AND concurrent stdin pressure racing against
    //! them inside a `tokio::select!`. Pre-fix code fails here because
    //! `read_exact` cancellation loses bytes; post-fix, the main-loop
    //! select polls `mpsc::Receiver::recv()` which IS cancellation-safe.

    use std::time::Duration;

    use tepegoz_proto::{
        Envelope, EventFrame, PROTOCOL_VERSION, ProbeProcess, codec::write_envelope,
    };
    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc;
    use tokio::time::MissedTickBehavior;

    use super::spawn_reader_task;

    const N_ENVELOPES: usize = 25;
    const ROWS_PER_ENVELOPE: usize = 500;

    fn big_process_list_envelope(envelope_id: usize) -> Envelope {
        // Long command strings to push each envelope into the multi-
        // poll regime on a unix socket. On this shape a 500-row
        // ProcessList serializes to ~100 KB — large enough that the
        // kernel can split the read across multiple `read_exact`
        // internal reads, which is exactly where the pre-fix code
        // lost bytes when the select! cancelled the read.
        let rows: Vec<ProbeProcess> = (0..ROWS_PER_ENVELOPE)
            .map(|j| ProbeProcess {
                pid: (envelope_id as u32) * 100_000 + j as u32,
                parent_pid: 1,
                start_time_unix_secs: 1_700_000_000,
                command: format!(
                    "long_command_envelope_{envelope_id}_row_{j}_padding_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                ),
                cpu_percent: Some(0.1 * j as f32),
                mem_bytes: 4096 * (j as u64 + 1),
                partial: false,
            })
            .collect();
        Envelope {
            version: PROTOCOL_VERSION,
            payload: tepegoz_proto::Payload::Event(EventFrame {
                subscription_id: 83,
                event: tepegoz_proto::Event::ProcessList {
                    rows,
                    source: "sysinfo-test".into(),
                },
            }),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stdin_pressure_does_not_desync_large_envelopes() {
        // Wire up a connected unix socket pair. The "daemon" side
        // writes large envelopes; the "client" side hosts
        // spawn_reader_task and the main-loop select!.
        let (daemon_side, client_side) = tokio::net::UnixStream::pair().expect("socket pair");
        let (client_read, _client_write_dropped) = client_side.into_split();
        let (_daemon_read_dropped, mut daemon_write) = daemon_side.into_split();

        // Spawn the reader task — the fix under test.
        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        let _reader_handle = spawn_reader_task(client_read, inbox_tx);

        // "Daemon" task: write N envelopes as fast as the socket
        // accepts them. Note we keep `daemon_write` alive until all
        // writes complete so the socket doesn't half-close mid-stream.
        let daemon_handle = tokio::spawn(async move {
            for id in 0..N_ENVELOPES {
                let env = big_process_list_envelope(id);
                if let Err(e) = write_envelope(&mut daemon_write, &env).await {
                    panic!("daemon write of envelope {id} failed: {e}");
                }
            }
            // Intentionally NOT dropping daemon_write yet so the
            // reader has time to drain the kernel buffer before EOF.
            daemon_write
        });

        // Stdin-pressure source: `tokio::io::repeat(b'y')` feeds
        // bytes as fast as the consumer reads them, equivalent to
        // `yes | tui` that reproduced the production desync.
        let mut stdin_source = tokio::io::repeat(b'y');
        let mut stdin_buf = vec![0u8; 4096];

        // Tick at 33 ms — same cadence as the TUI's main loop.
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Main-loop mimic. Under the fix, `inbox_rx.recv()` is
        // cancellation-safe; stdin and tick branches firing won't
        // corrupt envelope boundaries. Pre-fix, the same loop with
        // `read_envelope` in-place of the mpsc would desync within
        // seconds under this pressure.
        let mut received: usize = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while received < N_ENVELOPES {
            if std::time::Instant::now() > deadline {
                panic!(
                    "regression: received only {received} / {N_ENVELOPES} envelopes \
                     within 15 s — reader task OR main-loop select is back to a \
                     cancellation-unsafe read path"
                );
            }

            tokio::select! {
                inbox = inbox_rx.recv() => match inbox {
                    Some(Ok(env)) => {
                        let tepegoz_proto::Payload::Event(EventFrame {
                            event: tepegoz_proto::Event::ProcessList { rows, source },
                            ..
                        }) = env.payload
                        else {
                            panic!("expected ProcessList envelope");
                        };
                        assert_eq!(
                            rows.len(),
                            ROWS_PER_ENVELOPE,
                            "envelope #{received} must arrive with all rows intact \
                             — a partial-read desync would have shorter or corrupted rows"
                        );
                        assert_eq!(source, "sysinfo-test");
                        received += 1;
                    }
                    Some(Err(e)) => panic!("reader task emitted error after {received} envelopes: {e}"),
                    None => panic!("reader task channel closed after {received}/{N_ENVELOPES} envelopes"),
                },
                _ = stdin_source.read(&mut stdin_buf) => {
                    // Deliberately do nothing. The point is the cancellation
                    // pressure on OTHER select branches: every time this
                    // branch fires, the mpsc-recv branch's pending future
                    // gets cancelled. mpsc::Receiver::recv is documented
                    // cancellation-safe (pending items stay buffered); the
                    // pre-fix read_exact was not.
                }
                _ = tick.tick() => {
                    // Extra cancellation pressure to mirror the TUI's
                    // 30 Hz redraw tick.
                }
            }
        }

        // Clean up.
        let daemon_write = daemon_handle.await.expect("daemon task joined");
        drop(daemon_write);
        assert_eq!(received, N_ENVELOPES);
    }
}
