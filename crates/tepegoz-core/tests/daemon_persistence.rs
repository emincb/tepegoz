//! Phase 1 acceptance: the daemon owns state; clients are windows into it.
//!
//! Proves that daemon state (specifically `clients_total` and `uptime_seconds`)
//! advances across a full client disconnect/reconnect cycle. That's the demo
//! feeling ("kill the TUI, reopen it, nothing is lost") expressed as a test.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, ErrorKind, Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, StatusSnapshot,
    Subscription,
    codec::{read_envelope, write_envelope},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_state_persists_across_client_reconnect() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");

    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };

    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });

    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    // Client 1 connects, subscribes, gets a snapshot, then drops.
    let snap_1 = connect_and_capture_first_snapshot(&sock_path).await;
    assert_eq!(
        snap_1.clients_total, 1,
        "first connect should increment total"
    );

    // Intentionally wait so uptime advances between connects.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Client 2 reconnects. Daemon state must have survived.
    let snap_2 = connect_and_capture_first_snapshot(&sock_path).await;

    assert_eq!(
        snap_2.clients_total, 2,
        "reconnect should see +1 total connects"
    );
    assert!(
        snap_2.uptime_seconds >= snap_1.uptime_seconds,
        "uptime must not regress — daemon is persistent, not restarted. \
         before={}, after={}",
        snap_1.uptime_seconds,
        snap_2.uptime_seconds
    );
    assert_eq!(
        snap_2.daemon_pid, snap_1.daemon_pid,
        "same daemon process — pid stable across client reconnects"
    );
    assert!(
        snap_2.events_sent >= snap_1.events_sent,
        "event counter must not regress across reconnect"
    );

    daemon_handle.abort();
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

/// A v3 client connecting to a v4 daemon must be rejected at handshake time
/// with a structured `Error(VersionMismatch)`, NOT silently accepted (which
/// would later trip an opaque rkyv decode error when an unknown enum variant
/// arrives over the wire).
///
/// Architecture commitment: peers reject mismatches. This test enforces it
/// against both version fields (envelope-level + Hello-level).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_rejects_protocol_version_mismatch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");

    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });
    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    // Send a Hello pretending to be v1 in BOTH version fields. Daemon must
    // reply Error(VersionMismatch) and close.
    let stale_version: u32 = 1;
    assert_ne!(
        stale_version, PROTOCOL_VERSION,
        "test must use a version that's actually different from the current daemon"
    );

    let stream = tokio::net::UnixStream::connect(&sock_path)
        .await
        .expect("connect");
    let (mut reader, mut writer) = stream.into_split();

    let hello = Envelope {
        version: stale_version,
        payload: Payload::Hello(Hello {
            client_version: stale_version,
            client_name: "stale-client".into(),
        }),
    };
    write_envelope(&mut writer, &hello).await.expect("write");

    // Daemon should respond with a single Error envelope, then close.
    let env = tokio::time::timeout(Duration::from_secs(2), read_envelope(&mut reader))
        .await
        .expect("daemon must respond within 2s — silent acceptance is the bug")
        .expect("read");

    match env.payload {
        Payload::Error(info) => {
            assert!(
                matches!(info.kind, ErrorKind::VersionMismatch),
                "expected VersionMismatch, got {:?}",
                info.kind
            );
            // Reason should mention both the client's version and the
            // daemon's, so the user can act on it.
            assert!(
                info.message.contains("v1"),
                "error message should mention the client's version, got: {:?}",
                info.message
            );
            assert!(
                info.message.contains(&format!("v{PROTOCOL_VERSION}")),
                "error message should mention the daemon's version, got: {:?}",
                info.message
            );
        }
        Payload::Welcome(_) => panic!(
            "daemon accepted v{stale_version} client! that's the bug — version mismatch must be \
             rejected at handshake before any further dispatch."
        ),
        other => panic!("expected Error(VersionMismatch), got {other:?}"),
    }

    daemon_handle.abort();
}

async fn connect_and_capture_first_snapshot(path: &Path) -> StatusSnapshot {
    let stream = UnixStream::connect(path).await.expect("connect");
    let (mut reader, mut writer) = stream.into_split();

    let hello = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Hello(Hello {
            client_version: PROTOCOL_VERSION,
            client_name: "integration-test".into(),
        }),
    };
    write_envelope(&mut writer, &hello).await.expect("hello");

    let welcome = read_envelope(&mut reader).await.expect("welcome");
    match &welcome.payload {
        Payload::Welcome(_) => {}
        other => panic!("expected Welcome, got {other:?}"),
    }

    let sub = Envelope {
        version: PROTOCOL_VERSION,
        payload: Payload::Subscribe(Subscription::Status { id: 1 }),
    };
    write_envelope(&mut writer, &sub).await.expect("subscribe");

    let ev = read_envelope(&mut reader).await.expect("status event");
    match ev.payload {
        Payload::Event(EventFrame {
            event: Event::Status(snap),
            ..
        }) => snap,
        other => panic!("expected Event(Status), got {other:?}"),
    }
}
