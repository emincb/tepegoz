//! Phase 2 acceptance: "kill the TUI mid-command, reopen, see where I left off."
//!
//! Spawns the daemon, opens a pty pane via client #1, sends a shell command
//! that produces a unique marker, disconnects client #1, reconnects as
//! client #2, re-attaches to the same pane, and asserts the marker is in
//! the scrollback the daemon replays.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneId, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

const ATTACH_SUB: u64 = 42;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pane_scrollback_persists_across_client_reconnect() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");

    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });

    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    // ---- Client 1: open pane, send marker, verify it appears ----
    let (mut r1, mut w1) = connect(&sock_path).await;
    let pane = open_pane(&mut r1, &mut w1).await;
    assert!(pane.alive, "newly opened pane should be alive");

    send_input(&mut w1, pane.id, b"echo MARKER_ALPHA\n").await;

    let collected_1 = attach_and_collect(
        &mut r1,
        &mut w1,
        pane.id,
        Duration::from_secs(5),
        b"MARKER_ALPHA",
    )
    .await;
    assert!(
        contains(&collected_1, b"MARKER_ALPHA"),
        "expected MARKER_ALPHA in client-1 output. got: {:?}",
        String::from_utf8_lossy(&collected_1)
    );

    // Disconnect client 1 by dropping its socket halves.
    drop(r1);
    drop(w1);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ---- Client 2: same daemon, list the pane, re-attach, verify scrollback ----
    let (mut r2, mut w2) = connect(&sock_path).await;
    let panes = list_panes(&mut r2, &mut w2).await;
    assert_eq!(panes.len(), 1, "exactly one pane should still exist");
    assert_eq!(panes[0].id, pane.id, "same pane id");
    assert!(panes[0].alive, "pane must still be alive after client drop");

    let collected_2 = attach_and_collect(
        &mut r2,
        &mut w2,
        pane.id,
        Duration::from_secs(3),
        b"MARKER_ALPHA",
    )
    .await;
    assert!(
        contains(&collected_2, b"MARKER_ALPHA"),
        "expected MARKER_ALPHA in reattach scrollback — the daemon must replay \
         what happened before we disconnected. got: {:?}",
        String::from_utf8_lossy(&collected_2)
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
            client_name: "integration-test".into(),
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
    let env = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::OpenPane(OpenPaneSpec {
            shell: Some("/bin/sh".into()),
            cwd: None,
            env: Vec::new(),
            rows: 24,
            cols: 80,
        }),
    };
    write_envelope(w, &env).await.expect("openpane write");
    let rep = read_envelope(r).await.expect("pane response");
    match rep.payload {
        Payload::PaneOpened(info) => info,
        Payload::Error(e) => panic!("open failed: {:?} {}", e.kind, e.message),
        other => panic!("expected PaneOpened, got {other:?}"),
    }
}

async fn list_panes(
    r: &mut tokio::net::unix::OwnedReadHalf,
    w: &mut tokio::net::unix::OwnedWriteHalf,
) -> Vec<PaneInfo> {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::ListPanes,
        },
    )
    .await
    .expect("listpanes write");
    let rep = read_envelope(r).await.expect("panelist");
    match rep.payload {
        Payload::PaneList { panes } => panes,
        other => panic!("expected PaneList, got {other:?}"),
    }
}

async fn send_input(w: &mut tokio::net::unix::OwnedWriteHalf, pane_id: PaneId, data: &[u8]) {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id,
                data: data.to_vec(),
            },
        },
    )
    .await
    .expect("sendinput");
}

/// Send AttachPane and accumulate output until `needle` is seen or the
/// timeout fires. Returns whatever was collected.
async fn attach_and_collect(
    r: &mut tokio::net::unix::OwnedReadHalf,
    w: &mut tokio::net::unix::OwnedWriteHalf,
    pane_id: PaneId,
    timeout: Duration,
    needle: &[u8],
) -> Vec<u8> {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id,
                subscription_id: ATTACH_SUB,
            },
        },
    )
    .await
    .expect("attach");

    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, read_envelope(r)).await {
            Ok(Ok(env)) => match env.payload {
                Payload::Event(EventFrame {
                    event: Event::PaneSnapshot { scrollback, .. },
                    ..
                }) => {
                    collected.extend_from_slice(&scrollback);
                    if contains(&collected, needle) {
                        return collected;
                    }
                }
                Payload::Event(EventFrame {
                    event: Event::PaneOutput { data },
                    ..
                }) => {
                    collected.extend_from_slice(&data);
                    if contains(&collected, needle) {
                        return collected;
                    }
                }
                Payload::Event(EventFrame {
                    event: Event::PaneExit { .. },
                    ..
                }) => break,
                _ => {}
            },
            _ => break,
        }
    }
    collected
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
