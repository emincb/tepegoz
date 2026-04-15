//! Phase 5 Slice 5d-ii: pane-stack wire-level acceptance.
//!
//! The TUI's pane-stack relies on the daemon multiplexing independent
//! per-pane byte streams across a single client socket:
//!
//! - Opening multiple panes yields distinct pane ids;
//! - Bytes sent to pane A never leak onto pane B's subscription;
//! - Bytes from pane B never leak onto pane A's subscription;
//! - Closing one pane leaves the other running (Ctrl-b & only takes
//!   the active tab down, not the whole session).
//!
//! Tested against local panes, which share the Phase 5 Slice 5d-i
//! wire shape with remote panes (same `Payload::OpenPane(..)`,
//! `Payload::AttachPane { .. }`, `Event::PaneOutput { .. }` — only
//! `OpenPaneSpec.target` changes between `Local` and `Remote {
//! alias }`). The opt-in remote pipeline is already covered in
//! `tests/remote_pane.rs`; this always-on variant pins the
//! multiplexing invariant without needing a live SSH daemon in CI.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneId, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_panes_multiplex_bytes_on_independent_subscriptions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");
    let daemon_handle = tokio::spawn({
        let sock_path = sock_path.clone();
        async move {
            tepegoz_core::run_daemon(tepegoz_core::DaemonConfig {
                socket_path: Some(sock_path),
            })
            .await
            .expect("daemon ran");
        }
    });
    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    let (mut r, mut w) = connect(&sock_path).await;

    // Open two panes — stand-ins for what the TUI does when the user
    // presses Ctrl-b Enter twice on the Fleet tile (target changes
    // from Local to Remote, wire shape unchanged).
    let pane_a = open_local_pane(&mut r, &mut w).await;
    let pane_b = open_local_pane(&mut r, &mut w).await;
    assert_ne!(
        pane_a.id, pane_b.id,
        "daemon must allocate distinct pane ids per Open"
    );

    // Attach on distinct subscription ids. Daemon routes `PaneOutput`
    // events back on the sub id that asked for them.
    const SUB_A: u64 = 201;
    const SUB_B: u64 = 202;
    attach(&mut w, pane_a.id, SUB_A).await;
    attach(&mut w, pane_b.id, SUB_B).await;

    // Distinct markers into each pane. `printf` to force a write even
    // if the test shell's PS1 is empty.
    send_input(&mut w, pane_a.id, b"printf MARKER_A_EEMC\\n\n").await;
    send_input(&mut w, pane_b.id, b"printf MARKER_B_YBFI\\n\n").await;

    let (collected_a, collected_b) = drain_until_both_markers(
        &mut r,
        SUB_A,
        SUB_B,
        b"MARKER_A_EEMC",
        b"MARKER_B_YBFI",
        Duration::from_secs(10),
    )
    .await;

    assert!(
        contains(&collected_a, b"MARKER_A_EEMC"),
        "pane A subscription carries A's bytes; got: {:?}",
        String::from_utf8_lossy(&collected_a)
    );
    assert!(
        contains(&collected_b, b"MARKER_B_YBFI"),
        "pane B subscription carries B's bytes; got: {:?}",
        String::from_utf8_lossy(&collected_b)
    );
    assert!(
        !contains(&collected_a, b"MARKER_B_YBFI"),
        "pane A subscription must NOT carry pane B's bytes — this is the \
         isolation invariant the pane-stack relies on. got: {:?}",
        String::from_utf8_lossy(&collected_a)
    );
    assert!(
        !contains(&collected_b, b"MARKER_A_EEMC"),
        "pane B subscription must NOT carry pane A's bytes. got: {:?}",
        String::from_utf8_lossy(&collected_b)
    );

    // Close pane A; pane B must stay alive so Ctrl-b & only closes the
    // active tab, not every pane in the stack.
    close_pane(&mut w, pane_a.id).await;
    send_input(&mut w, pane_b.id, b"printf MARKER_B_AFTER_CLOSE_A\\n\n").await;
    let after_close = drain_sub_until(
        &mut r,
        SUB_B,
        b"MARKER_B_AFTER_CLOSE_A",
        Duration::from_secs(5),
    )
    .await;
    assert!(
        contains(&after_close, b"MARKER_B_AFTER_CLOSE_A"),
        "pane B must keep streaming after pane A closes; got: {:?}",
        String::from_utf8_lossy(&after_close)
    );

    // `ListPanes` reflects the closure — pane A is either gone or no
    // longer alive, pane B still alive.
    let panes = list_panes(&mut r, &mut w).await;
    let a_entry = panes.iter().find(|p| p.id == pane_a.id);
    let b_entry = panes.iter().find(|p| p.id == pane_b.id);
    assert!(
        a_entry.map(|p| !p.alive).unwrap_or(true),
        "pane A must be dead after ClosePane; got {a_entry:?}"
    );
    assert!(
        b_entry.map(|p| p.alive).unwrap_or(false),
        "pane B must still be alive; got {b_entry:?}"
    );

    daemon_handle.abort();
}

// ---- helpers ----

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
            client_name: "pane-stack-test".into(),
        }),
    };
    write_envelope(&mut w, &hello).await.expect("hello");
    let welcome = read_envelope(&mut r).await.expect("welcome");
    assert!(
        matches!(welcome.payload, Payload::Welcome(_)),
        "expected Welcome, got {:?}",
        welcome.payload
    );
    (r, w)
}

async fn open_local_pane(
    r: &mut tokio::net::unix::OwnedReadHalf,
    w: &mut tokio::net::unix::OwnedWriteHalf,
) -> PaneInfo {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(OpenPaneSpec {
                target: tepegoz_proto::PaneTarget::Local,
                shell: Some("/bin/sh".into()),
                cwd: None,
                env: Vec::new(),
                rows: 24,
                cols: 80,
            }),
        },
    )
    .await
    .expect("openpane");
    let rep = read_envelope(r).await.expect("pane reply");
    match rep.payload {
        Payload::PaneOpened(info) => info,
        Payload::Error(e) => panic!("open failed: {:?} {}", e.kind, e.message),
        other => panic!("expected PaneOpened, got {other:?}"),
    }
}

async fn attach(w: &mut tokio::net::unix::OwnedWriteHalf, pane_id: PaneId, sub: u64) {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id,
                subscription_id: sub,
            },
        },
    )
    .await
    .expect("attach");
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

async fn close_pane(w: &mut tokio::net::unix::OwnedWriteHalf, pane_id: PaneId) {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::ClosePane { pane_id },
        },
    )
    .await
    .expect("close");
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
    .expect("listpanes");
    // ListPanes responses are sequential — read envelopes until we
    // see the PaneList (earlier events for the attached panes may
    // still be arriving).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!("no PaneList within deadline");
        }
        let rep = tokio::time::timeout(remaining, read_envelope(r))
            .await
            .expect("listpanes timeout")
            .expect("read envelope");
        match rep.payload {
            Payload::PaneList { panes } => return panes,
            Payload::Event(_) => continue, // drain live events
            other => panic!("expected PaneList, got {other:?}"),
        }
    }
}

async fn drain_until_both_markers(
    r: &mut tokio::net::unix::OwnedReadHalf,
    sub_a: u64,
    sub_b: u64,
    needle_a: &[u8],
    needle_b: &[u8],
    timeout: Duration,
) -> (Vec<u8>, Vec<u8>) {
    let mut a = Vec::new();
    let mut b = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok(env)) = tokio::time::timeout(remaining, read_envelope(r)).await else {
            break;
        };
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            if let Event::PaneSnapshot { scrollback, .. } = &event {
                append_to(&mut a, &mut b, subscription_id, sub_a, sub_b, scrollback);
            }
            if let Event::PaneOutput { data } = &event {
                append_to(&mut a, &mut b, subscription_id, sub_a, sub_b, data);
            }
        }
        if contains(&a, needle_a) && contains(&b, needle_b) {
            break;
        }
    }
    (a, b)
}

fn append_to(a: &mut Vec<u8>, b: &mut Vec<u8>, sub_id: u64, sub_a: u64, sub_b: u64, bytes: &[u8]) {
    if sub_id == sub_a {
        a.extend_from_slice(bytes);
    } else if sub_id == sub_b {
        b.extend_from_slice(bytes);
    }
}

async fn drain_sub_until(
    r: &mut tokio::net::unix::OwnedReadHalf,
    sub: u64,
    needle: &[u8],
    timeout: Duration,
) -> Vec<u8> {
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(Ok(env)) = tokio::time::timeout(remaining, read_envelope(r)).await else {
            break;
        };
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
            && subscription_id == sub
        {
            match event {
                Event::PaneSnapshot { scrollback, .. } => collected.extend_from_slice(&scrollback),
                Event::PaneOutput { data } => collected.extend_from_slice(&data),
                _ => {}
            }
        }
        if contains(&collected, needle) {
            break;
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
