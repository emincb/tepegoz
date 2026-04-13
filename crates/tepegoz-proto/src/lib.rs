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
///
/// Bumped to 3 in Phase 3 Slice A: added `Subscription::Docker`,
/// `Event::ContainerList`, `Event::DockerUnavailable`, plus the
/// `DockerContainer`/`DockerPort`/`KeyValue` types they reference.
pub const PROTOCOL_VERSION: u32 = 3;

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
    Status {
        id: u64,
    },
    /// Subscribe to docker engine events: container list refreshes plus
    /// availability transitions. Daemon will retry connecting if docker is
    /// unreachable, so a single `Docker` subscription survives `dockerd`
    /// restarts without the client having to resubscribe.
    Docker {
        id: u64,
    },
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
    /// Full container list from the docker engine (running + stopped). Sent
    /// on initial connect and on every subsequent refresh tick.
    ContainerList {
        containers: Vec<DockerContainer>,
        engine_source: String,
    },
    /// The docker engine is not reachable. Sent once when the daemon transitions
    /// to unreachable; the daemon keeps retrying internally and will follow up
    /// with a `ContainerList` once a connection is established.
    DockerUnavailable {
        reason: String,
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

/// Docker container row delivered in `Event::ContainerList`. Lossy view of
/// `bollard::models::ContainerSummary` — fields the TUI actually renders, plus
/// labels (sorted, full set) for filter UX.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DockerContainer {
    pub id: String,
    /// Container names as reported by docker (with the leading `/`).
    pub names: Vec<String>,
    pub image: String,
    pub image_id: String,
    pub command: String,
    /// Container creation time (Unix seconds).
    pub created_unix_secs: i64,
    /// "running" | "exited" | "paused" | "created" | "restarting" | "removing" |
    /// "dead" | "unknown". The string form is intentional — it's what docker
    /// returns and what the TUI renders.
    pub state: String,
    /// Free-form short status, e.g. "Up 5 minutes" or "Exited (0) 3 hours ago".
    pub status: String,
    pub ports: Vec<DockerPort>,
    pub labels: Vec<KeyValue>,
}

/// Port mapping as reported in a container summary. `0` means "no public port"
/// (the host side is not bound).
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DockerPort {
    pub ip: String,
    pub private_port: u16,
    pub public_port: u16,
    /// "tcp" | "udp" | "sctp".
    pub protocol: String,
}

/// Generic key/value pair for label / env-style maps.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}
