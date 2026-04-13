//! Daemon-owned pty session manager.
//!
//! Each `Pane` wraps a portable-pty master. Output is captured by a blocking
//! reader thread, appended to a bounded ring buffer (default 2 MiB), and
//! broadcast to subscribed clients. Subscribers attaching after some output
//! has already been produced receive a snapshot of the ring buffer first,
//! then the live stream — so "detach/reattach and see what you missed"
//! works without the TUI having to buffer anything itself.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, info, warn};

use tepegoz_proto::{PaneId, PaneInfo};

pub const DEFAULT_RING_CAPACITY: usize = 2 * 1024 * 1024;
pub const BROADCAST_CHANNEL_SIZE: usize = 1024;

/// Output events published on a pane's broadcast channel.
#[derive(Debug, Clone)]
pub enum PaneUpdate {
    /// A chunk of pty output.
    Bytes(Bytes),
    /// The child has exited and no further bytes will arrive.
    Exit { exit_code: Option<i32> },
}

pub struct OpenSpec {
    pub shell: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
}

impl Default for OpenSpec {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            env: Vec::new(),
            rows: 40,
            cols: 120,
        }
    }
}

pub struct PtyManager {
    panes: Arc<RwLock<HashMap<PaneId, Arc<Pane>>>>,
    next_id: AtomicU64,
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            panes: Arc::new(RwLock::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    pub async fn open(&self, spec: OpenSpec) -> anyhow::Result<Arc<Pane>> {
        let shell = spec.shell.clone().unwrap_or_else(default_shell);
        let size = PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pty_system = NativePtySystem::default();
        let pair = pty_system.openpty(size)?;

        let mut cmd = CommandBuilder::new(&shell);
        if let Some(cwd) = spec.cwd.as_ref() {
            cmd.cwd(cwd);
        }
        // Inherit a sensible TERM so the child emits standard escape sequences.
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        );
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let mut child = pair.slave.spawn_command(cmd)?;
        // Dropping the slave is critical: it releases our copy of the slave
        // fd so the child sees EOF when *it* exits, not before.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let created_at_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();

        let (output_tx, _) = broadcast::channel::<PaneUpdate>(BROADCAST_CHANNEL_SIZE);
        let scrollback = Arc::new(std::sync::Mutex::new(Scrollback::new(
            DEFAULT_RING_CAPACITY,
        )));
        let size_cell = Arc::new(std::sync::Mutex::new((spec.rows, spec.cols)));
        let exit_code = Arc::new(std::sync::Mutex::new(None));
        let alive = Arc::new(std::sync::Mutex::new(true));

        let pane = Arc::new(Pane {
            id,
            created_at_unix_millis,
            shell: shell.clone(),
            size: Arc::clone(&size_cell),
            output_tx: output_tx.clone(),
            scrollback: Arc::clone(&scrollback),
            writer: Arc::new(std::sync::Mutex::new(writer)),
            master: Arc::new(std::sync::Mutex::new(pair.master)),
            exit_code: Arc::clone(&exit_code),
            alive: Arc::clone(&alive),
        });

        // Reader thread: read from pty master, append to scrollback, broadcast.
        {
            let scrollback_ref = Arc::clone(&scrollback);
            let output_tx = output_tx.clone();
            std::thread::Builder::new()
                .name(format!("tepegoz-pty-reader-{id}"))
                .spawn(move || reader_loop(reader, scrollback_ref, output_tx))
                .map_err(|e| anyhow::anyhow!("spawn reader thread: {e}"))?;
        }

        // Waiter thread: wait for child, record exit code, broadcast Exit.
        {
            let output_tx = output_tx.clone();
            let exit_code_ref = Arc::clone(&exit_code);
            let alive_ref = Arc::clone(&alive);
            std::thread::Builder::new()
                .name(format!("tepegoz-pty-waiter-{id}"))
                .spawn(move || {
                    let code = match child.wait() {
                        Ok(status) => status.exit_code().try_into().ok(),
                        Err(e) => {
                            warn!(pane_id = id, error = %e, "child wait failed");
                            None
                        }
                    };
                    *exit_code_ref.lock().expect("exit_code mutex") = code;
                    *alive_ref.lock().expect("alive mutex") = false;
                    debug!(pane_id = id, ?code, "pty child exited");
                    let _ = output_tx.send(PaneUpdate::Exit { exit_code: code });
                    drop(output_tx);
                })
                .map_err(|e| anyhow::anyhow!("spawn waiter thread: {e}"))?;
        }

        self.panes.write().await.insert(id, Arc::clone(&pane));
        info!(pane_id = id, shell = %shell, rows = spec.rows, cols = spec.cols, "pty opened");
        Ok(pane)
    }

    pub async fn get(&self, id: PaneId) -> Option<Arc<Pane>> {
        self.panes.read().await.get(&id).cloned()
    }

    pub async fn list(&self) -> Vec<PaneInfo> {
        self.panes.read().await.values().map(|p| p.info()).collect()
    }

    pub async fn close(&self, id: PaneId) -> anyhow::Result<()> {
        let pane = self
            .panes
            .write()
            .await
            .remove(&id)
            .ok_or_else(|| anyhow::anyhow!("no such pane: {id}"))?;
        // Dropping the master file descriptors terminates the child on next
        // SIGPIPE / EOF. We don't explicitly kill — graceful is better.
        drop(pane);
        Ok(())
    }

    pub async fn count(&self) -> usize {
        self.panes.read().await.len()
    }
}

pub struct Pane {
    pub id: PaneId,
    pub created_at_unix_millis: u64,
    pub shell: String,
    size: Arc<std::sync::Mutex<(u16, u16)>>,
    output_tx: broadcast::Sender<PaneUpdate>,
    scrollback: Arc<std::sync::Mutex<Scrollback>>,
    writer: Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    master: Arc<std::sync::Mutex<Box<dyn MasterPty + Send>>>,
    exit_code: Arc<std::sync::Mutex<Option<i32>>>,
    alive: Arc<std::sync::Mutex<bool>>,
}

impl Pane {
    pub fn info(&self) -> PaneInfo {
        let (rows, cols) = *self.size.lock().expect("size mutex");
        PaneInfo {
            id: self.id,
            created_at_unix_millis: self.created_at_unix_millis,
            rows,
            cols,
            shell: self.shell.clone(),
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

    /// Atomically capture current scrollback and subscribe to live updates.
    ///
    /// The returned `Bytes` replays every byte currently held in the ring
    /// buffer; the receiver streams every subsequent `PaneUpdate`. Bytes
    /// between the snapshot and the first received update are impossible —
    /// both are captured under the same scrollback lock, and appends take
    /// that lock before notifying subscribers.
    pub fn subscribe(&self) -> (Bytes, broadcast::Receiver<PaneUpdate>) {
        let sb = self.scrollback.lock().expect("scrollback mutex");
        let snapshot = sb.snapshot();
        let rx = self.output_tx.subscribe();
        drop(sb);
        (snapshot, rx)
    }

    pub fn send_input(&self, data: &[u8]) -> anyhow::Result<()> {
        let mut w = self.writer.lock().expect("writer mutex");
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> anyhow::Result<()> {
        let master = self.master.lock().expect("master mutex");
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        *self.size.lock().expect("size mutex") = (rows, cols);
        Ok(())
    }
}

/// Bounded append-only byte log with eviction-from-front on overflow.
struct Scrollback {
    chunks: VecDeque<Bytes>,
    total_bytes: usize,
    capacity: usize,
}

impl Scrollback {
    fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
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

fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    scrollback: Arc<std::sync::Mutex<Scrollback>>,
    output_tx: broadcast::Sender<PaneUpdate>,
) {
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = Bytes::copy_from_slice(&buf[..n]);
                // Hold the scrollback lock across BOTH the append and the
                // broadcast send. Otherwise a subscriber that takes a
                // snapshot between our unlock and our send will observe the
                // same bytes in both snapshot and live stream — the TUI
                // then renders them twice and glitches on attach.
                // `broadcast::send` is non-blocking, so this is cheap.
                let mut sb = scrollback.lock().expect("scrollback mutex");
                sb.append(bytes.clone());
                let _ = output_tx.send(PaneUpdate::Bytes(bytes));
                drop(sb);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                warn!(error = %e, "pty reader error");
                break;
            }
        }
    }
    // Reader EOF. Waiter thread publishes PaneUpdate::Exit with the exit
    // code; we just drop our tx clone.
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_evicts_on_overflow() {
        let mut sb = Scrollback::new(10);
        sb.append(Bytes::from_static(b"abcd"));
        sb.append(Bytes::from_static(b"efgh"));
        assert_eq!(sb.total_bytes, 8);
        assert_eq!(&sb.snapshot()[..], b"abcdefgh");

        sb.append(Bytes::from_static(b"ijkl")); // 12 bytes total, over cap
        // Oldest 4 ("abcd") evicted.
        assert_eq!(sb.total_bytes, 8);
        assert_eq!(&sb.snapshot()[..], b"efghijkl");
    }

    #[test]
    fn scrollback_snapshot_concats_chunks() {
        let mut sb = Scrollback::new(1024);
        sb.append(Bytes::from_static(b"hello "));
        sb.append(Bytes::from_static(b"world"));
        assert_eq!(&sb.snapshot()[..], b"hello world");
    }

    /// Regression for the subscribe/broadcast race.
    ///
    /// Drives a stream of deterministic numbered markers out of a real
    /// `/bin/sh`, subscribes mid-stream (the race window), and asserts each
    /// marker appears **exactly once** across (snapshot + live stream). If
    /// the reader released the scrollback lock before broadcasting, a
    /// subscriber taking a snapshot between those two points would see the
    /// same bytes twice — the invariant this test enforces is that a given
    /// byte is in exactly one of {snapshot, live} per subscriber.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_does_not_duplicate_bytes() {
        let manager = PtyManager::new();
        let pane = manager
            .open(OpenSpec {
                shell: Some("/bin/sh".into()),
                cwd: None,
                env: Vec::new(),
                rows: 24,
                cols: 80,
            })
            .await
            .expect("open pane");

        // Let the shell come up and print its initial prompt.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 50 markers with 5 ms between each → ~250 ms of production, plenty
        // of race windows for a mid-stream subscriber to land in.
        // `stty -echo` suppresses echo of future input; the command itself
        // is still echoed once but contains the literal `${i}`, not
        // `LINE_1_END`, so it won't contaminate our marker count.
        pane.send_input(
            b"stty -echo; for i in $(seq 1 50); do echo LINE_${i}_END; sleep 0.005; done; exit\n",
        )
        .expect("send_input");

        // Subscribe mid-stream.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (snap, mut rx) = pane.subscribe();
        let mut all = snap.to_vec();

        loop {
            match rx.recv().await {
                Ok(PaneUpdate::Bytes(b)) => all.extend_from_slice(&b),
                Ok(PaneUpdate::Exit { .. }) => break,
                Err(_) => break,
            }
        }

        for i in 1..=50 {
            let needle = format!("LINE_{i}_END");
            let count = all
                .windows(needle.len())
                .filter(|w| *w == needle.as_bytes())
                .count();
            assert_eq!(
                count, 1,
                "{needle} appeared {count} times (expected exactly 1). \
                 A count > 1 indicates the scrollback/broadcast race \
                 duplicated bytes across snapshot and live stream."
            );
        }
    }
}
