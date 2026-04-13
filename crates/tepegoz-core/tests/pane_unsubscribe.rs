//! Regression: `Payload::Unsubscribe { id }` must cancel a pane forwarder.
//!
//! This was a real bug surfaced while planning Slice C2: pane subscriptions
//! lived in a `JoinSet<()>` with no per-id key, and `Unsubscribe` only
//! touched `status_sub` and `docker_subs`. The Slice C1 TUI's synthetic
//! re-attach (Unsubscribe(prev_pane_sub) + AttachPane(new_pane_sub) on
//! Scope→Pane switch) was leaking one zombie forwarder per mode switch —
//! daemon CPU + writer-mpsc bandwidth burnt indefinitely, and every pane
//! byte was sent over the socket twice (once on the old sub, once on the
//! new) until session end.
//!
//! This test pins the fix: after `Unsubscribe(sub_1)`, no further envelopes
//! must arrive with `subscription_id == sub_1`.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneId, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

const PANE_SUB_1: u64 = 100;
const PANE_SUB_2: u64 = 101;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsubscribe_cancels_pane_forwarder() {
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
    let pane = open_pane(&mut r, &mut w).await;

    // First attachment.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id: pane.id,
                subscription_id: PANE_SUB_1,
            },
        },
    )
    .await
    .expect("attach 1");

    // Send some output; drain whatever arrives on PANE_SUB_1 until we see
    // our marker. We don't assert on it here — that's `pty_persistence`'s job.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id: pane.id,
                data: b"echo BEFORE_UNSUB\n".to_vec(),
            },
        },
    )
    .await
    .expect("input 1");
    drain_until_marker(&mut r, b"BEFORE_UNSUB", Duration::from_secs(3)).await;

    // Unsubscribe sub_1. Forwarder must stop.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Unsubscribe { id: PANE_SUB_1 },
        },
    )
    .await
    .expect("unsubscribe");

    // Give the daemon a moment to cancel the forwarder + drain any
    // already-in-flight envelopes from the writer mpsc.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Drain anything left on the connection (in-flight envelopes from before
    // the cancellation took effect). Allow up to 250 ms; after that the
    // socket should be quiet on PANE_SUB_1.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, read_envelope(&mut r)).await {
            Ok(Ok(env)) => {
                // Drop pre-cancellation in-flight bytes; just observe.
                if matches!(
                    &env.payload,
                    Payload::Event(EventFrame { subscription_id, .. })
                        if *subscription_id == PANE_SUB_1
                ) {
                    continue;
                }
            }
            _ => break,
        }
    }

    // Now: produce more pane output. If the forwarder is correctly cancelled,
    // NO envelope with subscription_id == PANE_SUB_1 should appear. The new
    // PANE_SUB_2 will pick the bytes up.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id: pane.id,
                data: b"echo AFTER_UNSUB\n".to_vec(),
            },
        },
    )
    .await
    .expect("input 2");
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id: pane.id,
                subscription_id: PANE_SUB_2,
            },
        },
    )
    .await
    .expect("attach 2");

    // Listen for AFTER_UNSUB on the new sub, while asserting no events on
    // the old sub leak through.
    let mut got_after_on_sub2 = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut accumulator: Vec<u8> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let env = match tokio::time::timeout(remaining, read_envelope(&mut r)).await {
            Ok(Ok(e)) => e,
            _ => break,
        };
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            assert_ne!(
                subscription_id, PANE_SUB_1,
                "forwarder for {PANE_SUB_1} should have been cancelled by Unsubscribe; \
                 receiving events on it after Unsubscribe means the daemon leaked the task"
            );
            if subscription_id == PANE_SUB_2 {
                match event {
                    Event::PaneSnapshot { scrollback, .. } => {
                        accumulator.extend_from_slice(&scrollback);
                    }
                    Event::PaneOutput { data } => {
                        accumulator.extend_from_slice(&data);
                    }
                    _ => {}
                }
                if contains(&accumulator, b"AFTER_UNSUB") {
                    got_after_on_sub2 = true;
                    break;
                }
            }
        }
    }

    assert!(
        got_after_on_sub2,
        "AFTER_UNSUB should be observable on the new subscription (PANE_SUB_2). \
         If we got here without seeing it, the daemon failed to attach the second forwarder."
    );

    daemon_handle.abort();
}

// ---- protocol helpers ----

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
            client_name: "pane-unsubscribe-test".into(),
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

async fn open_pane(
    r: &mut tokio::net::unix::OwnedReadHalf,
    w: &mut tokio::net::unix::OwnedWriteHalf,
) -> PaneInfo {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(OpenPaneSpec {
                shell: Some("/bin/sh".into()),
                cwd: None,
                env: Vec::new(),
                rows: 24,
                cols: 80,
            }),
        },
    )
    .await
    .expect("openpane write");
    let rep = read_envelope(r).await.expect("pane response");
    match rep.payload {
        Payload::PaneOpened(info) => info,
        Payload::Error(e) => panic!("open failed: {:?} {}", e.kind, e.message),
        other => panic!("expected PaneOpened, got {other:?}"),
    }
}

async fn drain_until_marker(
    r: &mut tokio::net::unix::OwnedReadHalf,
    needle: &[u8],
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut accumulator: Vec<u8> = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let env = match tokio::time::timeout(remaining, read_envelope(r)).await {
            Ok(Ok(e)) => e,
            _ => return,
        };
        if let Payload::Event(EventFrame { event, .. }) = env.payload {
            match event {
                Event::PaneSnapshot { scrollback, .. } => {
                    accumulator.extend_from_slice(&scrollback);
                }
                Event::PaneOutput { data } => {
                    accumulator.extend_from_slice(&data);
                }
                _ => {}
            }
            if contains(&accumulator, needle) {
                return;
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

async fn wait_for_socket(path: &Path, timeout: Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon socket never appeared at {}", path.display());
}

#[allow(dead_code)]
fn _unused_pane_id_marker(_id: PaneId) {}
