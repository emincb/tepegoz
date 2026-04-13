//! Tepegöz wire protocol.
//!
//! Every transport (Unix socket, SSH channel, QUIC, WSS) carries the same
//! envelope on the wire:
//!
//! ```text
//! [4-byte big-endian length] [rkyv-encoded Envelope]
//! ```
//!
//! Compat policy: [`Envelope`] carries an explicit [`PROTOCOL_VERSION`].
//! Peers reject unknown versions. Validation via `bytecheck` is mandatory on
//! the network boundary; optional on the trusted local Unix socket.

use rkyv::{Archive, Deserialize, Serialize};

pub mod codec;
pub mod socket;

/// Current wire protocol version. Bumps on breaking change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Top-level framed message on every transport.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct Envelope {
    pub version: u32,
    pub payload: Payload,
}

/// All message kinds, client-originated and daemon-originated, in one enum.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub enum Payload {
    // --- client → daemon ---
    Hello(Hello),
    Ping,
    Subscribe(Subscription),
    Unsubscribe { id: u64 },

    // --- daemon → client ---
    Welcome(Welcome),
    Pong,
    Event(EventFrame),
    Error(ErrorInfo),
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub client_version: u32,
    pub client_name: String,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct Welcome {
    pub daemon_version: String,
    pub protocol_version: u32,
    pub daemon_pid: u32,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub enum Subscription {
    Status { id: u64 },
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct EventFrame {
    pub subscription_id: u64,
    pub event: Event,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub enum Event {
    Status(StatusSnapshot),
}

/// Live daemon status — sent on subscription and at 1 Hz thereafter.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct StatusSnapshot {
    pub daemon_pid: u32,
    pub daemon_version: String,
    pub started_at_unix_millis: u64,
    pub uptime_seconds: u64,
    pub clients_now: u32,
    pub clients_total: u64,
    pub events_sent: u64,
    pub socket_path: String,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct ErrorInfo {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub enum ErrorKind {
    VersionMismatch,
    UnknownSubscription,
    InvalidRequest,
    Internal,
}
