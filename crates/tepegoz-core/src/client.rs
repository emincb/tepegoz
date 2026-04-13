//! Per-client handler: handshake, command dispatch, status event stream.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info};

use tepegoz_proto::{
    Envelope, Event, EventFrame, PROTOCOL_VERSION, Payload, Subscription, Welcome,
    codec::{read_envelope, write_envelope},
};

use crate::state::{DAEMON_VERSION, SharedState};

pub(crate) async fn handle_client(
    stream: UnixStream,
    state: Arc<SharedState>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    let total = state.clients_total.fetch_add(1, Ordering::Relaxed) + 1;
    let now = state.clients_now.fetch_add(1, Ordering::Relaxed) + 1;
    info!(client_no = total, concurrent = now, "client connected");

    let result = session(&mut reader, &mut writer, &state).await;

    state.clients_now.fetch_sub(1, Ordering::Relaxed);
    info!(
        remaining = state.clients_now.load(Ordering::Relaxed),
        "client disconnected"
    );

    result
}

async fn session(
    reader: &mut tokio::net::unix::OwnedReadHalf,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    state: &SharedState,
) -> anyhow::Result<()> {
    // ---- handshake ----
    let hello = read_envelope(reader).await?;
    match &hello.payload {
        Payload::Hello(h) => {
            debug!(client = %h.client_name, version = h.client_version, "client hello");
        }
        other => {
            anyhow::bail!("expected Hello, got {other:?}");
        }
    }

    let welcome = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Welcome(Welcome {
            daemon_version: DAEMON_VERSION.to_string(),
            protocol_version: PROTOCOL_VERSION,
            daemon_pid: state.daemon_pid,
        }),
    };
    write_envelope(writer, &welcome).await?;
    state.events_sent.fetch_add(1, Ordering::Relaxed);

    // ---- main loop ----
    let mut status_sub: Option<u64> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(1000));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick(), if status_sub.is_some() => {
                let id = status_sub.expect("guarded by if");
                send_status(writer, state, id).await?;
            }

            msg = read_envelope(reader) => {
                let envelope = msg?;
                match envelope.payload {
                    Payload::Ping => {
                        let pong = Envelope { version: PROTOCOL_VERSION, payload: Payload::Pong };
                        write_envelope(writer, &pong).await?;
                        state.events_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Payload::Subscribe(Subscription::Status { id }) => {
                        status_sub = Some(id);
                        // Fire immediate snapshot so the client renders without a 1s wait.
                        send_status(writer, state, id).await?;
                    }
                    Payload::Unsubscribe { id } => {
                        if status_sub == Some(id) {
                            status_sub = None;
                        }
                    }
                    Payload::Hello(_) => {} // ignore re-hellos
                    _ => {
                        debug!("ignoring unsupported client payload");
                    }
                }
            }
        }
    }
}

async fn send_status(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    state: &SharedState,
    subscription_id: u64,
) -> anyhow::Result<()> {
    let snapshot = state.snapshot();
    let ev = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Event(EventFrame {
            subscription_id,
            event: Event::Status(snapshot),
        }),
    };
    write_envelope(writer, &ev).await?;
    state.events_sent.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
