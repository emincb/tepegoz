//! Per-client handler: handshake, command dispatch, event forwarding.
//!
//! Design: one dedicated writer task owns the socket's write half and
//! consumes [`Envelope`]s from an mpsc channel. Every other task (main
//! command loop, per-subscription forwarders) sends envelopes through that
//! channel. This serializes writes without per-write locking.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio::task::AbortHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use tepegoz_proto::{
    DockerActionOutcome, DockerActionRequest, DockerActionResult, DockerContainer, DockerStats,
    Envelope, ErrorInfo, ErrorKind, Event, EventFrame, LogStream, PROTOCOL_VERSION, Payload,
    Subscription, Welcome,
    codec::{read_envelope, write_envelope},
};
use tepegoz_pty::{OpenSpec as PtyOpenSpec, Pane, PaneUpdate};

use crate::state::{DAEMON_VERSION, SharedState};

/// How often the docker subscription re-fetches the container list.
const DOCKER_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Backoff before re-attempting `Engine::connect` after a failure.
const DOCKER_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

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

    // Handshake. Validate the wire framing version (`Envelope.version`) AND
    // the application-level `Hello.client_version` before we start dispatching
    // commands. Architecture commitment: peers reject mismatches with a
    // structured `Error(VersionMismatch)`. Without this guard the daemon
    // would silently accept a v3 client and later trip a rkyv decode error
    // when an unknown variant arrives — opaque to the user.
    let hello = read_envelope(&mut reader).await?;
    if hello.version != PROTOCOL_VERSION {
        let _ = event_tx.send(error_envelope(
            ErrorKind::VersionMismatch,
            &format!(
                "envelope protocol v{} is not supported (daemon speaks v{PROTOCOL_VERSION}); upgrade your client",
                hello.version
            ),
        ));
        // Let the writer flush, then drop tx so the writer task ends and
        // the client sees a clean close.
        drop(event_tx);
        let _ = writer_handle.await;
        return Ok(());
    }
    match &hello.payload {
        Payload::Hello(h) => {
            if h.client_version != PROTOCOL_VERSION {
                let _ = event_tx.send(error_envelope(
                    ErrorKind::VersionMismatch,
                    &format!(
                        "client v{} not supported (daemon speaks v{PROTOCOL_VERSION}); upgrade your client",
                        h.client_version
                    ),
                ));
                drop(event_tx);
                let _ = writer_handle.await;
                return Ok(());
            }
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

    // Both pane and docker subscriptions live in HashMap<id, AbortHandle> so
    // `Unsubscribe { id }` can cancel either kind by id. Until C2 / Slice C1
    // landed we tracked pane subs in a `JoinSet<()>` with no per-id key, so
    // `Unsubscribe` of a pane sub silently no-op'd. The C1 TUI's synthetic
    // re-attach (Unsubscribe(prev_pane_sub) + AttachPane(new_pane_sub) on
    // Scope→Pane switch) was leaking one zombie forwarder per mode switch.
    let mut pane_subs: HashMap<u64, AbortHandle> = HashMap::new();
    let mut docker_subs: HashMap<u64, AbortHandle> = HashMap::new();
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
                    &mut docker_subs,
                )
                .await
                {
                    Ok(()) => continue,
                    Err(e) => break Err(e),
                }
            }
        }
    };

    for (_, handle) in pane_subs.drain() {
        handle.abort();
    }
    for (_, handle) in docker_subs.drain() {
        handle.abort();
    }
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
    pane_subs: &mut HashMap<u64, AbortHandle>,
    docker_subs: &mut HashMap<u64, AbortHandle>,
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

        Payload::Subscribe(Subscription::Docker { id }) => {
            if let Some(prev) = docker_subs.remove(&id) {
                debug!(id, "replacing existing docker subscription");
                prev.abort();
            }
            let tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                forward_docker(id, tx).await;
            });
            docker_subs.insert(id, handle.abort_handle());
        }

        Payload::Subscribe(Subscription::DockerLogs {
            id,
            container_id,
            follow,
            tail_lines,
        }) => {
            if let Some(prev) = docker_subs.remove(&id) {
                debug!(id, "replacing existing docker logs subscription");
                prev.abort();
            }
            let tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                forward_docker_logs(id, container_id, follow, tail_lines, tx).await;
            });
            docker_subs.insert(id, handle.abort_handle());
        }

        Payload::Subscribe(Subscription::DockerStats { id, container_id }) => {
            if let Some(prev) = docker_subs.remove(&id) {
                debug!(id, "replacing existing docker stats subscription");
                prev.abort();
            }
            let tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                forward_docker_stats(id, container_id, tx).await;
            });
            docker_subs.insert(id, handle.abort_handle());
        }

        Payload::DockerAction(req) => {
            let tx = event_tx.clone();
            // Spawn so a slow docker daemon doesn't stall the session loop.
            // Each action is independent; we don't track these handles —
            // the writer mpsc closing will collapse any orphaned task.
            tokio::spawn(async move {
                let result = run_docker_action(req).await;
                let _ = tx.send(Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::DockerActionResult(result),
                });
            });
        }

        Payload::Unsubscribe { id } => {
            if *status_sub == Some(id) {
                *status_sub = None;
            }
            if let Some(handle) = docker_subs.remove(&id) {
                handle.abort();
            }
            if let Some(handle) = pane_subs.remove(&id) {
                handle.abort();
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
                if let Some(prev) = pane_subs.remove(&subscription_id) {
                    debug!(
                        subscription_id,
                        "replacing existing pane attachment on same id"
                    );
                    prev.abort();
                }
                let tx = event_tx.clone();
                let handle = tokio::spawn(async move {
                    forward_pane(pane, subscription_id, tx).await;
                });
                pane_subs.insert(subscription_id, handle.abort_handle());
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

/// Per-subscription docker poll loop.
///
/// Connects to the engine, emits an immediate `ContainerList`, then refreshes
/// every [`DOCKER_REFRESH_INTERVAL`]. If `Engine::connect` or
/// `list_containers` fails, emits a single `DockerUnavailable` (only on the
/// transition from available — or initial — to unavailable, not on every retry)
/// and reconnects every [`DOCKER_RECONNECT_INTERVAL`].
///
/// Exits when the writer mpsc closes (client disconnected) or the task is
/// aborted (Unsubscribe).
async fn forward_docker(subscription_id: u64, event_tx: mpsc::UnboundedSender<Envelope>) {
    let mut last_was_unavailable: Option<bool> = None;

    loop {
        let engine = match tepegoz_docker::Engine::connect().await {
            Ok(e) => e,
            Err(e) => {
                if !matches!(last_was_unavailable, Some(true))
                    && event_tx
                        .send(docker_unavailable_envelope(subscription_id, e.to_string()))
                        .is_err()
                {
                    return;
                }
                last_was_unavailable = Some(true);
                tokio::time::sleep(DOCKER_RECONNECT_INTERVAL).await;
                continue;
            }
        };
        let source = engine.source().to_string();
        debug!(
            subscription_id,
            source = %source,
            "docker engine connected for subscription"
        );

        loop {
            match engine.list_containers().await {
                Ok(containers) => {
                    if event_tx
                        .send(container_list_envelope(
                            subscription_id,
                            containers,
                            source.clone(),
                        ))
                        .is_err()
                    {
                        return;
                    }
                    last_was_unavailable = Some(false);
                }
                Err(e) => {
                    warn!(
                        subscription_id,
                        error = %e,
                        "docker list_containers failed; engine may have gone away"
                    );
                    if !matches!(last_was_unavailable, Some(true))
                        && event_tx
                            .send(docker_unavailable_envelope(subscription_id, e.to_string()))
                            .is_err()
                    {
                        return;
                    }
                    last_was_unavailable = Some(true);
                    break; // outer loop reconnects after RECONNECT_INTERVAL
                }
            }
            tokio::time::sleep(DOCKER_REFRESH_INTERVAL).await;
        }

        tokio::time::sleep(DOCKER_RECONNECT_INTERVAL).await;
    }
}

fn container_list_envelope(
    subscription_id: u64,
    containers: Vec<DockerContainer>,
    engine_source: String,
) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::ContainerList {
                containers,
                engine_source,
            },
        }),
    }
}

fn docker_unavailable_envelope(subscription_id: u64, reason: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::DockerUnavailable { reason },
        }),
    }
}

fn stream_ended_envelope(subscription_id: u64, reason: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::DockerStreamEnded { reason },
        }),
    }
}

/// Execute a one-shot docker lifecycle action.
///
/// Always returns a `DockerActionResult` — never propagates an `anyhow::Error`
/// up — so callers (in particular the spawned task in `handle_command`) can
/// reliably forward the structured result back to the client. Engine connect
/// failures and bollard errors both surface as `Failure { reason }`.
async fn run_docker_action(req: DockerActionRequest) -> DockerActionResult {
    let outcome = match tepegoz_docker::Engine::connect().await {
        Ok(engine) => match engine.action(&req.container_id, req.kind).await {
            Ok(()) => DockerActionOutcome::Success,
            Err(e) => DockerActionOutcome::Failure {
                reason: format!("{e:#}"),
            },
        },
        Err(e) => DockerActionOutcome::Failure {
            reason: format!("docker engine unavailable: {e}"),
        },
    };
    DockerActionResult {
        request_id: req.request_id,
        container_id: req.container_id,
        kind: req.kind,
        outcome,
    }
}

/// Per-`Subscribe(DockerLogs)` forwarder.
///
/// Connects to the engine, opens the bollard log stream, and forwards each
/// chunk as a `ContainerLog` event. Always emits a final
/// `DockerStreamEnded` (even on connect failure or if the container
/// doesn't exist) so the client knows the stream is terminal — without it
/// a UI would be left "spinning" with no signal that the docker side is
/// gone. After that event the task exits; client may unsubscribe to free
/// local state.
async fn forward_docker_logs(
    subscription_id: u64,
    container_id: String,
    follow: bool,
    tail_lines: u32,
    event_tx: mpsc::UnboundedSender<Envelope>,
) {
    let engine = match tepegoz_docker::Engine::connect().await {
        Ok(e) => e,
        Err(e) => {
            let _ = event_tx.send(stream_ended_envelope(
                subscription_id,
                format!("engine unavailable: {e}"),
            ));
            return;
        }
    };

    let mut stream = engine.logs_stream(&container_id, follow, tail_lines);
    let mut end_reason = String::from("stream ended");
    while let Some(item) = stream.next().await {
        match item {
            Ok((stream_kind, data)) => {
                let env = log_chunk_envelope(subscription_id, stream_kind, data);
                if event_tx.send(env).is_err() {
                    return;
                }
            }
            Err(e) => {
                end_reason = e.to_string();
                break;
            }
        }
    }
    let _ = event_tx.send(stream_ended_envelope(subscription_id, end_reason));
}

/// Per-`Subscribe(DockerStats)` forwarder.
///
/// Same shape as `forward_docker_logs`: stream samples until the container
/// or engine goes away, then emit `DockerStreamEnded` with the reason.
async fn forward_docker_stats(
    subscription_id: u64,
    container_id: String,
    event_tx: mpsc::UnboundedSender<Envelope>,
) {
    let engine = match tepegoz_docker::Engine::connect().await {
        Ok(e) => e,
        Err(e) => {
            let _ = event_tx.send(stream_ended_envelope(
                subscription_id,
                format!("engine unavailable: {e}"),
            ));
            return;
        }
    };

    let mut stream = engine.stats_stream(&container_id);
    let mut end_reason = String::from("stream ended");
    while let Some(item) = stream.next().await {
        match item {
            Ok(stats) => {
                let env = stats_envelope(subscription_id, stats);
                if event_tx.send(env).is_err() {
                    return;
                }
            }
            Err(e) => {
                end_reason = e.to_string();
                break;
            }
        }
    }
    let _ = event_tx.send(stream_ended_envelope(subscription_id, end_reason));
}

fn log_chunk_envelope(subscription_id: u64, stream: LogStream, data: Vec<u8>) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::ContainerLog { stream, data },
        }),
    }
}

fn stats_envelope(subscription_id: u64, stats: DockerStats) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::ContainerStats(stats),
        }),
    }
}
