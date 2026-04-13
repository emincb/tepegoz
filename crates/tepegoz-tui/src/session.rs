//! Connect, attach, and pipe bytes until detach or pane exit.

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{debug, info, warn};

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneId, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

use crate::input::{InputAction, InputFilter};
use crate::terminal;

const ATTACH_SUB_ID: u64 = 100;

pub(crate) async fn run(socket_path: PathBuf) -> anyhow::Result<()> {
    let stream = UnixStream::connect(&socket_path).await?;
    info!(socket = %socket_path.display(), "connected");
    let (mut reader, mut writer) = stream.into_split();

    handshake(&mut reader, &mut writer).await?;

    let (cols, rows) = terminal_size_fallback();
    let pane = ensure_pane(&mut reader, &mut writer, rows, cols).await?;
    info!(pane_id = pane.id, alive = pane.alive, "attaching to pane");

    attach(&mut reader, &mut writer, pane.id, rows, cols).await
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

    // No live pane — open one.
    write_envelope(
        writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(OpenPaneSpec {
                shell: None,
                cwd: None,
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

async fn attach(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    pane_id: PaneId,
    rows: u16,
    cols: u16,
) -> anyhow::Result<()> {
    write_envelope(
        writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id,
                subscription_id: ATTACH_SUB_ID,
            },
        },
    )
    .await?;

    // Terminal goes raw for the lifetime of the attach; guard restores it
    // on any exit path.
    terminal::enter_raw()?;
    let _guard = terminal::TerminalGuard;

    // Tell the daemon our current size — the pane's initial size might not
    // match us.
    write_envelope(
        writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::ResizePane {
                pane_id,
                rows,
                cols,
            },
        },
    )
    .await?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buf = vec![0u8; 4096];
    let mut input_filter = InputFilter::new();

    let mut winch = signal(SignalKind::window_change())?;

    let exit_reason: ExitReason = 'attach: loop {
        tokio::select! {
            n = stdin.read(&mut stdin_buf) => {
                let n = match n {
                    Ok(0) => break 'attach ExitReason::StdinClosed,
                    Ok(n) => n,
                    Err(e) => break 'attach ExitReason::StdinError(e.to_string()),
                };
                for action in input_filter.process(&stdin_buf[..n]) {
                    match action {
                        InputAction::Forward(bytes) => {
                            let env = Envelope {
                                version: PROTOCOL_VERSION,
                                payload: Payload::SendInput { pane_id, data: bytes },
                            };
                            if let Err(e) = write_envelope(writer, &env).await {
                                break 'attach ExitReason::DaemonError(e.to_string());
                            }
                        }
                        InputAction::Detach => {
                            break 'attach ExitReason::UserDetach;
                        }
                    }
                }
            }

            env = read_envelope(reader) => {
                let env = match env {
                    Ok(e) => e,
                    Err(e) => break 'attach ExitReason::DaemonClosed(e.to_string()),
                };
                match env.payload {
                    Payload::Event(EventFrame { event: Event::PaneSnapshot { scrollback, .. }, .. }) => {
                        if !scrollback.is_empty() {
                            stdout.write_all(&scrollback).await?;
                            stdout.flush().await?;
                        }
                    }
                    Payload::Event(EventFrame { event: Event::PaneOutput { data }, .. }) => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    Payload::Event(EventFrame { event: Event::PaneExit { exit_code }, .. }) => {
                        break 'attach ExitReason::PaneExited { code: exit_code };
                    }
                    Payload::Event(EventFrame { event: Event::PaneLagged { dropped_bytes }, .. }) => {
                        warn!(dropped = dropped_bytes, "pane subscriber lagged — some output skipped");
                    }
                    Payload::Pong | Payload::Welcome(_) => {}
                    Payload::Error(e) => {
                        warn!(?e.kind, msg = %e.message, "daemon error");
                    }
                    other => debug!(?other, "unexpected envelope during attach"),
                }
            }

            _ = winch.recv() => {
                let (new_cols, new_rows) = terminal_size_fallback();
                debug!(rows = new_rows, cols = new_cols, "terminal resized");
                let env = Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::ResizePane { pane_id, rows: new_rows, cols: new_cols },
                };
                if let Err(e) = write_envelope(writer, &env).await {
                    warn!(error = %e, "failed to forward resize");
                }
            }
        }
    };

    // Terminal guard restores on drop. Print a short diagnostic line to
    // the restored terminal so the user knows why we exited.
    drop(_guard);
    match exit_reason {
        ExitReason::UserDetach => {
            println!("\n[detached — daemon and pane {pane_id} still running]");
        }
        ExitReason::PaneExited { code } => {
            println!("\n[pane {pane_id} exited (code={code:?})]");
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
    }
    Ok(())
}

enum ExitReason {
    UserDetach,
    PaneExited { code: Option<i32> },
    DaemonClosed(String),
    DaemonError(String),
    StdinClosed,
    StdinError(String),
}

fn terminal_size_fallback() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((120, 40))
}
