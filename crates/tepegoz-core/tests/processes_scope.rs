//! Phase 4 Slice 4b acceptance: `Subscribe(Processes)` returns either a
//! `ProcessList` (when the sysinfo probe succeeds) or a
//! `ProcessesUnavailable` (when the probe fails). Both are valid; the
//! daemon doesn't panic, doesn't hang, and reaches some terminal state
//! within a few seconds.
//!
//! An opt-in test gated on `TEPEGOZ_PROBE_TEST=1` spawns a known child
//! with a recognizable cmdline and asserts the probe reports it (with
//! non-empty command, non-zero mem_bytes or partial:true) within a
//! small budget. First sample must carry `cpu_percent: None` per the
//! CTO's em-dash semantic; we assert that explicitly.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

const PROCESSES_SUB_ID: u64 = 83;

/// Generous first-event budget — the daemon's first sysinfo refresh on a
/// busy CI host can take a few hundred milliseconds, and task scheduling
/// stretches that. 30 s matches the docker_scope / ports_scope precedent.
const FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Force-kills a child process on Drop so panics mid-test don't leak the
/// spawned `sleep` subprocess into the test runner's parent shell.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_subscription_emits_either_process_list_or_unavailable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");

    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });

    wait_for_socket(&sock_path, Duration::from_secs(5)).await;
    let (mut r, mut w) = connect(&sock_path).await;

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Processes {
                id: PROCESSES_SUB_ID,
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        },
    )
    .await
    .expect("subscribe");

    let event = read_first_processes_event(&mut r).await;

    match event {
        Event::ProcessList { rows, source } => {
            assert!(
                !source.is_empty(),
                "ProcessList source must be non-empty (e.g. `sysinfo`)"
            );
            // First sample contract: every row must carry cpu_percent: None.
            for row in &rows {
                assert!(
                    row.cpu_percent.is_none(),
                    "first ProcessList row must have cpu_percent: None so \
                     the TUI can render em-dash (row={row:?})"
                );
            }
        }
        Event::ProcessesUnavailable { reason } => {
            assert!(
                !reason.is_empty(),
                "ProcessesUnavailable must carry a non-empty reason — \
                 clients render it directly"
            );
        }
        other => panic!("expected ProcessList or ProcessesUnavailable, got {other:?}"),
    }

    daemon_handle.abort();
}

/// Opt-in: spawn a child with a recognizable cmdline; assert the probe
/// reports it within a small budget. Enable with `TEPEGOZ_PROBE_TEST=1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_subscription_sees_spawned_child_within_budget() {
    if std::env::var("TEPEGOZ_PROBE_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_PROBE_TEST=1 to enable");
        return;
    }

    // `sleep 30` is cross-platform (BSD sleep + GNU sleep both accept it)
    // and has a recognizable short name in the process table. stdin /
    // stdout / stderr piped to null so the test doesn't inherit output.
    let child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child `sleep 30`");
    let child_pid = child.id();
    let _guard = ChildGuard(child);
    eprintln!("spawned child sleep pid={child_pid}");

    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");

    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });

    wait_for_socket(&sock_path, Duration::from_secs(5)).await;
    let (mut r, mut w) = connect(&sock_path).await;

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Processes {
                id: PROCESSES_SUB_ID,
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        },
    )
    .await
    .expect("subscribe");

    // Drain events until we see our child pid, budget of 5 s. First
    // ProcessList is emitted immediately; a missed child (if sysinfo's
    // initial scan ran before our spawn landed in /proc) would appear on
    // the second tick at +2 s — the 5 s budget leaves slack.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("did not observe child pid {child_pid} in ProcessList within 5s budget");
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");

        let Payload::Event(EventFrame {
            subscription_id,
            event: Event::ProcessList { rows, source },
        }) = env.payload
        else {
            continue;
        };
        assert_eq!(subscription_id, PROCESSES_SUB_ID);

        if let Some(found) = rows.iter().find(|r| r.pid == child_pid) {
            assert!(
                !found.command.is_empty(),
                "child command must resolve (row={found:?} source={source})"
            );
            assert!(
                found.command.contains("sleep"),
                "child cmdline must contain 'sleep' \
                 (got command={:?})",
                found.command
            );
            assert!(
                found.start_time_unix_secs > 0,
                "start_time must be a real Unix timestamp"
            );
            // mem_bytes > 0 for a real process — but `sleep` is tiny and
            // some libproc/procfs backends might momentarily report 0
            // before the first real sample. Accept either non-zero mem
            // OR partial:true.
            assert!(
                found.mem_bytes > 0 || found.partial,
                "non-partial child must report non-zero mem_bytes (row={found:?})"
            );
            eprintln!(
                "probe found child pid={child_pid} command={:?} mem={}",
                found.command, found.mem_bytes
            );
            break;
        }
    }

    daemon_handle.abort();
}

async fn read_first_processes_event(r: &mut tokio::net::unix::OwnedReadHalf) -> Event {
    let env = tokio::time::timeout(FIRST_EVENT_TIMEOUT, read_envelope(r))
        .await
        .expect("processes subscription must produce an event within timeout")
        .expect("read envelope");
    match env.payload {
        Payload::Event(EventFrame {
            subscription_id,
            event,
        }) => {
            assert_eq!(
                subscription_id, PROCESSES_SUB_ID,
                "event must reference our subscription id"
            );
            event
        }
        other => panic!("expected Event, got {other:?}"),
    }
}

async fn connect(
    path: &Path,
) -> (
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
) {
    let stream = UnixStream::connect(path).await.expect("connect");
    let (mut r, mut w) = stream.into_split();

    let hello = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Hello(Hello {
            client_version: PROTOCOL_VERSION,
            client_name: "processes-acceptance-test".into(),
        }),
    };
    write_envelope(&mut w, &hello).await.expect("hello");
    let welcome = read_envelope(&mut r).await.expect("welcome");
    match &welcome.payload {
        Payload::Welcome(_) => {}
        other => panic!("expected Welcome, got {other:?}"),
    }
    (r, w)
}

async fn wait_for_socket(path: &Path, timeout: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket never appeared at {}", path.display());
}
