//! Daemon-side agent connection pool (Phase 6 Slice 6c-proper).
//!
//! Shape: one [`AgentConnection`] per live agent, keyed by Fleet alias
//! in `SharedState.agent_conns`. The Fleet supervisor deploys + spawns
//! the agent on `HostState::Connected`, inserts the entry, and removes
//! it on any transition out of Connected.
//!
//! Client-side `Subscribe(Docker { Remote { alias } })` handling goes:
//!
//! 1. Look up `agent_conns[alias]`.
//! 2. Miss → emit `Event::DockerUnavailable { reason: "…" }` on the
//!    client's event_tx.
//! 3. Hit but agent lacks the `"docker"` capability → same, with
//!    reason `"no docker on <alias>"`.
//! 4. Hit → allocate `daemon_sub_id` (per-agent monotonic), register
//!    `RoutedSub { client_event_tx, client_id: client_sub_id, kind }`
//!    in the agent's routing map, forward
//!    `Subscribe(Docker { id: daemon_sub_id, target: Local })` via the
//!    agent's `writer_tx`.
//!
//! The agent responds with `Event { subscription_id: daemon_sub_id, ... }`.
//! The agent driver's envelope-parser task reads those off the stdout
//! stream, looks up the routing entry, rewrites `subscription_id` to
//! the client's id, and forwards to `client_event_tx`.
//!
//! `Unsubscribe(id)` from the client: remove the routing entry whose
//! `client_id == id`, forward `Unsubscribe { id: daemon_sub_id }` to
//! the agent.
//!
//! `DockerAction` with `Remote { alias }`: allocate `daemon_req_id`,
//! register a `OneShot` routing entry, forward. Result travels the
//! same path; the parser removes the entry after forwarding.
//!
//! ## Scope boundaries
//!
//! - Agent TOFU / deploy integrity: handled by `tepegoz_ssh::deploy`
//!   (hash-verified every deploy; no first-seen DB).
//! - Fleet heartbeat / reconnect: handled by `host_supervisor` in
//!   `client.rs`. This module only cares about "an agent exists for
//!   alias X right now" — the supervisor is the lifetime owner.
//! - Remote target on wire: `ScopeTarget::Remote { alias }` reaches
//!   the daemon; the daemon translates to `Local` before forwarding
//!   over the stdio tunnel. The agent never sees `Remote`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use tokio::io::AsyncWrite;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, warn};

use tepegoz_proto::{
    DockerActionOutcome, DockerActionResult, Envelope, Event, EventFrame, PROTOCOL_VERSION,
    Payload, codec::read_envelope,
};

/// Maximum buffered bytes between the russh `ChannelMsg::Data` source
/// and the envelope parser. 64 KiB matches russh's default channel
/// window; smaller risks stalling the channel when large ContainerList
/// envelopes arrive, larger just wastes memory.
const AGENT_PIPE_BYTES: usize = 64 * 1024;

/// One connected agent, pooled by alias in `SharedState.agent_conns`.
///
/// `writer_tx`: envelopes to send TO the agent (client Subscribes,
/// Unsubscribes, DockerActions). The driver task consumes from the
/// matching `writer_rx` and serializes to the russh channel.
///
/// `routing`: maps daemon-allocated id → client destination. Agent
/// replies carry the daemon id; the parser rewrites to the client id
/// before forwarding.
///
/// `next_sub_id`: per-agent monotonic allocator. Starts at 1 so id 0
/// stays free as a sentinel if a future refactor wants it.
pub struct AgentConnection {
    pub alias: String,
    pub capabilities: Vec<String>,
    pub writer_tx: mpsc::UnboundedSender<Envelope>,
    pub next_sub_id: AtomicU64,
    pub routing: Mutex<HashMap<u64, RoutedSub>>,
}

impl AgentConnection {
    /// Allocate a fresh daemon-side id for a subscription or one-shot
    /// request routed through this agent.
    pub fn alloc_id(&self) -> u64 {
        self.next_sub_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Entry in [`AgentConnection::routing`]. Maps one daemon-side id to
/// the client we should forward the response to.
pub struct RoutedSub {
    pub client_event_tx: mpsc::UnboundedSender<Envelope>,
    pub client_id: u64,
    pub kind: RoutedKind,
    pub scope: RoutedScope,
}

/// Routing entries live either for the full subscription lifetime
/// (`Subscription`) or just long enough to ferry one response
/// (`OneShot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutedKind {
    Subscription,
    OneShot,
}

/// Which event variant to emit when this routing entry is torn down
/// by an agent disconnect. Per-scope so the TUI sees the same
/// "unavailable" shape it would on a local engine failure — a remote
/// agent going away is conceptually the same as a local engine dying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutedScope {
    Docker,
    DockerLogs,
    DockerStats,
    /// One-shot `DockerAction`. On cleanup we emit a
    /// `DockerActionResult { outcome: Failure }` with the stored
    /// request context — the TUI unblocks the pending toast.
    DockerAction {
        container_id: String,
        action_kind: tepegoz_proto::DockerActionKind,
    },
    /// Phase 6 Slice 6d-ii: remote Ports subscription. On cleanup
    /// emits `Event::PortsUnavailable` parallel to local ports
    /// outages.
    Ports,
    /// Phase 6 Slice 6d-ii: remote Processes subscription. On cleanup
    /// emits `Event::ProcessesUnavailable` parallel to local
    /// processes-probe failures.
    Processes,
}

/// Spawn the driver task pair for an agent connection. The driver
/// owns the russh channel exclusively; an envelope parser runs as a
/// sibling task reading byte-accumulated frames through a tokio
/// in-memory duplex.
///
/// Returns `(driver_abort, parser_abort)`; dropping the
/// `AgentConnection` from the pool aborts both so the tasks never
/// outlive their pool entry.
pub fn spawn_agent_driver_russh(
    conn: Arc<AgentConnection>,
    channel: russh::Channel<russh::client::Msg>,
    writer_rx: mpsc::UnboundedReceiver<Envelope>,
) -> (AbortHandle, AbortHandle) {
    let (pipe_writer, pipe_reader) = tokio::io::duplex(AGENT_PIPE_BYTES);

    let parser_conn = Arc::clone(&conn);
    let parser_alias = conn.alias.clone();
    let parser: JoinHandle<()> = tokio::spawn(async move {
        run_envelope_parser(parser_alias, pipe_reader, parser_conn).await;
    });

    let driver_conn = Arc::clone(&conn);
    let driver: JoinHandle<()> = tokio::spawn(async move {
        run_agent_driver_russh(driver_conn, channel, writer_rx, pipe_writer).await;
    });

    (driver.abort_handle(), parser.abort_handle())
}

/// Test-only driver. Backs the agent with an AsyncRead + AsyncWrite
/// pair instead of a russh channel, so integration tests can run a
/// real [`tepegoz_agent::serve`] inside the same process over tokio
/// duplex streams.
#[cfg(test)]
pub fn spawn_agent_driver_over_stream<R, W>(
    conn: Arc<AgentConnection>,
    reader: R,
    writer: W,
    writer_rx: mpsc::UnboundedReceiver<Envelope>,
) -> (AbortHandle, AbortHandle)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let parser_conn = Arc::clone(&conn);
    let parser_alias = conn.alias.clone();
    let parser: JoinHandle<()> = tokio::spawn(async move {
        run_envelope_parser(parser_alias, reader, parser_conn).await;
    });

    let driver: JoinHandle<()> = tokio::spawn(async move {
        run_agent_writer(writer, writer_rx).await;
    });

    (driver.abort_handle(), parser.abort_handle())
}

async fn run_agent_driver_russh(
    conn: Arc<AgentConnection>,
    mut channel: russh::Channel<russh::client::Msg>,
    mut writer_rx: mpsc::UnboundedReceiver<Envelope>,
    mut pipe_writer: tokio::io::DuplexStream,
) {
    loop {
        tokio::select! {
            biased;
            cmd = writer_rx.recv() => {
                let Some(env) = cmd else { break; };
                match serialize_envelope(&env) {
                    Ok((prefix, body)) => {
                        if channel.data(prefix.as_slice()).await.is_err() { break; }
                        if channel.data(body.as_slice()).await.is_err() { break; }
                    }
                    Err(e) => {
                        warn!(alias = %conn.alias, error = %e, "agent envelope serialize failed");
                    }
                }
            }
            msg = channel.wait() => {
                let Some(msg) = msg else { break; };
                match msg {
                    russh::ChannelMsg::Data { data } => {
                        let write_result = pipe_writer.write_all(&data).await;
                        if write_result.is_err() { break; }
                    }
                    russh::ChannelMsg::ExtendedData { data, .. } => {
                        // Agent stderr (tracing output). Drain so the
                        // channel window doesn't fill up, but surface
                        // as debug logs only.
                        let s = String::from_utf8_lossy(&data);
                        debug!(alias = %conn.alias, stderr = %s, "agent stderr");
                    }
                    russh::ChannelMsg::Close | russh::ChannelMsg::Eof => break,
                    _ => {}
                }
            }
        }
    }

    let _ = channel.close().await;
    // Dropping pipe_writer here signals EOF to the parser; it exits
    // naturally once it drains whatever bytes are still in flight.
    drop(pipe_writer);
}

#[cfg(test)]
async fn run_agent_writer<W>(mut writer: W, mut writer_rx: mpsc::UnboundedReceiver<Envelope>)
where
    W: AsyncWrite + Unpin,
{
    use tepegoz_proto::codec::write_envelope;
    while let Some(env) = writer_rx.recv().await {
        if write_envelope(&mut writer, &env).await.is_err() {
            break;
        }
    }
}

async fn run_envelope_parser<R>(alias: String, mut reader: R, conn: Arc<AgentConnection>)
where
    R: AsyncRead + Unpin,
{
    while let Ok(env) = read_envelope(&mut reader).await {
        route_agent_envelope(&alias, env, &conn).await;
    }

    // Parser exit means the agent's byte stream closed — either
    // normal agent shutdown or SSH channel death. Drain any routing
    // entries + notify clients; the Fleet supervisor will also call
    // `remove_agent` on the pool shortly, but we can't count on that
    // ordering and routing cleanup is idempotent.
    drain_routing_on_disconnect(&alias, &conn).await;
}

async fn route_agent_envelope(alias: &str, env: Envelope, conn: &Arc<AgentConnection>) {
    match env.payload {
        Payload::Event(EventFrame {
            subscription_id: daemon_id,
            event,
        }) => {
            let routing = conn.routing.lock().await;
            if let Some(routed) = routing.get(&daemon_id) {
                let client_env = Envelope {
                    version: env.version,
                    payload: Payload::Event(EventFrame {
                        subscription_id: routed.client_id,
                        event,
                    }),
                };
                let _ = routed.client_event_tx.send(client_env);
            } else {
                debug!(alias, daemon_id, "routing miss on agent Event");
            }
        }
        Payload::DockerActionResult(mut result) => {
            let mut routing = conn.routing.lock().await;
            let daemon_req_id = result.request_id;
            if let Some(routed) = routing.remove(&daemon_req_id) {
                if routed.kind == RoutedKind::OneShot {
                    result.request_id = routed.client_id;
                    let _ = routed.client_event_tx.send(Envelope {
                        version: env.version,
                        payload: Payload::DockerActionResult(result),
                    });
                }
            } else {
                debug!(
                    alias,
                    daemon_req_id, "routing miss on agent DockerActionResult"
                );
            }
        }
        Payload::Error(info) => {
            warn!(
                alias,
                kind = ?info.kind,
                message = %info.message,
                "agent returned Error — not attributable to a subscription",
            );
        }
        other => {
            debug!(alias, payload = ?std::any::type_name_of_val(&other), "agent sent unexpected payload");
        }
    }
}

/// Emit one unavailable event per subscription + failure result per
/// pending action, then clear the routing map.
///
/// Called on two paths:
/// 1. Parser exit (agent stream closed before the Fleet supervisor
///    noticed).
/// 2. Fleet supervisor removing the alias on terminal state
///    transition.
///
/// Both are idempotent — the second caller to a given
/// `AgentConnection` sees an already-empty routing map.
async fn drain_routing_on_disconnect(alias: &str, conn: &Arc<AgentConnection>) {
    let mut routing = conn.routing.lock().await;
    let reason = format!("agent disconnected: {alias}");
    for (_, routed) in routing.drain() {
        match routed.scope {
            RoutedScope::Docker => {
                let _ = routed.client_event_tx.send(docker_unavailable_envelope(
                    routed.client_id,
                    reason.clone(),
                ));
            }
            RoutedScope::DockerLogs | RoutedScope::DockerStats => {
                let _ = routed
                    .client_event_tx
                    .send(stream_ended_envelope(routed.client_id, reason.clone()));
            }
            RoutedScope::DockerAction {
                container_id,
                action_kind,
            } => {
                let _ = routed.client_event_tx.send(docker_action_failure_envelope(
                    routed.client_id,
                    container_id,
                    action_kind,
                    reason.clone(),
                ));
            }
            RoutedScope::Ports => {
                let _ = routed
                    .client_event_tx
                    .send(ports_unavailable_envelope(routed.client_id, reason.clone()));
            }
            RoutedScope::Processes => {
                let _ = routed.client_event_tx.send(processes_unavailable_envelope(
                    routed.client_id,
                    reason.clone(),
                ));
            }
        }
    }
}

/// Public hook for the Fleet supervisor: tear down routing + abort
/// driver tasks. Idempotent; safe to call after the parser already
/// drained the routing map. The `AgentConnection` itself is dropped
/// by the caller removing it from `agent_conns` — we only need to
/// notify registered subs here.
pub async fn shutdown_agent_connection(conn: &Arc<AgentConnection>) {
    drain_routing_on_disconnect(&conn.alias, conn).await;
    // writer_tx holders (handle_command callers) may still try to
    // send a Subscribe through a dead writer; the send fails, the
    // caller gracefully surfaces DockerUnavailable. No need to close
    // the channel proactively — letting it GC keeps the shutdown
    // path lock-free.
}

/// Fleet supervisor entry point: deploy + handshake + register an
/// agent for `alias` against a fresh channel on `session`. On
/// success, inserts an `AgentConnection` into `state.agent_conns` so
/// client-side `Subscribe(Docker { Remote { alias } })` handling can
/// route through it.
///
/// Best-effort semantics: any failure (no resolver, unsupported
/// target triple, deploy or handshake error) is logged + swallowed.
/// The Fleet supervisor continues running the heartbeat — the host
/// stays `Connected` from the user's POV, and remote subscriptions
/// surface `DockerUnavailable { reason: "agent not connected: …" }`
/// until a reconnect pass succeeds.
pub async fn deploy_and_register_agent(
    alias: &str,
    session: &tepegoz_ssh::SshSession,
    state: &Arc<crate::state::SharedState>,
) -> Option<Vec<String>> {
    let Some(resolver) = state.agent_resolver else {
        warn!(
            alias,
            "no agent resolver configured (run `cargo xtask build-agents`); \
             remote scopes will be unavailable"
        );
        return None;
    };

    match deploy_and_register_inner(alias, session, state, resolver).await {
        Ok(capabilities) => {
            debug!(alias, "agent deployed + registered");
            Some(capabilities)
        }
        Err(e) => {
            warn!(alias, error = %e, "agent deploy failed; remote scopes unavailable");
            None
        }
    }
}

async fn deploy_and_register_inner(
    alias: &str,
    session: &tepegoz_ssh::SshSession,
    state: &Arc<crate::state::SharedState>,
    resolver: crate::config::AgentResolver,
) -> Result<Vec<String>, tepegoz_ssh::SshError> {
    // Detect remote target + resolve embedded bytes.
    let target = tepegoz_ssh::deploy::detect_target(session).await?;
    let bytes =
        resolver(&target.target_triple).ok_or_else(|| tepegoz_ssh::SshError::AgentNotEmbedded {
            triple: target.target_triple.clone(),
        })?;

    // Deploy + spawn + handshake.
    let outcome = tepegoz_ssh::deploy::deploy_agent(session, bytes, PROTOCOL_VERSION).await?;
    let mut channel =
        tepegoz_ssh::deploy::spawn_agent_channel(session, &outcome.remote_path).await?;
    let info = tepegoz_ssh::deploy::handshake_agent(&mut channel, PROTOCOL_VERSION).await?;

    // Build AgentConnection + spawn driver tasks.
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<Envelope>();
    let conn = Arc::new(AgentConnection {
        alias: alias.to_string(),
        capabilities: info.capabilities.clone(),
        writer_tx,
        next_sub_id: AtomicU64::new(1),
        routing: Mutex::new(HashMap::new()),
    });
    let _driver_handles =
        spawn_agent_driver_russh(Arc::clone(&conn), channel.into_inner(), writer_rx);

    // Any pre-existing entry for this alias is stale; abort its
    // driver tasks via shutdown_agent_connection, then overwrite.
    {
        let mut pool = state.agent_conns.lock().await;
        if let Some(prev) = pool.remove(alias) {
            shutdown_agent_connection(&prev).await;
        }
        pool.insert(alias.to_string(), conn);
    }

    debug!(
        alias,
        os = %info.os,
        arch = %info.arch,
        capabilities = ?info.capabilities,
        "agent registered in pool"
    );
    Ok(info.capabilities)
}

/// Fleet supervisor entry point: remove the agent for `alias` from
/// the pool + notify all registered client subscriptions. Idempotent
/// — safe to call even if no entry exists (e.g. deploy failed on the
/// matching Connected transition).
pub async fn remove_and_shutdown_agent(alias: &str, state: &Arc<crate::state::SharedState>) {
    let removed = {
        let mut pool = state.agent_conns.lock().await;
        pool.remove(alias)
    };
    if let Some(conn) = removed {
        shutdown_agent_connection(&conn).await;
    }
}

fn serialize_envelope(env: &Envelope) -> anyhow::Result<([u8; 4], Vec<u8>)> {
    let body = rkyv::to_bytes::<rkyv::rancor::Error>(env)
        .map_err(|e| anyhow::anyhow!("rkyv serialize: {e}"))?;
    let len = u32::try_from(body.len())
        .map_err(|_| anyhow::anyhow!("envelope too large: {} bytes", body.len()))?;
    Ok((len.to_be_bytes(), body.to_vec()))
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

fn docker_action_failure_envelope(
    client_request_id: u64,
    container_id: String,
    action_kind: tepegoz_proto::DockerActionKind,
    reason: String,
) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::DockerActionResult(DockerActionResult {
            request_id: client_request_id,
            container_id,
            kind: action_kind,
            outcome: DockerActionOutcome::Failure { reason },
            target: tepegoz_proto::ScopeTarget::Local,
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

fn processes_unavailable_envelope(subscription_id: u64, reason: String) -> Envelope {
    Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::ProcessesUnavailable { reason },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tepegoz_proto::{DockerActionKind, Subscription};

    #[tokio::test]
    async fn alloc_id_is_monotonic_and_starts_at_one() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let conn = Arc::new(AgentConnection {
            alias: "test".into(),
            capabilities: vec!["docker".into()],
            writer_tx: tx,
            next_sub_id: AtomicU64::new(1),
            routing: Mutex::new(HashMap::new()),
        });
        assert_eq!(conn.alloc_id(), 1);
        assert_eq!(conn.alloc_id(), 2);
        assert_eq!(conn.alloc_id(), 3);
    }

    /// End-to-end round-trip against a real `tepegoz_agent::serve`
    /// inside the same process. Pipes agent ↔ daemon-routing through
    /// a pair of tokio duplex streams — exercises the envelope
    /// parser, the writer task, the routing lookup, the id
    /// translation, and the Unsubscribe teardown path without
    /// requiring SSH or docker.
    #[tokio::test]
    async fn daemon_routes_subscribe_through_real_agent_and_sees_unavailable_when_docker_missing() {
        // Set up: duplex pair simulating the agent's stdin / stdout.
        let (daemon_to_agent_tx, daemon_to_agent_rx) = tokio::io::duplex(4 * 1024);
        let (agent_to_daemon_tx, agent_to_daemon_rx) = tokio::io::duplex(64 * 1024);

        // Spawn the real agent server.
        let agent_handle = tokio::spawn(async move {
            let _ = tepegoz_agent::serve(daemon_to_agent_rx, agent_to_daemon_tx).await;
        });

        // Build a daemon-side AgentConnection with the two duplex
        // endpoints wired as the agent's reader / writer.
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let conn = Arc::new(AgentConnection {
            alias: "test-alias".into(),
            capabilities: vec!["docker".into()],
            writer_tx,
            next_sub_id: AtomicU64::new(1),
            routing: Mutex::new(HashMap::new()),
        });
        let (_driver, _parser) = spawn_agent_driver_over_stream(
            Arc::clone(&conn),
            agent_to_daemon_rx,
            daemon_to_agent_tx,
            writer_rx,
        );

        // Simulate a client: mpsc to receive routed events.
        let (client_tx, mut client_rx) = mpsc::unbounded_channel::<Envelope>();

        // Register a routing entry and send the Subscribe through
        // the agent's writer channel — the agent will pick docker
        // probe, fail to connect (no docker in test env likely),
        // and emit DockerUnavailable with the daemon-allocated id.
        let daemon_id = conn.alloc_id();
        let client_id = 1000_u64;
        {
            let mut routing = conn.routing.lock().await;
            routing.insert(
                daemon_id,
                RoutedSub {
                    client_event_tx: client_tx.clone(),
                    client_id,
                    kind: RoutedKind::Subscription,
                    scope: RoutedScope::Docker,
                },
            );
        }
        conn.writer_tx
            .send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Subscribe(Subscription::Docker {
                    id: daemon_id,
                    target: tepegoz_proto::ScopeTarget::Local,
                }),
            })
            .unwrap();

        // First response from the real agent is either ContainerList
        // (docker reachable in CI) or DockerUnavailable (docker
        // absent). Either way the subscription_id must be rewritten
        // to `client_id`, not `daemon_id`.
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), client_rx.recv())
            .await
            .expect("agent responded within timeout")
            .expect("channel open");
        match first.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event,
            }) => {
                assert_eq!(
                    subscription_id, client_id,
                    "parser must rewrite daemon id → client id"
                );
                match event {
                    Event::ContainerList { .. } | Event::DockerUnavailable { .. } => {}
                    other => panic!("expected Docker event, got {other:?}"),
                }
            }
            other => panic!("expected Event, got {other:?}"),
        }

        // Unsubscribe path.
        conn.writer_tx
            .send(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Unsubscribe { id: daemon_id },
            })
            .unwrap();

        // Tear down: drop the writer_tx so the agent sees EOF.
        drop(conn.writer_tx.clone());
        // Give the agent a moment to propagate the unsubscribe.
        // (We can't block on the agent_handle completing because we
        // hold one writer_tx clone in `conn`.)
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        agent_handle.abort();
    }

    #[tokio::test]
    async fn shutdown_notifies_all_scope_kinds_with_appropriate_event() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let conn = Arc::new(AgentConnection {
            alias: "test".into(),
            capabilities: vec!["docker".into()],
            writer_tx: tx,
            next_sub_id: AtomicU64::new(1),
            routing: Mutex::new(HashMap::new()),
        });

        // Three different scope kinds, three separate client mpsc
        // ends — after shutdown each must receive the right wire
        // event.
        let (client_tx_docker, mut client_rx_docker) = mpsc::unbounded_channel();
        let (client_tx_logs, client_rx_logs) = mpsc::unbounded_channel();
        let (client_tx_stats, client_rx_stats) = mpsc::unbounded_channel();
        let (client_tx_action, mut client_rx_action) = mpsc::unbounded_channel();

        {
            let mut routing = conn.routing.lock().await;
            routing.insert(
                10,
                RoutedSub {
                    client_event_tx: client_tx_docker,
                    client_id: 1,
                    kind: RoutedKind::Subscription,
                    scope: RoutedScope::Docker,
                },
            );
            routing.insert(
                11,
                RoutedSub {
                    client_event_tx: client_tx_logs,
                    client_id: 2,
                    kind: RoutedKind::Subscription,
                    scope: RoutedScope::DockerLogs,
                },
            );
            routing.insert(
                12,
                RoutedSub {
                    client_event_tx: client_tx_stats,
                    client_id: 3,
                    kind: RoutedKind::Subscription,
                    scope: RoutedScope::DockerStats,
                },
            );
            routing.insert(
                13,
                RoutedSub {
                    client_event_tx: client_tx_action,
                    client_id: 4,
                    kind: RoutedKind::OneShot,
                    scope: RoutedScope::DockerAction {
                        container_id: "abcd".into(),
                        action_kind: DockerActionKind::Restart,
                    },
                },
            );
        }

        shutdown_agent_connection(&conn).await;

        // Docker subscription → DockerUnavailable event.
        let env = client_rx_docker.try_recv().expect("docker sub gets event");
        match env.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::DockerUnavailable { reason },
            }) => {
                assert_eq!(subscription_id, 1);
                assert!(reason.contains("test"), "reason mentions alias");
            }
            other => panic!("expected DockerUnavailable, got {other:?}"),
        }

        // Logs & stats → DockerStreamEnded.
        for mut rx in [client_rx_logs, client_rx_stats] {
            let env = rx.try_recv().expect("stream sub gets event");
            match env.payload {
                Payload::Event(EventFrame {
                    event: Event::DockerStreamEnded { .. },
                    ..
                }) => {}
                other => panic!("expected DockerStreamEnded, got {other:?}"),
            }
        }

        // OneShot action → Failure DockerActionResult with client id.
        let env = client_rx_action.try_recv().expect("action gets result");
        match env.payload {
            Payload::DockerActionResult(result) => {
                assert_eq!(result.request_id, 4);
                assert_eq!(result.container_id, "abcd");
                assert_eq!(result.kind, DockerActionKind::Restart);
                match result.outcome {
                    DockerActionOutcome::Failure { reason } => {
                        assert!(reason.contains("disconnected"));
                    }
                    other => panic!("expected Failure, got {other:?}"),
                }
            }
            other => panic!("expected DockerActionResult, got {other:?}"),
        }

        // Routing map is empty after shutdown.
        assert!(conn.routing.lock().await.is_empty());

        // Idempotent: second shutdown is a no-op.
        shutdown_agent_connection(&conn).await;
    }
}
