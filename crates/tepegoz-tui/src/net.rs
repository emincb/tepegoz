//! Connection task + blocking input loop.

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

use crate::app::{AppEvent, ConnectionState};

const STATUS_SUB_ID: u64 = 1;

pub(crate) async fn run_connection(path: PathBuf, tx: UnboundedSender<AppEvent>) {
    let _ = tx.send(AppEvent::ConnectionState(ConnectionState::Connecting));

    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("connect: {e}");
            warn!(%e, "connect failed");
            let _ = tx.send(AppEvent::ConnectionLost(msg));
            return;
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    // ---- handshake ----
    let hello = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Hello(Hello {
            client_version: PROTOCOL_VERSION,
            client_name: "tepegoz-tui".to_string(),
        }),
    };
    if let Err(e) = write_envelope(&mut writer, &hello).await {
        let _ = tx.send(AppEvent::ConnectionLost(format!("hello: {e}")));
        return;
    }

    let welcome = match read_envelope(&mut reader).await {
        Ok(env) => env,
        Err(e) => {
            let _ = tx.send(AppEvent::ConnectionLost(format!("welcome: {e}")));
            return;
        }
    };
    debug!(?welcome, "received welcome");

    let _ = tx.send(AppEvent::ConnectionState(ConnectionState::Connected));

    // ---- subscribe to status ----
    let sub = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Subscribe(Subscription::Status { id: STATUS_SUB_ID }),
    };
    if let Err(e) = write_envelope(&mut writer, &sub).await {
        let _ = tx.send(AppEvent::ConnectionLost(format!("subscribe: {e}")));
        return;
    }

    // ---- event loop ----
    loop {
        match read_envelope(&mut reader).await {
            Ok(env) => match env.payload {
                Payload::Event(EventFrame {
                    event: Event::Status(snap),
                    ..
                }) => {
                    if tx.send(AppEvent::Status(snap)).is_err() {
                        return;
                    }
                }
                Payload::Pong | Payload::Welcome(_) => {}
                other => {
                    debug!(?other, "unexpected daemon payload");
                }
            },
            Err(e) => {
                let _ = tx.send(AppEvent::ConnectionLost(format!("{e}")));
                return;
            }
        }
    }
}

pub(crate) fn input_loop(tx: &UnboundedSender<AppEvent>) {
    use crossterm::event::{self, Event as CtEvent, KeyEventKind};

    loop {
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(CtEvent::Key(k)) if k.kind == KeyEventKind::Press => {
                    if tx.send(AppEvent::Key(k.code)).is_err() {
                        return;
                    }
                }
                Ok(CtEvent::Resize(_, _)) => {
                    if tx.send(AppEvent::Redraw).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            },
            Ok(false) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}
