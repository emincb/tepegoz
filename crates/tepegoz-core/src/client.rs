//! Per-client handler: handshake, command dispatch, event forwarding.
//!
//! Design: one dedicated writer task owns the socket's write half and
//! consumes [`Envelope`]s from an mpsc channel. Every other task (main
//! command loop, per-subscription forwarders) sends envelopes through that
//! channel. This serializes writes without per-write locking.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use tepegoz_proto::{
    Envelope, ErrorInfo, ErrorKind, Event, EventFrame, PROTOCOL_VERSION, Payload, Subscription,
    Welcome,
    codec::{read_envelope, write_envelope},
};
use tepegoz_pty::{OpenSpec as PtyOpenSpec, Pane, PaneUpdate};

use crate::state::{DAEMON_VERSION, SharedState};

pub(crate) async fn handle_client(
    stream: UnixStream,
    state: Arc<SharedState>,
) -> anyhow::Result<()> {
    let (reader, writer) = stream.into_split();

    let total = state.clients_total.fetch_add(1, Ordering::Relaxed) + 1;
    let now = state.clients_now.fetch_add(1, Ordering::Relaxed) + 1;
    info!(client_no = total, concurrent = now, "client connected");

    let result = session(reader, writer, Arc::clone(&state)).await;

    state.clients_now.fetch_sub(1, Ordering::Relaxed);
    info!(
        remaining = state.clients_now.load(Ordering::Relaxed),
        "client disconnected"
    );

    result
}

async fn session(
    mut reader: tokio::net::unix::OwnedReadHalf,
    writer: tokio::net::unix::OwnedWriteHalf,
    state: Arc<SharedState>,
) -> anyhow::Result<()> {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Envelope>();
    let writer_handle = spawn_writer_task(writer, event_rx, Arc::clone(&state));

    // Handshake
    let hello = read_envelope(&mut reader).await?;
    match &hello.payload {
        Payload::Hello(h) => {
            debug!(client = %h.client_name, version = h.client_version, "client hello");
        }
        other => anyhow::bail!("expected Hello, got {other:?}"),
    }
    event_tx.send(Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Welcome(Welcome {
            daemon_version: DAEMON_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            daemon_pid: state.daemon_pid,
        }),
    })?;

    let mut pane_subs: JoinSet<()> = JoinSet::new();
    let mut status_sub: Option<u64> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(1000));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let result = loop {
        tokio::select! {
            _ = ticker.tick(), if status_sub.is_some() => {
                let id = status_sub.expect("checked by guard");
                if send_status(&state, &event_tx, id).await.is_err() {
                    break Ok(());
                }
            }

            msg = read_envelope(&mut reader) => {
                let env = match msg {
                    Ok(e) => e,
                    Err(e) => break Err(e),
                };
                match handle_command(
                    env.payload,
                    &state,
                    &event_tx,
                    &mut status_sub,
                    &mut pane_subs,
                )
                .await
                {
                    Ok(()) => continue,
                    Err(e) => break Err(e),
                }
            }
        }
    };

    pane_subs.abort_all();
    drop(event_tx);
    let _ = writer_handle.await;

    result
}

fn spawn_writer_task(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut event_rx: mpsc::UnboundedReceiver<Envelope>,
    state: Arc<SharedState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(env) = event_rx.recv().await {
            if let Err(e) = write_envelope(&mut writer, &env).await {
                debug!(error = %e, "writer task ending");
                break;
            }
            state.events_sent.fetch_add(1, Ordering::Relaxed);
        }
    })
}

async fn handle_command(
    payload: Payload,
    state: &Arc<SharedState>,
    event_tx: &mpsc::UnboundedSender<Envelope>,
    status_sub: &mut Option<u64>,
    pane_subs: &mut JoinSet<()>,
) -> anyhow::Result<()> {
    match payload {
        Payload::Ping => {
            event_tx.send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Pong,
            })?;
        }

        Payload::Subscribe(Subscription::Status { id }) => {
            *status_sub = Some(id);
            send_status(state, event_tx, id).await?;
        }

        Payload::Unsubscribe { id } => {
            if *status_sub == Some(id) {
                *status_sub = None;
            }
        }

        Payload::OpenPane(spec) => {
            let pty_spec = PtyOpenSpec {
                shell: spec.shell,
                cwd: spec.cwd.map(std::path::PathBuf::from),
                env: spec.env.into_iter().map(|e| (e.key, e.value)).collect(),
                rows: spec.rows,
                cols: spec.cols,
            };
            match state.pty.open(pty_spec).await {
                Ok(pane) => {
                    event_tx.send(Envelope {
                        version: PROTOCOL_VERSION,
                        payload: Payload::PaneOpened(pane.info()),
                    })?;
                }
                Err(e) => {
                    event_tx.send(error_envelope(
                        ErrorKind::Internal,
                        &format!("open pane: {e}"),
                    ))?;
                }
            }
        }

        Payload::ListPanes => {
            let panes = state.pty.list().await;
            event_tx.send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::PaneList { panes },
            })?;
        }

        Payload::AttachPane {
            pane_id,
            subscription_id,
        } => match state.pty.get(pane_id).await {
            Some(pane) => {
                let tx = event_tx.clone();
                pane_subs.spawn(async move {
                    forward_pane(pane, subscription_id, tx).await;
                });
            }
            None => {
                event_tx.send(error_envelope(
                    ErrorKind::UnknownPane,
                    &format!("no pane {pane_id}"),
                ))?;
            }
        },

        Payload::SendInput { pane_id, data } => {
            if let Some(pane) = state.pty.get(pane_id).await {
                if let Err(e) = pane.send_input(&data) {
                    debug!(pane_id, error = %e, "send_input failed (pane may be dead)");
                }
            }
        }

        Payload::ResizePane {
            pane_id,
            rows,
            cols,
        } => {
            if let Some(pane) = state.pty.get(pane_id).await {
                if let Err(e) = pane.resize(rows, cols) {
                    debug!(pane_id, error = %e, "resize failed");
                }
            }
        }

        Payload::ClosePane { pane_id } => {
            if let Err(e) = state.pty.close(pane_id).await {
                debug!(pane_id, error = %e, "close failed");
            }
        }

        Payload::Hello(_) => {} // ignore re-hellos

        other => {
            debug!(?other, "ignoring unexpected client payload");
        }
    }
    Ok(())
}

async fn send_status(
    state: &Arc<SharedState>,
    event_tx: &mpsc::UnboundedSender<Envelope>,
    subscription_id: u64,
) -> anyhow::Result<()> {
    let snapshot = state.snapshot().await;
    event_tx.send(Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::Status(snapshot),
        }),
    })?;
    Ok(())
}

/// Forward a pane's live output to a client subscription until the pane
/// exits or the client disconnects.
async fn forward_pane(
    pane: Arc<Pane>,
    subscription_id: u64,
    event_tx: mpsc::UnboundedSender<Envelope>,
) {
    let (scrollback, mut rx) = pane.subscribe();
    let (rows, cols) = pane.size();

    let initial = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::PaneSnapshot {
                scrollback: scrollback.to_vec(),
                rows,
                cols,
            },
        }),
    };
    if event_tx.send(initial).is_err() {
        return;
    }

    if !pane.is_alive() {
        let _ = event_tx.send(exit_envelope(subscription_id, pane.exit_code()));
        return;
    }

    loop {
        match rx.recv().await {
            Ok(PaneUpdate::Bytes(b)) => {
                let env = Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::Event(EventFrame {
                        subscription_id,
                        event: Event::PaneOutput { data: b.to_vec() },
                    }),
                };
                if event_tx.send(env).is_err() {
                    return;
                }
            }
            Ok(PaneUpdate::Exit { exit_code }) => {
                let _ = event_tx.send(exit_envelope(subscription_id, exit_code));
                return;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let env = Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::Event(EventFrame {
                        subscription_id,
                        event: Event::PaneLagged { dropped_bytes: n },
                    }),
                };
                if event_tx.send(env).is_err() {
                    return;
                }
                warn!(subscription_id, skipped = n, "pane subscriber lagged");
            }
            Err(broadcast::error::RecvError::Closed) => {
                let _ = event_tx.send(exit_envelope(subscription_id, pane.exit_code()));
                return;
            }
        }
    }
}

fn exit_envelope(subscription_id: u64, exit_code: Option<i32>) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::PaneExit { exit_code },
        }),
    }
}

fn error_envelope(kind: ErrorKind, message: &str) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Error(ErrorInfo {
            kind,
            message: message.to_string(),
        }),
    }
}
