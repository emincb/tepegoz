//! SSH-backed remote pty panes.
//!
//! Phase 5 Slice 5d-i: the daemon's Fleet tile already knows which
//! hosts exist + their connection states; 5d adds the ability to open
//! a pty on a remote host and surface it as a pane to the client.
//! Bytes flow through the same `broadcast::Sender<PaneUpdate>` shape
//! that `tepegoz_pty::Pane` uses, so the client-side forwarder logic
//! in `handle_client` does not branch on local-vs-remote — only
//! `handle_command` picks the right manager at open / attach /
//! send_input / resize / close time.
//!
//! Each [`RemotePane`] opens its own fresh SSH connection via
//! `tepegoz_ssh::connect_host`. Multiple panes against the same host
//! open multiple SSH connections for v1 simplicity — Phase 6's agent
//! deployment consolidates connections through a shared per-host
//! session channel that proxies our wire protocol over stdio, at
//! which point `RemotePane::open` will be rewritten to register a
//! pane on the agent's stdio session rather than opening its own
//! TCP connection. The wire shape
//! (`PaneTarget::Remote { alias }` → `PaneOpened`) stays identical
//! across the two eras.
//!
//! Phase-5 known limitation (documented in OPERATIONS): a dropped
//! SSH connection kills the pane. russh's channel-close propagates
//! through as `PaneUpdate::Exit` and the client renders the pane's
//! scrollback with the terminal banner, after which the pane is dead.
//! Phase 6's agent session survives SSH disconnects transparently.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use russh::ChannelMsg;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::debug;

use tepegoz_proto::{PaneId, PaneInfo};
use tepegoz_pty::PaneUpdate;

/// Scrollback ring capacity for remote panes. Matches the local
/// `tepegoz_pty` default so UX feels identical across local + remote.
const REMOTE_SCROLLBACK_CAPACITY: usize = 2 * 1024 * 1024;
/// Broadcast capacity — mirrors local pane sizing so lagged-subscriber
/// behavior is identical across local + remote.
const REMOTE_BROADCAST_CAPACITY: usize = 1024;

/// Manager for remote (SSH-backed) panes. Lives alongside
/// `tepegoz_pty::PtyManager` in `SharedState`; the daemon's
/// command handlers consult both on pane-keyed operations and dispatch
/// by matching the `PaneId`.
pub struct RemotePaneManager {
    panes: RwLock<HashMap<PaneId, Arc<RemotePane>>>,
    next_id: AtomicU64,
}

impl RemotePaneManager {
    pub fn new() -> Self {
        Self {
            panes: RwLock::new(HashMap::new()),
            // Start at a high-ish value so local + remote pane ids
            // don't conflict in the common case. Local panes start at
            // 1; remote panes start at 2^32 to keep visual separation
            // in logs without any mathematical overlap claim (the real
            // uniqueness guarantee comes from allocation within each
            // manager plus the parallel-map lookup pattern).
            next_id: AtomicU64::new(1 << 32),
        }
    }

    /// Open a pty on a remote host identified by `alias`. Returns a
    /// fresh [`RemotePane`] registered in the manager's map. Dispatches
    /// through `tepegoz_ssh::connect_host` — so every TOFU / auth /
    /// ProxyJump-pre-check invariant that Slice 5a pinned applies here
    /// equally.
    pub async fn open(
        &self,
        alias: String,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Arc<RemotePane>> {
        let hosts = tokio::task::spawn_blocking(tepegoz_ssh::HostList::discover)
            .await
            .map_err(|e| anyhow::anyhow!("host discovery task panic: {e}"))?
            .map_err(|e| anyhow::anyhow!("ssh host discovery failed: {e}"))?;
        let known_hosts = tepegoz_ssh::KnownHostsStore::open()
            .map_err(|e| anyhow::anyhow!("open known_hosts: {e}"))?;

        let session = tepegoz_ssh::connect_host(&alias, &hosts, &known_hosts)
            .await
            .map_err(|e| anyhow::anyhow!("connect_host({alias}): {e}"))?;
        let session = Arc::new(session);

        // Open a session channel + request a pty + shell. If any step
        // fails, `session` drops, which drops the russh Handle, which
        // tears down the TCP connection cleanly.
        let channel = tepegoz_ssh::open_session(&session)
            .await
            .map_err(|e| anyhow::anyhow!("open_session({alias}): {e}"))?;
        let channel = channel.into_inner();
        channel
            .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
            .await
            .map_err(|e| anyhow::anyhow!("request_pty: {e}"))?;
        channel
            .request_shell(false)
            .await
            .map_err(|e| anyhow::anyhow!("request_shell: {e}"))?;

        let id: PaneId = self.next_id.fetch_add(1, Ordering::Relaxed);
        let pane = RemotePane::spawn(id, alias, rows, cols, session, channel);

        {
            let mut panes = self.panes.write().await;
            panes.insert(id, Arc::clone(&pane));
        }

        Ok(pane)
    }

    pub async fn get(&self, id: PaneId) -> Option<Arc<RemotePane>> {
        self.panes.read().await.get(&id).cloned()
    }

    pub async fn contains(&self, id: PaneId) -> bool {
        self.panes.read().await.contains_key(&id)
    }

    pub async fn list(&self) -> Vec<PaneInfo> {
        self.panes.read().await.values().map(|p| p.info()).collect()
    }

    pub async fn close(&self, id: PaneId) -> anyhow::Result<()> {
        let Some(pane) = self.panes.write().await.remove(&id) else {
            anyhow::bail!("no remote pane with id {id:?}");
        };
        pane.shutdown();
        Ok(())
    }

    pub async fn count(&self) -> usize {
        self.panes.read().await.len()
    }
}

impl Default for RemotePaneManager {
    fn default() -> Self {
        Self::new()
    }
}

/// A single SSH-backed pty pane. Broadcast channel + scrollback match
/// the local [`tepegoz_pty::Pane`] exactly so subscribers can't tell
/// local from remote.
pub struct RemotePane {
    pub id: PaneId,
    pub alias: String,
    pub created_at_unix_millis: u64,
    size: Mutex<(u16, u16)>,
    output_tx: broadcast::Sender<PaneUpdate>,
    scrollback: Arc<Mutex<Scrollback>>,
    exit_code: Arc<Mutex<Option<i32>>>,
    alive: Arc<Mutex<bool>>,
    /// Send side of the channel-driver mpsc — `send_input`, `resize`,
    /// `close` push commands here, the driver task executes them on
    /// the actual SSH channel.
    cmd_tx: mpsc::UnboundedSender<RemotePaneCmd>,
    /// Keep the SSH session alive for the lifetime of the pane.
    /// Dropping this handle closes the session; the channel driver
    /// task detects that via `ChannelMsg::Close` and emits `PaneExit`.
    _session: Arc<tepegoz_ssh::SshSession>,
}

/// Commands the pane's public API pushes to the channel-driver task.
/// One task owns the russh channel (both halves, via `split`), so all
/// mutations serialize naturally.
enum RemotePaneCmd {
    Data(Vec<u8>),
    Resize { rows: u16, cols: u16 },
    Close,
}

impl RemotePane {
    fn spawn(
        id: PaneId,
        alias: String,
        rows: u16,
        cols: u16,
        session: Arc<tepegoz_ssh::SshSession>,
        channel: russh::Channel<russh::client::Msg>,
    ) -> Arc<Self> {
        let created_at_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let (output_tx, _) = broadcast::channel(REMOTE_BROADCAST_CAPACITY);
        let scrollback = Arc::new(Mutex::new(Scrollback::new(REMOTE_SCROLLBACK_CAPACITY)));
        let exit_code = Arc::new(Mutex::new(None::<i32>));
        let alive = Arc::new(Mutex::new(true));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Driver task owns the channel outright + selects between
        // command-mpsc (outbound) and channel.wait() (inbound).
        let output_tx_bg = output_tx.clone();
        let scrollback_bg = Arc::clone(&scrollback);
        let exit_code_bg = Arc::clone(&exit_code);
        let alive_bg = Arc::clone(&alive);
        let alias_log = alias.clone();
        tokio::spawn(async move {
            drive_channel(
                alias_log,
                channel,
                cmd_rx,
                scrollback_bg,
                output_tx_bg,
                exit_code_bg,
                alive_bg,
            )
            .await;
        });

        Arc::new(Self {
            id,
            alias,
            created_at_unix_millis,
            size: Mutex::new((rows, cols)),
            output_tx,
            scrollback,
            exit_code,
            alive,
            cmd_tx,
            _session: session,
        })
    }

    pub fn info(&self) -> PaneInfo {
        let (rows, cols) = *self.size.lock().expect("size mutex");
        PaneInfo {
            id: self.id,
            created_at_unix_millis: self.created_at_unix_millis,
            rows,
            cols,
            // Expose the alias where the local pane exposes its shell
            // command string — clients render it as the pane's label.
            shell: format!("ssh:{}", self.alias),
            alive: self.is_alive(),
        }
    }

    pub fn is_alive(&self) -> bool {
        *self.alive.lock().expect("alive mutex")
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().expect("exit_code mutex")
    }

    pub fn size(&self) -> (u16, u16) {
        *self.size.lock().expect("size mutex")
    }

    /// Atomically capture current scrollback and subscribe to live
    /// updates. Identical semantics to `tepegoz_pty::Pane::subscribe` —
    /// the two are interchangeable at the forwarder level.
    pub fn subscribe(&self) -> (Bytes, broadcast::Receiver<PaneUpdate>) {
        let sb = self.scrollback.lock().expect("scrollback mutex");
        let snapshot = sb.snapshot();
        let rx = self.output_tx.subscribe();
        drop(sb);
        (snapshot, rx)
    }

    pub fn send_input(&self, data: &[u8]) -> anyhow::Result<()> {
        self.cmd_tx
            .send(RemotePaneCmd::Data(data.to_vec()))
            .map_err(|_| anyhow::anyhow!("remote pane driver task has exited"))
    }

    pub fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        *self.size.lock().expect("size mutex") = (rows, cols);
        self.cmd_tx
            .send(RemotePaneCmd::Resize { rows, cols })
            .map_err(|_| anyhow::anyhow!("remote pane driver task has exited"))
    }

    fn shutdown(&self) {
        let _ = self.cmd_tx.send(RemotePaneCmd::Close);
    }
}

async fn drive_channel(
    alias: String,
    mut channel: russh::Channel<russh::client::Msg>,
    mut cmd_rx: mpsc::UnboundedReceiver<RemotePaneCmd>,
    scrollback: Arc<Mutex<Scrollback>>,
    output_tx: broadcast::Sender<PaneUpdate>,
    exit_code: Arc<Mutex<Option<i32>>>,
    alive: Arc<Mutex<bool>>,
) {
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    // All senders dropped — pane was removed from the
                    // manager and no further commands are possible.
                    let _ = channel.close().await;
                    break;
                };
                match cmd {
                    RemotePaneCmd::Data(data) => {
                        let mut slice: &[u8] = &data;
                        if let Err(e) = channel.data(&mut slice).await {
                            debug!(alias, error = %e, "channel.data write failed; ending driver");
                            break;
                        }
                    }
                    RemotePaneCmd::Resize { rows, cols } => {
                        if let Err(e) = channel
                            .window_change(cols as u32, rows as u32, 0, 0)
                            .await
                        {
                            debug!(alias, error = %e, "channel.window_change failed");
                        }
                    }
                    RemotePaneCmd::Close => {
                        let _ = channel.eof().await;
                        let _ = channel.close().await;
                        break;
                    }
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        let bytes = Bytes::copy_from_slice(&data);
                        {
                            let mut sb = scrollback.lock().expect("scrollback mutex");
                            sb.append(bytes.clone());
                        }
                        // Lagged receivers surface via `broadcast::error::SendError`
                        // wrapping `RecvError::Lagged` at receive time — nothing
                        // to do at send time.
                        let _ = output_tx.send(PaneUpdate::Bytes(bytes));
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        // stderr (ext = 1) and future ext codes merge into the
                        // same byte stream — matches pty semantics where the
                        // shell's stderr ends up on the same tty.
                        let bytes = Bytes::copy_from_slice(&data);
                        {
                            let mut sb = scrollback.lock().expect("scrollback mutex");
                            sb.append(bytes.clone());
                        }
                        let _ = output_tx.send(PaneUpdate::Bytes(bytes));
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        *exit_code.lock().expect("exit_code mutex") = Some(exit_status as i32);
                    }
                    Some(ChannelMsg::Close) | Some(ChannelMsg::Eof) | None => {
                        break;
                    }
                    _ => {
                        // Ignore XonXoff, WindowAdjusted, Success, Failure,
                        // Signal — none affect the byte-forwarding loop.
                    }
                }
            }
        }
    }

    // Channel is done — mark the pane dead, emit Exit so subscribers
    // transition the UI.
    *alive.lock().expect("alive mutex") = false;
    let ec = *exit_code.lock().expect("exit_code mutex");
    let _ = output_tx.send(PaneUpdate::Exit { exit_code: ec });
    debug!(alias, exit_code = ?ec, "remote pane driver exiting");
}

/// Bounded append-only byte log with eviction-from-front on overflow.
/// Duplicated from `tepegoz_pty::Scrollback` to avoid making it a
/// public API just for this one additional consumer — the two should
/// converge if we ever factor a shared `PaneBackend` trait.
struct Scrollback {
    chunks: std::collections::VecDeque<Bytes>,
    total_bytes: usize,
    capacity: usize,
}

impl Scrollback {
    fn new(capacity: usize) -> Self {
        Self {
            chunks: std::collections::VecDeque::new(),
            total_bytes: 0,
            capacity,
        }
    }

    fn append(&mut self, bytes: Bytes) {
        if bytes.is_empty() {
            return;
        }
        self.total_bytes += bytes.len();
        self.chunks.push_back(bytes);
        while self.total_bytes > self.capacity {
            let Some(dropped) = self.chunks.pop_front() else {
                break;
            };
            self.total_bytes -= dropped.len();
        }
    }

    fn snapshot(&self) -> Bytes {
        if self.chunks.is_empty() {
            return Bytes::new();
        }
        let mut out = BytesMut::with_capacity(self.total_bytes);
        for chunk in &self.chunks {
            out.extend_from_slice(chunk);
        }
        out.freeze()
    }
}
