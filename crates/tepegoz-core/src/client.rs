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
    ProbePort, ProbeProcess, Subscription, Welcome,
    codec::{read_envelope, write_envelope},
};
use tepegoz_pty::{OpenSpec as PtyOpenSpec, Pane, PaneUpdate};

use crate::state::{DAEMON_VERSION, SharedState};

/// How often the docker subscription re-fetches the container list.
const DOCKER_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Backoff before re-attempting `Engine::connect` after a failure.
const DOCKER_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
/// How often the ports subscription re-runs the native probe. Matches the
/// docker cadence (Q4 of the Phase 4 proposal): listening ports are stable
/// over minutes, 2s is enough for live UX without redraw churn.
const PORTS_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Backoff before re-running the probe after a failure (e.g., probe
/// permission denied, task panic). Mirrors `DOCKER_RECONNECT_INTERVAL`.
const PORTS_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
/// How often the processes subscription re-samples sysinfo. Matches the
/// docker/ports cadence. CPU% is computed as a delta over this interval,
/// so the first `ProcessList` after subscription has `cpu_percent: None`
/// and subsequent events carry `Some(x)` — the TUI renders `None` as an
/// em-dash to disambiguate "not yet measured" from "idle".
const PROCESSES_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Backoff before restarting the processes probe after a failure.
const PROCESSES_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

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

    // Pane, docker, and ports subscriptions all live in HashMap<id, AbortHandle>
    // so `Unsubscribe { id }` can cancel any kind by id. Until C2 / Slice C1
    // landed we tracked pane subs in a `JoinSet<()>` with no per-id key, so
    // `Unsubscribe` of a pane sub silently no-op'd. The C1 TUI's synthetic
    // re-attach (Unsubscribe(prev_pane_sub) + AttachPane(new_pane_sub) on
    // Scope→Pane switch) was leaking one zombie forwarder per mode switch.
    let mut pane_subs: HashMap<u64, AbortHandle> = HashMap::new();
    let mut docker_subs: HashMap<u64, AbortHandle> = HashMap::new();
    let mut ports_subs: HashMap<u64, AbortHandle> = HashMap::new();
    let mut processes_subs: HashMap<u64, AbortHandle> = HashMap::new();
    let mut fleet_subs: HashMap<u64, AbortHandle> = HashMap::new();
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
                    &mut ports_subs,
                    &mut processes_subs,
                    &mut fleet_subs,
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
    for (_, handle) in ports_subs.drain() {
        handle.abort();
    }
    for (_, handle) in processes_subs.drain() {
        handle.abort();
    }
    for (_, handle) in fleet_subs.drain() {
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
            if write_envelope(&mut writer, &env).await.is_err() {
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
    ports_subs: &mut HashMap<u64, AbortHandle>,
    processes_subs: &mut HashMap<u64, AbortHandle>,
    fleet_subs: &mut HashMap<u64, AbortHandle>,
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

        Payload::Subscribe(Subscription::Ports { id }) => {
            if let Some(prev) = ports_subs.remove(&id) {
                debug!(id, "replacing existing ports subscription");
                prev.abort();
            }
            let tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                forward_ports(id, tx).await;
            });
            ports_subs.insert(id, handle.abort_handle());
        }

        Payload::Subscribe(Subscription::Processes { id }) => {
            if let Some(prev) = processes_subs.remove(&id) {
                debug!(id, "replacing existing processes subscription");
                prev.abort();
            }
            let tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                forward_processes(id, tx).await;
            });
            processes_subs.insert(id, handle.abort_handle());
        }

        Payload::Subscribe(Subscription::Fleet { id }) => {
            if let Some(prev) = fleet_subs.remove(&id) {
                debug!(id, "replacing existing fleet subscription");
                prev.abort();
            }
            let tx = event_tx.clone();
            let handle = tokio::spawn(async move {
                forward_fleet(id, tx).await;
            });
            fleet_subs.insert(id, handle.abort_handle());
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
            if let Some(handle) = ports_subs.remove(&id) {
                handle.abort();
            }
            if let Some(handle) = processes_subs.remove(&id) {
                handle.abort();
            }
            if let Some(handle) = fleet_subs.remove(&id) {
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

/// Per-subscription ports poll loop.
///
/// Runs the native probe every [`PORTS_REFRESH_INTERVAL`] and emits a
/// `PortList` event. On probe failure, emits a single `PortsUnavailable`
/// transition event (only on the flip from available — or initial — to
/// unavailable, not on every retry) and retries every
/// [`PORTS_RECONNECT_INTERVAL`].
///
/// macOS correlation: the probe returns rows with `container_id = None`
/// on macOS because pid → container correlation requires a Docker engine
/// lookup (macOS pids are Docker Desktop VM host pids, not in-container
/// pids). This task opportunistically opens a Docker engine connection
/// when a port can't already be attributed to a container, then matches
/// `local_port` against each container's `HostConfig.PortBindings` (as
/// delivered in `DockerContainer::ports`). Engine errors are swallowed —
/// Docker-down gracefully degrades to `container_id = None` without
/// blocking the Ports subscription.
///
/// Exits when the writer mpsc closes (client disconnected) or the task is
/// aborted (Unsubscribe).
async fn forward_ports(subscription_id: u64, event_tx: mpsc::UnboundedSender<Envelope>) {
    let mut last_was_unavailable: Option<bool> = None;
    // Cached engine for macOS correlation. Reset to `None` on any error so
    // the next poll retries `Engine::connect`. Gated to macOS since Linux
    // does correlation in the probe via /proc/<pid>/cgroup.
    #[cfg(target_os = "macos")]
    let mut docker_engine: Option<tepegoz_docker::Engine> = None;

    loop {
        // `list_ports` does synchronous fs / syscall work. Run on the
        // blocking pool so we don't stall the runtime.
        let probe_result = tokio::task::spawn_blocking(tepegoz_probe::list_ports).await;

        // `mut` is only consumed by the `target_os = "macos"` correlation
        // block below (mutates rows in place). On Linux the block is
        // cfg-gated out, leaving `mut` unused — silence clippy there.
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut ports = match probe_result {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                let msg = e.to_string();
                warn!(subscription_id, error = %msg, "ports probe failed");
                if !matches!(last_was_unavailable, Some(true))
                    && event_tx
                        .send(ports_unavailable_envelope(subscription_id, msg))
                        .is_err()
                {
                    return;
                }
                last_was_unavailable = Some(true);
                tokio::time::sleep(PORTS_RECONNECT_INTERVAL).await;
                continue;
            }
            Err(join_err) => {
                // spawn_blocking task panicked — unusual but surfaceable.
                let msg = format!("ports probe task panicked: {join_err}");
                warn!(subscription_id, error = %msg, "ports probe task panic");
                if !matches!(last_was_unavailable, Some(true))
                    && event_tx
                        .send(ports_unavailable_envelope(subscription_id, msg))
                        .is_err()
                {
                    return;
                }
                last_was_unavailable = Some(true);
                tokio::time::sleep(PORTS_RECONNECT_INTERVAL).await;
                continue;
            }
        };

        // macOS: complete pid → container correlation via Docker engine.
        // macOS pids are Docker Desktop VM host pids — they can't carry a
        // cgroup reference, so the probe always returns `container_id: None`
        // on macOS and the daemon matches port numbers against bollard's
        // container list instead.
        //
        // On Linux the probe already filled `container_id` from cgroup for
        // containerized processes; non-containerized processes have no
        // container to correlate to, so the whole block is skipped —
        // avoids a pointless `Engine::connect` on every Linux poll.
        #[cfg(target_os = "macos")]
        {
            let needs_correlation = ports.iter().any(|p| p.container_id.is_none() && p.pid != 0);
            if needs_correlation {
                if docker_engine.is_none() {
                    docker_engine = tepegoz_docker::Engine::connect().await.ok();
                }
                if let Some(engine) = docker_engine.as_ref() {
                    match engine.list_containers().await {
                        Ok(containers) => correlate_ports_to_containers(&mut ports, &containers),
                        Err(e) => {
                            debug!(
                                subscription_id,
                                error = %e,
                                "docker engine failed during ports correlation; dropping engine handle"
                            );
                            docker_engine = None;
                        }
                    }
                }
            }
        }

        if event_tx
            .send(port_list_envelope(
                subscription_id,
                ports,
                tepegoz_probe::SOURCE_LABEL.to_string(),
            ))
            .is_err()
        {
            return;
        }
        last_was_unavailable = Some(false);

        tokio::time::sleep(PORTS_REFRESH_INTERVAL).await;
    }
}

/// For every port that doesn't yet know its container, look for a container
/// with a matching `public_port` in its port bindings. First match wins.
/// Gated to macOS since Linux correlates inline in the probe via cgroup.
#[cfg(target_os = "macos")]
fn correlate_ports_to_containers(ports: &mut [ProbePort], containers: &[DockerContainer]) {
    for port in ports.iter_mut() {
        if port.container_id.is_some() {
            continue;
        }
        for container in containers {
            if container
                .ports
                .iter()
                .any(|cp| cp.public_port == port.local_port && cp.public_port != 0)
            {
                port.container_id = Some(container.id.clone());
                break;
            }
        }
    }
}

fn port_list_envelope(subscription_id: u64, ports: Vec<ProbePort>, source: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::PortList { ports, source },
        }),
    }
}

fn ports_unavailable_envelope(subscription_id: u64, reason: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::PortsUnavailable { reason },
        }),
    }
}

/// Per-subscription processes poll loop.
///
/// Holds a [`tepegoz_probe::ProcessesProbe`] across iterations so sysinfo's
/// CPU% delta computation has a prior sample to compare against. The first
/// emitted `ProcessList` carries `cpu_percent: None` for every row (by
/// probe design); subsequent events carry `Some(x)`.
///
/// Emits `ProcessesUnavailable { reason }` exactly once per availability
/// transition and retries every [`PROCESSES_RECONNECT_INTERVAL`] — same
/// contract as Docker / Ports.
///
/// The probe itself is sync (reads /proc on Linux, calls libproc on macOS);
/// we move it into `spawn_blocking` each iteration and receive it back
/// through the return tuple so the stateful delta computation persists
/// while the runtime stays unblocked.
async fn forward_processes(subscription_id: u64, event_tx: mpsc::UnboundedSender<Envelope>) {
    let mut last_was_unavailable: Option<bool> = None;
    let mut probe = tepegoz_probe::ProcessesProbe::new();

    loop {
        let (probe_back, sample_result) = match tokio::task::spawn_blocking(move || {
            let mut p = probe;
            let r = p.sample();
            (p, r)
        })
        .await
        {
            Ok((p, r)) => (p, r),
            Err(join_err) => (
                // Task panicked — reset probe so the next iteration starts
                // fresh. The first sample after this reset will again emit
                // `cpu_percent: None` (correct per the probe contract).
                tepegoz_probe::ProcessesProbe::new(),
                Err(tepegoz_probe::ProcessesError::Backend(format!(
                    "processes probe task panicked: {join_err}"
                ))),
            ),
        };
        probe = probe_back;

        let rows = match sample_result {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                warn!(subscription_id, error = %msg, "processes probe failed");
                if !matches!(last_was_unavailable, Some(true))
                    && event_tx
                        .send(processes_unavailable_envelope(subscription_id, msg))
                        .is_err()
                {
                    return;
                }
                last_was_unavailable = Some(true);
                tokio::time::sleep(PROCESSES_RECONNECT_INTERVAL).await;
                continue;
            }
        };

        if event_tx
            .send(process_list_envelope(
                subscription_id,
                rows,
                tepegoz_probe::processes::SOURCE_LABEL.to_string(),
            ))
            .is_err()
        {
            return;
        }
        last_was_unavailable = Some(false);

        tokio::time::sleep(PROCESSES_REFRESH_INTERVAL).await;
    }
}

fn process_list_envelope(
    subscription_id: u64,
    rows: Vec<ProbeProcess>,
    source: String,
) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::ProcessList { rows, source },
        }),
    }
}

fn processes_unavailable_envelope(subscription_id: u64, reason: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::ProcessesUnavailable { reason },
        }),
    }
}

/// How often the supervisor probes a live SSH session with a
/// `keepalive@openssh.com` global request. Matches OpenSSH's
/// `ServerAliveInterval` default.
const SSH_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Miss count that transitions a healthy host to Degraded. A "miss"
/// is a keepalive send that returned Err or a closed-handle check
/// that reported true.
const SSH_DEGRADED_THRESHOLD: u32 = 1;
/// Miss count that transitions to Disconnected + triggers reconnect.
/// Matches the CTO-spec "three consecutive misses" shape exactly.
/// russh 0.60's `send_keepalive` is fire-and-forget — there's no
/// Future that resolves on server ack — so a miss is a send that
/// returned Err OR `handle.is_closed()`. Against a cleanly-killed
/// TCP connection every miss fires fast (TCP RST), so the whole
/// window from first miss → disconnect is ~90 s worst case: one
/// heartbeat interval each.
const SSH_DISCONNECTED_THRESHOLD: u32 = 3;
/// Minimum dwell time in `Connected` to reset the reconnect backoff.
/// A connection that holds longer than this before dying is treated
/// as "healthy, then transient failure" — next retry starts from 1 s.
/// Shorter connections compound backoff so a perpetually-broken host
/// doesn't spin.
const SSH_RECONNECT_RESET_THRESHOLD: Duration = Duration::from_secs(30);
/// Exponential-backoff ladder for reconnect attempts. Cap at the
/// final entry; healthy-connection reset on hold > 30 s drops back to
/// the first entry.
const SSH_BACKOFF_LADDER: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];

/// Coordinator for `Subscription::Fleet`.
///
/// Discovers SSH hosts (ssh_config / tepegoz config.toml / env),
/// emits an initial `HostList` snapshot, then spawns a per-host
/// [`host_supervisor`] task inside a tokio `JoinSet`. When this
/// coordinator is cancelled (Unsubscribe / client disconnect / daemon
/// shutdown), the `JoinSet` drops → all supervisor tasks abort
/// cleanly.
///
/// Phase 5 Slice 5c-i: supervisors own the Disconnected → Connecting
/// → Connected → (Degraded) → Disconnected state machine with
/// exponential backoff reconnect. No user-driven FleetAction yet —
/// that ships in 5c-ii with wire v8. Hosts with `autoconnect = true`
/// in tepegoz `config.toml` dial on startup; everything else waits
/// for 5c-ii's `FleetAction::Reconnect`.
///
/// Discovery runs on tokio's blocking pool because ssh_config parsing
/// does filesystem reads.
async fn forward_fleet(subscription_id: u64, event_tx: mpsc::UnboundedSender<Envelope>) {
    let list = match tokio::task::spawn_blocking(tepegoz_ssh::HostList::discover).await {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            warn!(subscription_id, error = %e, "fleet discovery failed");
            let _ = event_tx.send(host_list_envelope(
                subscription_id,
                Vec::new(),
                format!("discovery error: {e}"),
            ));
            std::future::pending::<()>().await;
            return;
        }
        Err(e) => {
            warn!(subscription_id, error = %e, "fleet discovery task panicked");
            return;
        }
    };

    let hosts = list.hosts;
    let source = list.source.label();
    let autoconnect = list.autoconnect;

    if event_tx
        .send(host_list_envelope(subscription_id, hosts.clone(), source))
        .is_err()
    {
        return;
    }

    // Spawn one supervisor per host in a JoinSet so subscription
    // cancellation (coordinator drop) aborts every supervisor
    // automatically. Same aggregate-lifecycle pattern as Phase 3's
    // Docker forwarders — no per-task abort bookkeeping needed.
    let mut supervisors: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    for host in hosts {
        let should_autoconnect = autoconnect.contains(&host.alias);
        let tx = event_tx.clone();
        supervisors.spawn(async move {
            host_supervisor(subscription_id, host, should_autoconnect, tx).await;
        });
    }

    // Wait for either (a) all supervisors to complete on their own
    // (terminal states with no reconnect loop running — only happens
    // if every host ends in HostKeyMismatch / AuthFailed / ProxyJump-
    // not-supported and never gets a Reconnect action, which 5c-i
    // can't receive anyway), or (b) this coordinator gets cancelled
    // from outside.
    while supervisors.join_next().await.is_some() {}
    std::future::pending::<()>().await;
}

/// State-machine loop for a single SSH host. Emits
/// `Event::HostStateChanged` on every transition; runs heartbeat
/// while Connected; applies exponential backoff on reconnect.
///
/// Phase 5 Slice 5c-i: runs autonomously — no external message
/// channel (5c-ii adds that for `FleetAction::Reconnect`). Hosts with
/// `autoconnect = false` emit an initial Disconnected and park
/// forever in 5c-i; they'll get an action channel in 5c-ii.
async fn host_supervisor(
    subscription_id: u64,
    entry: tepegoz_proto::HostEntry,
    autoconnect: bool,
    event_tx: mpsc::UnboundedSender<Envelope>,
) {
    let alias = entry.alias.clone();

    // ProxyJump pre-check (5a follow-up #1): we don't speak ProxyJump
    // in Phase 5; transition straight to AuthFailed with the v1.1
    // reason so the Fleet tile renders ⚠ red + (5c-ii) the red toast
    // carries the reason verbatim. Terminal state; no reconnect loop.
    if entry.proxy_jump.is_some() {
        emit_state(
            &event_tx,
            subscription_id,
            &alias,
            tepegoz_proto::HostState::AuthFailed,
        );
        warn!(
            alias,
            proxy_jump = entry.proxy_jump.as_deref().unwrap_or("?"),
            "host requires ProxyJump which is not supported in Phase 5 (v1.1)"
        );
        std::future::pending::<()>().await;
        return;
    }

    // Always emit the initial Disconnected so the tile's per-alias
    // state map is seeded — the renderer depends on it.
    emit_state(
        &event_tx,
        subscription_id,
        &alias,
        tepegoz_proto::HostState::Disconnected,
    );

    if !autoconnect {
        // Lazy-connect hosts wait for user action. 5c-i has no action
        // channel, so they park here; 5c-ii replaces the pending with
        // an action-select loop.
        std::future::pending::<()>().await;
        return;
    }

    let mut backoff_idx: usize = 0;
    loop {
        emit_state(
            &event_tx,
            subscription_id,
            &alias,
            tepegoz_proto::HostState::Connecting,
        );

        let connect_start = std::time::Instant::now();
        let connect_result = try_connect(&entry).await;

        match connect_result {
            Ok(session) => {
                emit_state(
                    &event_tx,
                    subscription_id,
                    &alias,
                    tepegoz_proto::HostState::Connected,
                );
                // Run the heartbeat loop until the session dies.
                run_connected_session(&alias, subscription_id, session, &event_tx).await;
                // Session ended: reset backoff if the connection held
                // longer than the threshold, otherwise compound it so
                // a flapping host doesn't retry every second forever.
                if connect_start.elapsed() >= SSH_RECONNECT_RESET_THRESHOLD {
                    backoff_idx = 0;
                }
                emit_state(
                    &event_tx,
                    subscription_id,
                    &alias,
                    tepegoz_proto::HostState::Disconnected,
                );
            }
            Err(tepegoz_ssh::SshError::HostKeyMismatch { .. }) => {
                warn!(
                    alias,
                    "host-key TOFU rejected — terminal state, awaiting user --ssh-forget"
                );
                emit_state(
                    &event_tx,
                    subscription_id,
                    &alias,
                    tepegoz_proto::HostState::HostKeyMismatch,
                );
                // Terminal; no reconnect until 5c-ii's FleetAction
                // resets the supervisor after the user runs
                // --ssh-forget.
                std::future::pending::<()>().await;
                return;
            }
            Err(tepegoz_ssh::SshError::AuthFailed { reason, .. }) => {
                warn!(alias, reason = %reason, "authentication failed — terminal state");
                emit_state(
                    &event_tx,
                    subscription_id,
                    &alias,
                    tepegoz_proto::HostState::AuthFailed,
                );
                std::future::pending::<()>().await;
                return;
            }
            Err(e) => {
                warn!(alias, error = %e, "connect failed — will retry after backoff");
                emit_state(
                    &event_tx,
                    subscription_id,
                    &alias,
                    tepegoz_proto::HostState::Disconnected,
                );
            }
        }

        let delay = SSH_BACKOFF_LADDER[backoff_idx.min(SSH_BACKOFF_LADDER.len() - 1)];
        tokio::time::sleep(delay).await;
        backoff_idx = (backoff_idx + 1).min(SSH_BACKOFF_LADDER.len() - 1);
    }
}

/// Thin wrapper around `tepegoz_ssh::connect_host` that builds a one-
/// entry `HostList` + opens a `KnownHostsStore`. Discovery already ran
/// in the coordinator; we rebuild a single-host list here just for the
/// connect_host API shape (alias → lookup).
async fn try_connect(
    entry: &tepegoz_proto::HostEntry,
) -> Result<tepegoz_ssh::SshSession, tepegoz_ssh::SshError> {
    use std::collections::HashSet;
    let hosts = tepegoz_ssh::HostList {
        hosts: vec![entry.clone()],
        source: tepegoz_ssh::HostSource::None,
        autoconnect: HashSet::new(),
    };
    let known_hosts = tepegoz_ssh::KnownHostsStore::open()?;
    tepegoz_ssh::connect_host(&entry.alias, &hosts, &known_hosts).await
}

/// Heartbeat loop while `Connected`. Runs until the session dies
/// (heartbeat send fails / handle reports closed). Transitions the
/// state to `Degraded` after the first miss so the tile renders ◐
/// yellow before the final cutover.
async fn run_connected_session(
    alias: &str,
    subscription_id: u64,
    session: tepegoz_ssh::SshSession,
    event_tx: &mpsc::UnboundedSender<Envelope>,
) {
    let mut interval = tokio::time::interval(SSH_HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Consume the first (immediate) tick — we just emitted Connected,
    // no point in sending a keepalive before we've held the connection
    // for even one interval.
    interval.tick().await;

    let mut miss_counter: u32 = 0;
    let mut current_state = tepegoz_proto::HostState::Connected;

    loop {
        interval.tick().await;

        let handle = session.handle();
        let send_ok = if handle.is_closed() {
            false
        } else {
            handle.send_keepalive(true).await.is_ok()
        };

        if send_ok {
            if miss_counter > 0 && current_state == tepegoz_proto::HostState::Degraded {
                current_state = tepegoz_proto::HostState::Connected;
                emit_state(event_tx, subscription_id, alias, current_state);
            }
            miss_counter = 0;
        } else {
            miss_counter += 1;
            if miss_counter >= SSH_DISCONNECTED_THRESHOLD {
                // Disconnect path — caller emits Disconnected on
                // return and starts the backoff retry.
                return;
            }
            if miss_counter >= SSH_DEGRADED_THRESHOLD
                && current_state != tepegoz_proto::HostState::Degraded
            {
                current_state = tepegoz_proto::HostState::Degraded;
                emit_state(event_tx, subscription_id, alias, current_state);
            }
        }
    }
}

fn emit_state(
    event_tx: &mpsc::UnboundedSender<Envelope>,
    subscription_id: u64,
    alias: &str,
    state: tepegoz_proto::HostState,
) {
    let _ = event_tx.send(Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::HostStateChanged {
                alias: alias.to_string(),
                state,
            },
        }),
    });
}

fn host_list_envelope(
    subscription_id: u64,
    hosts: Vec<tepegoz_proto::HostEntry>,
    source: String,
) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::HostList { hosts, source },
        }),
    }
}
