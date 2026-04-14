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
/// Version history:
/// - **v3 (Phase 3 Slice A)**: `Subscription::Docker`, `Event::ContainerList`,
///   `Event::DockerUnavailable`, `DockerContainer`/`DockerPort`/`KeyValue`.
/// - **v4 (Phase 3 Slice B)**: `Subscription::DockerLogs` /
///   `Subscription::DockerStats`, `Payload::DockerAction` +
///   `Payload::DockerActionResult`, `Event::ContainerLog` /
///   `Event::ContainerStats` / `Event::DockerStreamEnded`, plus
///   `DockerActionKind`, `DockerActionOutcome`, `LogStream`, `DockerStats`.
/// - **v5 (Phase 4 Slice 4a)**: `Subscription::Ports`, `Event::PortList`,
///   `Event::PortsUnavailable`, `ProbePort`. Daemon-side port → process →
///   container correlation delivers pre-joined rows so clients stay dumb.
/// - **v6 (Phase 4 Slice 4b)**: `Subscription::Processes`,
///   `Event::ProcessList`, `Event::ProcessesUnavailable`, `ProbeProcess`.
///   `cpu_percent: Option<f32>` — `None` on the first sample after
///   subscription (sysinfo has no prior delta to compute against); the TUI
///   renders `None` as an em-dash rather than `0.0%` to disambiguate
///   "not-yet-measured" from "idle". `start_time_unix_secs` pairs with
///   `pid` to form a stable identity for selection persistence under pid
///   reuse.
pub const PROTOCOL_VERSION: u32 = 8;

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

    // docker commands (client → daemon)
    /// One-shot lifecycle action against a container. Daemon replies with a
    /// matching `DockerActionResult` carrying the same `request_id`.
    DockerAction(DockerActionRequest),

    // fleet commands (client → daemon)
    /// Request the daemon's Fleet supervisor to dial or hang up a host
    /// by alias. Daemon replies with a matching `FleetActionResult`
    /// carrying the same `request_id`. Mirrors the `DockerAction`
    /// shape for pending-action correlation + toast UX.
    FleetAction(FleetActionRequest),

    // ---- daemon → client ----
    Welcome(Welcome),
    Pong,
    Event(EventFrame),
    PaneOpened(PaneInfo),
    PaneList {
        panes: Vec<PaneInfo>,
    },
    /// Response to a `DockerAction` command. `request_id` mirrors the
    /// originating request so clients can multiplex multiple in-flight actions.
    DockerActionResult(DockerActionResult),
    /// Response to a `FleetAction` command. `request_id` mirrors the
    /// originating request. Success means "dispatched to the
    /// supervisor" — actual connection outcome arrives via
    /// `HostStateChanged` events, not this reply.
    FleetActionResult(FleetActionResult),
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
    /// Stream a single container's logs. `tail_lines = 0` means "all". When
    /// `follow = true` the subscription stays live until cancelled, the
    /// container exits, or the engine becomes unreachable; on terminal
    /// conditions the daemon emits `Event::DockerStreamEnded`.
    DockerLogs {
        id: u64,
        container_id: String,
        follow: bool,
        tail_lines: u32,
    },
    /// Stream a single container's stats (CPU%, memory). Periodic events
    /// every ~1 s while the container is alive. Like docker logs, the
    /// stream terminates with `DockerStreamEnded`.
    DockerStats {
        id: u64,
        container_id: String,
    },
    /// Subscribe to listening-port events: per-refresh `PortList` events with
    /// pid / process-name / container correlation. On probe failure the
    /// daemon emits a single `PortsUnavailable` transition event and retries
    /// internally, so one `Ports` subscription survives transient failures
    /// without the client having to resubscribe.
    Ports {
        id: u64,
    },
    /// Subscribe to running-process events: per-refresh `ProcessList` events
    /// with pid, parent, command, cpu%, mem. On probe failure the daemon
    /// emits a single `ProcessesUnavailable` transition event and retries
    /// internally. The first `ProcessList` after subscription carries
    /// `cpu_percent: None` for every row because sysinfo needs a prior
    /// delta to compute CPU% against.
    Processes {
        id: u64,
    },
    /// Subscribe to SSH fleet events: per-host state transitions plus an
    /// initial `HostList`. Phase 5 Slice 5b ships the discovery + tile
    /// wiring; Slice 5c ships the per-host connection supervisor that
    /// actually drives the state machine. Between the two, every host
    /// remains `HostState::Disconnected` — same degrade-gracefully shape
    /// as Phase 3's `DockerUnavailable`.
    Fleet {
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
    /// One chunk of container log output (delivered under a `DockerLogs`
    /// subscription).
    ContainerLog {
        stream: LogStream,
        data: Vec<u8>,
    },
    /// One sample of container resource usage (delivered under a
    /// `DockerStats` subscription).
    ContainerStats(DockerStats),
    /// Streaming subscription terminated. Reason is human-readable; common
    /// causes are container exit, container removal, or engine going away.
    /// After this event the daemon will not emit further events on this
    /// subscription id (it's effectively closed; client may unsubscribe to
    /// free local state, but doesn't have to).
    DockerStreamEnded {
        reason: String,
    },
    /// Full list of listening ports with process + container correlation.
    /// Delivered under a `Ports` subscription on every poll cycle.
    PortList {
        ports: Vec<ProbePort>,
        /// Which native probe produced this list, e.g. `"linux-procfs"`,
        /// `"macos-libproc"`, or `"fallback-sysinfo"`. Non-empty so clients
        /// can surface it in the tile footer (mirrors `engine_source` on
        /// `ContainerList`).
        source: String,
    },
    /// The ports probe is unavailable. Sent once on the transition from
    /// available (or initial) to unavailable — not on every retry. The
    /// daemon retries internally and will follow up with a `PortList` once
    /// the probe succeeds again.
    PortsUnavailable {
        reason: String,
    },
    /// Full list of running processes with pid + parent + command + cpu% +
    /// memory. Delivered under a `Processes` subscription on every poll
    /// cycle.
    ProcessList {
        rows: Vec<ProbeProcess>,
        /// Which probe produced this list, e.g. `"sysinfo"`. Non-empty so
        /// clients can surface it in the tile footer (mirrors
        /// `engine_source` on `ContainerList` and `source` on `PortList`).
        source: String,
    },
    /// The processes probe is unavailable. Sent once on the transition from
    /// available (or initial) to unavailable — not on every retry. The
    /// daemon retries internally and will follow up with a `ProcessList`
    /// once the probe succeeds again.
    ProcessesUnavailable {
        reason: String,
    },
    /// Full list of configured SSH hosts. Delivered once at subscribe
    /// time; further changes (e.g. user edits ssh_config live) are out
    /// of scope for v1. The `source` string labels which precedence
    /// layer produced this list — rendered in the Fleet-tile footer
    /// when it's an override (tepegoz config.toml or env), hidden when
    /// the source is the user's ssh_config.
    HostList {
        hosts: Vec<HostEntry>,
        source: String,
    },
    /// A single host's connection state changed. Delivered under a
    /// `Fleet` subscription. 5b emits one `HostStateChanged { state:
    /// Disconnected }` per host right after the initial `HostList` —
    /// 5c replaces that with real connection-supervisor transitions
    /// through Connecting → Connected → Degraded → Disconnected.
    ///
    /// `reason` is populated only for terminal `⚠` states
    /// (`AuthFailed`, `HostKeyMismatch`, Phase 6's `AgentNotDeployed` /
    /// `AgentVersionMismatch`). For transient states
    /// (`Disconnected`/`Connecting`/`Connected`/`Degraded`) it is
    /// always `None`. Clients render the reason as a red toast on the
    /// transition into terminal; logs, error messages, or hovering
    /// the Fleet row surfaces the field at the tile level.
    HostStateChanged {
        alias: String,
        state: HostState,
        reason: Option<String>,
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

/// Lifecycle operations a client can request against a container.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockerActionKind {
    Start,
    Stop,
    Restart,
    /// Send SIGKILL (or container's stop signal if explicitly configured).
    Kill,
    /// Force-remove the container, including running ones.
    Remove,
}

/// One-shot lifecycle command.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct DockerActionRequest {
    /// Client-assigned id mirrored back in the response. Lets the client
    /// match a response to its in-flight request without holding the only
    /// outbound socket lock.
    pub request_id: u64,
    pub container_id: String,
    pub kind: DockerActionKind,
}

/// Daemon's reply to a `DockerActionRequest`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct DockerActionResult {
    pub request_id: u64,
    pub container_id: String,
    pub kind: DockerActionKind,
    pub outcome: DockerActionOutcome,
}

/// Result of a docker lifecycle action.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum DockerActionOutcome {
    Success,
    /// The action failed. `reason` carries the error verbatim from bollard
    /// (and ultimately from dockerd) — surface it to the user; don't try to
    /// classify it on the wire.
    Failure {
        reason: String,
    },
}

/// Which docker log stream a `ContainerLog` chunk came from.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// One sample of container resource usage. Computed by the daemon from the
/// raw bollard stats response; clients render this directly.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DockerStats {
    /// CPU usage as a percentage (0.0..=N*100 where N = number of cores).
    /// Computed from cpu_stats vs precpu_stats deltas using the standard
    /// docker stats CLI formula. `0.0` if a delta could not be calculated
    /// (e.g. first sample, or precpu missing on Windows).
    pub cpu_percent: f32,
    /// Current memory usage in bytes.
    pub mem_bytes: u64,
    /// Container memory limit in bytes; `0` if no limit (unconstrained — use
    /// host total memory if you want to compute a percent).
    pub mem_limit_bytes: u64,
}

/// A single listening-socket row delivered in `Event::PortList`.
///
/// Produced by `tepegoz-probe` (native per-OS implementation) and optionally
/// correlated with a Docker container by the daemon before being sent to the
/// client. Clients render this directly.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProbePort {
    /// Local bind address. `"0.0.0.0"` / `"::"` for wildcards, else a
    /// specific IPv4/IPv6 string.
    pub local_ip: String,
    /// Local bind port.
    pub local_port: u16,
    /// `"tcp"` or `"udp"`. Matches the convention of `DockerPort::protocol`.
    pub protocol: String,
    /// Owning process id. `0` if the probe could not determine the owner
    /// (usually means insufficient privilege — see `partial`).
    pub pid: u32,
    /// Short process name, e.g. `"nginx"`, `"bun"`. Empty string if unknown.
    pub process_name: String,
    /// Docker container id (short or long form acceptable) if the port is
    /// bound by a container and the daemon could correlate it. `None` for
    /// non-containerized listeners, when Docker is unreachable, or when the
    /// platform cannot correlate (e.g. macOS without Docker Desktop's VM
    /// exposing the binding through bollard).
    pub container_id: Option<String>,
    /// `true` if the probe couldn't fill every field (pid/process_name/
    /// container_id may be empty or `None`). Typically means insufficient
    /// privilege. TUI renders partial rows with a visual cue so the user
    /// knows to elevate for the full view.
    pub partial: bool,
}

/// A single running-process row delivered in `Event::ProcessList`.
///
/// Produced by `tepegoz-probe`'s `ProcessesProbe` (sysinfo-backed on both
/// Linux and macOS). Clients render this directly.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ProbeProcess {
    /// Owning process id.
    pub pid: u32,
    /// Parent pid. `0` when there is no parent (PID 1 / init) or the probe
    /// couldn't resolve it.
    pub parent_pid: u32,
    /// Process start time (Unix seconds). Pairs with `pid` to form a stable
    /// identity for selection persistence: a short-lived process whose pid
    /// is reused by a later process has a different `start_time_unix_secs`,
    /// so the TUI's selection logic can detect the swap rather than
    /// silently re-targeting.
    pub start_time_unix_secs: i64,
    /// Full command line when available (argv joined by spaces), falling
    /// back to the short process name. Empty string when even the name
    /// couldn't be read — pairs with `partial: true`.
    pub command: String,
    /// CPU usage over the last refresh interval (~2 s), expressed as a
    /// percentage in `0.0..=N*100` where `N` is the number of CPU cores.
    /// `None` on the first `ProcessList` event after subscription because
    /// sysinfo has no prior delta to compute against. The TUI renders
    /// `None` as an em-dash so "not yet measured" is visually distinct
    /// from "measured as zero / idle". Subsequent events carry `Some(x)`.
    pub cpu_percent: Option<f32>,
    /// Resident memory in bytes at the sample instant.
    pub mem_bytes: u64,
    /// `true` if the probe couldn't fill every field (most often `command`
    /// was empty because `/proc/<pid>/cmdline` or the libproc equivalent
    /// was unreadable). TUI renders partial rows with a visual cue so the
    /// user knows to elevate for the full view.
    pub partial: bool,
}

/// A single configured SSH host. Produced by `tepegoz-ssh`'s host
/// discovery (ssh_config / tepegoz config.toml / env) and delivered to
/// clients via `Event::HostList`.
///
/// `identity_files` carries display strings rather than `PathBuf` so the
/// wire shape stays stable across OSes (rkyv doesn't archive `PathBuf`
/// natively) and so the user can eyeball the list in `tepegoz doctor
/// --ssh-hosts`. The consuming side maps each entry through `PathBuf`
/// just before calling `load_secret_key`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// Lookup alias — `tepegoz connect <alias>` and the Fleet tile use this.
    pub alias: String,
    /// DNS name or IP literally dialed.
    pub hostname: String,
    /// Remote user — resolved from `User` in ssh_config or the current
    /// username when unset.
    pub user: String,
    /// TCP port. Defaults to 22.
    pub port: u16,
    /// `IdentityFile` entries in ssh_config declaration order. Empty
    /// when the host relies solely on an SSH agent.
    pub identity_files: Vec<String>,
    /// `ProxyJump` alias (if set). Carried forward unused in Phase 5 —
    /// the daemon surfaces a clear "ProxyJump not supported in v1" error
    /// when a host with this field set is actually dialed (Slice 5c).
    pub proxy_jump: Option<String>,
}

/// Per-host connection state. Rendered in the Fleet tile as a marker
/// glyph (● / ◐ / ○ / ⚠). Phase 5 Slice 5b emits only `Disconnected`
/// (no connections are made yet); 5c drives the full state machine.
/// Phase 6 adds `AgentNotDeployed` and `AgentVersionMismatch`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// Not connected. Initial state; also the idle state after clean
    /// disconnect. Rendered as ○ gray.
    Disconnected,
    /// Dialing / key-exchanging / authenticating. Rendered as ◐ yellow.
    Connecting,
    /// Connected, heartbeat fresh. Rendered as ● green.
    Connected,
    /// Connected but heartbeat is late. Transient — one more missed
    /// tick transitions to `Disconnected` and schedules a reconnect.
    /// Rendered as ◐ yellow.
    Degraded,
    /// Authentication failed. Terminal state: no auto-reconnect; the
    /// user must fix the auth issue and trigger `Ctrl-b r` on the
    /// Fleet row. Rendered as ⚠ red.
    AuthFailed,
    /// Host-key TOFU rejected the presented key. Terminal state: user
    /// must `tepegoz doctor --ssh-forget <alias>` after verifying the
    /// key change is legitimate. Rendered as ⚠ red.
    HostKeyMismatch,
    /// (Phase 6) Agent binary not yet deployed to the remote host.
    /// Rendered as ⚠ red.
    AgentNotDeployed,
    /// (Phase 6) Agent protocol version doesn't match controller's.
    /// Rendered as ⚠ red.
    AgentVersionMismatch,
}

impl HostState {
    /// `true` for `⚠` red states — the ones that carry a `reason`
    /// string on `Event::HostStateChanged` and trigger a red toast on
    /// the transition. Helper for TUI code that branches on the state
    /// shape rather than spelling out every variant.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            HostState::AuthFailed
                | HostState::HostKeyMismatch
                | HostState::AgentNotDeployed
                | HostState::AgentVersionMismatch
        )
    }
}

/// Action the client asks the daemon's Fleet supervisor to take
/// against a host.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetActionKind {
    /// Reset backoff and jump the supervisor to `Connecting`
    /// (recovers from terminal `AuthFailed` / `HostKeyMismatch` too —
    /// pairs with `tepegoz doctor --ssh-forget` for the latter).
    Reconnect,
    /// Move the supervisor to `Disconnected` without triggering a
    /// reconnect. Pairs with a per-host `autoconnect = true` that
    /// the user wants to override for the session.
    Disconnect,
}

/// One-shot Fleet command carrying a client-assigned id.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct FleetActionRequest {
    pub request_id: u64,
    pub alias: String,
    pub kind: FleetActionKind,
}

/// Daemon's reply to a `FleetAction`. `Success` means "dispatched to
/// the supervisor" — actual connection outcome arrives as
/// `Event::HostStateChanged`, not as a `Success` here. `Failure` is
/// returned when the alias is unknown or the supervisor task has
/// terminated.
#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub struct FleetActionResult {
    pub request_id: u64,
    pub alias: String,
    pub kind: FleetActionKind,
    pub outcome: FleetActionOutcome,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone)]
pub enum FleetActionOutcome {
    Success,
    Failure { reason: String },
}
