//! Tepegöz remote agent — speaks the wire protocol to the controller
//! over stdio.
//!
//! Phase 6 Slice 6a shipped the handshake path. Slice 6c-proper extends
//! it to a subscription-capable server:
//!
//! 1. Controller spawns the agent binary on a remote host.
//! 2. Controller sends `Payload::AgentHandshake { request_id }`. Agent
//!    probes local capabilities (docker reachable?) and replies with
//!    `Payload::AgentHandshakeResponse { ..., capabilities }`.
//! 3. Controller sends `Subscribe(Docker/DockerLogs/DockerStats { Local })`
//!    (the daemon has already translated Remote→Local before forwarding
//!    across the stdio tunnel). Agent spawns a per-subscription forwarder
//!    task that emits `Event(..)` envelopes through a dedicated writer
//!    task — same design as the daemon's per-client handler.
//! 4. Controller sends `Unsubscribe { id }` → agent aborts the forwarder.
//! 5. Controller sends `DockerAction(..)` → agent runs the one-shot
//!    action and replies with `DockerActionResult(..)`.
//! 6. On stdin EOF, agent aborts all subscription tasks and drains the
//!    writer, exits cleanly.
//!
//! The agent always serves Local-targeted subscriptions. The daemon
//! maintains per-(alias, daemon_sub_id) → client routing and translates
//! `Remote { alias }` to the agent's local namespace before forwarding,
//! so the agent never sees `ScopeTarget::Remote`.
//!
//! Version drift: the agent's compiled-in [`tepegoz_proto::PROTOCOL_VERSION`]
//! is the source of truth it reports in the handshake response. The
//! controller's `build.rs` asserts at compile time that every embedded
//! agent's manifest matches the controller's own `PROTOCOL_VERSION`; the
//! runtime handshake is the second line of defense for user-deployed
//! (non-embedded) agents.

use std::collections::HashMap;

use futures_util::StreamExt;
use tepegoz_proto::{
    DockerActionOutcome, DockerActionRequest, DockerActionResult, DockerContainer, DockerStats,
    Envelope, ErrorInfo, ErrorKind, Event, EventFrame, LogStream, PROTOCOL_VERSION, Payload,
    Subscription,
    codec::{read_envelope, write_envelope},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, warn};

/// How often the docker subscription re-fetches the container list.
/// Matches the daemon's `DOCKER_REFRESH_INTERVAL` so remote Docker feels
/// identical to local Docker from the TUI's perspective.
const DOCKER_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Backoff before re-attempting `Engine::connect` after a failure.
const DOCKER_RECONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Run the agent over the process's stdio. Drives the event loop
/// until stdin closes or a fatal read/write error is hit.
pub async fn run_stdio() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve(stdin, stdout).await
}

/// Core event loop, generic over reader / writer. `run_stdio` wraps
/// this with real stdin / stdout; unit + integration tests drive it
/// with in-memory duplex streams.
pub async fn serve<R, W>(mut reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Envelope>();
    let writer_handle = spawn_writer_task(writer, event_rx);

    // Active Docker / DockerLogs / DockerStats subscriptions keyed by
    // the daemon-allocated id. Shared across the three Docker kinds
    // because `Unsubscribe { id }` doesn't carry a kind discriminator.
    let mut docker_subs: HashMap<u64, AbortHandle> = HashMap::new();

    let outcome = loop {
        let envelope = match read_envelope(&mut reader).await {
            Ok(env) => env,
            Err(e) => {
                // Plain EOF is graceful shutdown. Anything else is a
                // real read error worth surfacing.
                let msg = format!("{e}");
                if msg.contains("early eof") || msg.contains("UnexpectedEof") {
                    debug!("stdin closed — agent exiting");
                    break Ok(());
                }
                break Err(e);
            }
        };

        // Version-drift guard mirrors the daemon↔client handshake
        // shape — reject with a structured Error and close. Do this
        // before payload dispatch so a malformed-but-well-framed
        // envelope can't get past us with the wrong version number
        // attached.
        if envelope.version != PROTOCOL_VERSION {
            let _ = event_tx.send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Error(ErrorInfo {
                    kind: ErrorKind::VersionMismatch,
                    message: format!(
                        "agent built for wire v{}, controller sent v{}",
                        PROTOCOL_VERSION, envelope.version
                    ),
                }),
            });
            break Ok(());
        }

        handle_envelope(envelope.payload, &event_tx, &mut docker_subs).await;
    };

    // Abort every live subscription forwarder so they don't outlive
    // the connection, then drop the event tx so the writer task drains
    // any in-flight sends and exits.
    for (_, handle) in docker_subs.drain() {
        handle.abort();
    }
    drop(event_tx);
    let _ = writer_handle.await;
    outcome
}

/// Payload dispatch. Each arm either synthesizes an immediate response
/// (handshake, ping) or spawns a per-subscription forwarder task whose
/// events flow through `event_tx` to the writer.
pub(crate) async fn handle_envelope(
    payload: Payload,
    event_tx: &mpsc::UnboundedSender<Envelope>,
    docker_subs: &mut HashMap<u64, AbortHandle>,
) {
    match payload {
        Payload::AgentHandshake { request_id } => {
            let capabilities = probe_capabilities().await;
            let _ = event_tx.send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::AgentHandshakeResponse {
                    request_id,
                    version: PROTOCOL_VERSION,
                    os: std::env::consts::OS.to_string(),
                    arch: std::env::consts::ARCH.to_string(),
                    capabilities,
                },
            });
        }

        Payload::Ping => {
            let _ = event_tx.send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Pong,
            });
        }

        // Daemon always translates Remote → Local before forwarding to
        // the agent, so we ignore `target` here (both variants run the
        // same local bollard forwarder). If a client somehow sends
        // Remote directly, we fall through to the same forwarder —
        // bollard doesn't care, and the daemon layer is the right
        // place to enforce target semantics.
        Payload::Subscribe(Subscription::Docker { id, target: _ }) => {
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
            target: _,
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

        Payload::Subscribe(Subscription::DockerStats {
            id,
            container_id,
            target: _,
        }) => {
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

        Payload::Unsubscribe { id } => {
            if let Some(handle) = docker_subs.remove(&id) {
                debug!(id, "unsubscribing agent forwarder");
                handle.abort();
            }
        }

        Payload::DockerAction(req) => {
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = run_docker_action(req).await;
                let _ = tx.send(Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::DockerActionResult(result),
                });
            });
        }

        other => {
            warn!(?other, "agent received unhandled payload");
            let _ = event_tx.send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Error(ErrorInfo {
                    kind: ErrorKind::InvalidRequest,
                    message: format!(
                        "agent v{PROTOCOL_VERSION} does not handle this payload in Slice 6c-proper"
                    ),
                }),
            });
        }
    }
}

/// Probe which remote-scope capabilities this agent can serve. Called
/// during handshake so the controller knows ahead of time whether a
/// `Subscribe(Docker { Remote })` can succeed — the TUI uses this to
/// grey out hosts in the picker modal that lack the required
/// capability (e.g. an agent host without a running docker daemon).
///
/// 6c-proper: "docker" populates iff `Engine::connect().await` succeeds.
/// 6d will add "ports" / "processes" on the same probe-at-handshake
/// shape. Capabilities are a snapshot: if docker comes up after the
/// agent started, the next handshake (on reconnect) picks it up.
async fn probe_capabilities() -> Vec<String> {
    let mut caps = Vec::new();
    // 5s timeout matches `tepegoz_docker`'s PROBE_TIMEOUT_SECS; we
    // can't import the const, so we rely on `Engine::connect` itself
    // to enforce its own bound.
    if tepegoz_docker::Engine::connect().await.is_ok() {
        caps.push("docker".to_string());
    }
    caps
}

fn spawn_writer_task<W>(
    mut writer: W,
    mut event_rx: mpsc::UnboundedReceiver<Envelope>,
) -> tokio::task::JoinHandle<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(env) = event_rx.recv().await {
            if write_envelope(&mut writer, &env).await.is_err() {
                break;
            }
        }
    })
}

/// Per-`Subscribe(Docker)` forwarder. Mirrors
/// `tepegoz_core::client::forward_docker` — same cadence, same
/// unavailable-transition semantics, same reconnect behaviour — so the
/// daemon can proxy remote subscriptions transparently without the
/// client noticing a behavioural difference versus local Docker.
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
            "docker engine connected for agent subscription"
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
                        "agent docker list_containers failed"
                    );
                    if !matches!(last_was_unavailable, Some(true))
                        && event_tx
                            .send(docker_unavailable_envelope(subscription_id, e.to_string()))
                            .is_err()
                    {
                        return;
                    }
                    last_was_unavailable = Some(true);
                    break;
                }
            }
            tokio::time::sleep(DOCKER_REFRESH_INTERVAL).await;
        }

        tokio::time::sleep(DOCKER_RECONNECT_INTERVAL).await;
    }
}

/// Per-`Subscribe(DockerLogs)` forwarder. Mirrors
/// `tepegoz_core::client::forward_docker_logs`.
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

/// Per-`Subscribe(DockerStats)` forwarder. Mirrors
/// `tepegoz_core::client::forward_docker_stats`.
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

/// Execute a one-shot docker lifecycle action. Mirrors
/// `tepegoz_core::client::run_docker_action`. `target` on the
/// `DockerActionRequest` reaches the agent as `Remote { alias }` —
/// the agent simply echoes it back in the result; the daemon's
/// routing layer is what attributed this action to the alias in the
/// first place, and the TUI needs the echo for pending-action
/// correlation.
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
        target: req.target,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handshake_reports_self_and_capabilities() {
        // Two independent duplex channels: `a` acts as the agent's
        // stdin (we write controller→agent envelopes into `a_tx`,
        // server reads from `a_rx`); `b` is the agent's stdout
        // (server writes into `b_tx`, we read from `b_rx`).
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, mut b_rx) = tokio::io::duplex(2048);

        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::AgentHandshake { request_id: 7 },
                },
            )
            .await
            .unwrap();
        }

        let response = read_envelope(&mut b_rx).await.unwrap();
        match response.payload {
            Payload::AgentHandshakeResponse {
                request_id,
                version,
                os,
                arch,
                capabilities,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(version, PROTOCOL_VERSION);
                assert_eq!(os, std::env::consts::OS);
                assert_eq!(arch, std::env::consts::ARCH);
                // capabilities is env-dependent ("docker" iff a local
                // engine answers within the probe timeout). Only
                // assert the set is valid — no unknown strings.
                for cap in &capabilities {
                    assert!(
                        matches!(cap.as_str(), "docker"),
                        "unexpected capability {cap:?}"
                    );
                }
            }
            other => panic!("expected AgentHandshakeResponse, got {other:?}"),
        }

        let outcome = server.await.unwrap();
        assert!(outcome.is_ok(), "clean EOF is Ok(()), got {outcome:?}");
    }

    #[tokio::test]
    async fn ping_returns_pong() {
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, mut b_rx) = tokio::io::duplex(512);
        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::Ping,
                },
            )
            .await
            .unwrap();
        }

        let response = read_envelope(&mut b_rx).await.unwrap();
        assert!(matches!(response.payload, Payload::Pong));

        let _ = server.await.unwrap();
    }

    #[tokio::test]
    async fn version_mismatch_rejects_and_closes() {
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, mut b_rx) = tokio::io::duplex(512);
        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION + 1,
                    payload: Payload::Ping,
                },
            )
            .await
            .unwrap();
        }

        let response = read_envelope(&mut b_rx).await.unwrap();
        match response.payload {
            Payload::Error(info) => {
                assert!(matches!(info.kind, ErrorKind::VersionMismatch));
                assert!(info.message.contains("agent built for wire v"));
            }
            other => panic!("expected Error(VersionMismatch), got {other:?}"),
        }

        let outcome = server.await.unwrap();
        assert!(outcome.is_ok(), "version mismatch closes cleanly");
    }

    #[tokio::test]
    async fn unhandled_payload_surfaces_invalid_request() {
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, mut b_rx) = tokio::io::duplex(512);
        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            // `ListPanes` is a pty-side command; the agent doesn't own
            // pty state, so this must come back as InvalidRequest.
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::ListPanes,
                },
            )
            .await
            .unwrap();
        }

        let response = read_envelope(&mut b_rx).await.unwrap();
        match response.payload {
            Payload::Error(info) => {
                assert!(matches!(info.kind, ErrorKind::InvalidRequest));
            }
            other => panic!("expected Error(InvalidRequest), got {other:?}"),
        }

        let _ = server.await.unwrap();
    }

    #[tokio::test]
    async fn unsubscribe_unknown_id_is_silent_noop() {
        // Unsubscribe for an id we never subscribed to must not panic,
        // must not respond, must not block — it should be a quiet
        // pop-or-nothing on the sub map. Close the input right after
        // so the server exits and the test terminates.
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, _b_rx) = tokio::io::duplex(512);
        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::Unsubscribe { id: 4242 },
                },
            )
            .await
            .unwrap();
        }

        let outcome = server.await.unwrap();
        assert!(outcome.is_ok(), "unsubscribe noop exits cleanly");
    }
}
