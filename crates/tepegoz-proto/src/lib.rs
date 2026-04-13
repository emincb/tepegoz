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
pub const PROTOCOL_VERSION: u32 = 2;

/// Identifier for a pty pane owned by the daemon.
pub type PaneId = u64;

/// Top-level framed message on every transport.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct Envelope {
    pub version: u32,
    pub payload: Payload,
}

/// All message kinds, client-originated and daemon-originated, in one enum.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub enum Payload {
    // ---- client → daemon ----
    Hello(Hello),
    Ping,
    Subscribe(Subscription),
    Unsubscribe {
        id: u64,
    },

    // pty commands (client → daemon)
    OpenPane(OpenPaneSpec),
    AttachPane {
        pane_id: PaneId,
        subscription_id: u64,
    },
    ClosePane {
        pane_id: PaneId,
    },
    ListPanes,
    SendInput {
        pane_id: PaneId,
        data: Vec<u8>,
    },
    ResizePane {
        pane_id: PaneId,
        rows: u16,
        cols: u16,
    },

    // ---- daemon → client ----
    Welcome(Welcome),
    Pong,
    Event(EventFrame),
    PaneOpened(PaneInfo),
    PaneList {
        panes: Vec<PaneInfo>,
    },
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

/// Client-initiated subscription kinds. Each subscription has a client-chosen
/// `id`; daemon events reference that `id` so clients can multiplex many
/// subscriptions on one connection.
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
    /// Initial replay of a pane's scrollback, delivered once per AttachPane.
    PaneSnapshot {
        scrollback: Vec<u8>,
        rows: u16,
        cols: u16,
    },
    /// Live output chunk from a pane.
    PaneOutput {
        data: Vec<u8>,
    },
    /// The pane's child process has exited; the subscription is closed.
    PaneExit {
        exit_code: Option<i32>,
    },
    /// The scrollback was too large to fit since the subscription started
    /// and some bytes were dropped from the front of the ring buffer.
    PaneLagged {
        dropped_bytes: u64,
    },
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
    pub panes_open: u32,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct OpenPaneSpec {
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<EnvVar>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    pub created_at_unix_millis: u64,
    pub rows: u16,
    pub cols: u16,
    pub shell: String,
    pub alive: bool,
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
    UnknownPane,
    InvalidRequest,
    Internal,
}
