//! Phase 4 Slice 4a acceptance: `Subscribe(Ports)` returns either a
//! `PortList` (when the native probe succeeds) or a `PortsUnavailable`
//! (when it fails). Both are valid; what matters is that the daemon
//! doesn't panic, doesn't hang, and reaches *some* terminal state within
//! a few seconds.
//!
//! An opt-in test gated on `TEPEGOZ_PROBE_TEST=1` provisions a local TCP
//! listener and asserts the probe finds it with the expected port + pid
//! within a small budget.

use std::path::Path;
use std::time::Duration;

use tokio::net::{TcpListener, UnixStream};

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

const PORTS_SUB_ID: u64 = 71;

/// Wait up to this long for the probe's first event after Subscribe. The
/// probe's initial poll is synchronous work on the tokio blocking pool;
/// give it a generous budget on slow CI hosts.
const FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ports_subscription_emits_either_port_list_or_unavailable() {
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
            payload: Payload::Subscribe(Subscription::Ports {
                id: PORTS_SUB_ID,
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        },
    )
    .await
    .expect("subscribe");

    let event = read_first_ports_event(&mut r).await;

    match event {
        Event::PortList { ports: _, source } => {
            assert!(
                !source.is_empty(),
                "PortList source must be non-empty (e.g. `linux-procfs`, `macos-libproc`)"
            );
            // `ports` may be empty or non-empty — test hosts have arbitrary
            // listeners. We only assert structural invariants (see opt-in
            // test for a provisioned-listener round trip).
        }
        Event::PortsUnavailable { reason } => {
            assert!(
                !reason.is_empty(),
                "PortsUnavailable must carry a non-empty reason — clients render it directly"
            );
        }
        other => panic!("expected PortList or PortsUnavailable, got {other:?}"),
    }

    daemon_handle.abort();
}

/// Opt-in: bind a known TCP port in the test process; assert the probe
/// finds it. Enable with `TEPEGOZ_PROBE_TEST=1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ports_subscription_sees_locally_bound_listener_within_budget() {
    if std::env::var("TEPEGOZ_PROBE_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_PROBE_TEST=1 to enable");
        return;
    }

    // Bind an ephemeral TCP port in this process so we own it and the
    // probe can attribute it without elevated privileges.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let want_port = listener.local_addr().expect("local addr").port();
    let test_pid = std::process::id();
    eprintln!("provisioned TCP listener on 127.0.0.1:{want_port} pid={test_pid}");

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
            payload: Payload::Subscribe(Subscription::Ports {
                id: PORTS_SUB_ID,
                target: tepegoz_proto::ScopeTarget::Local,
            }),
        },
    )
    .await
    .expect("subscribe");

    // Drain events until we see our provisioned port, or the budget expires.
    // First PortList should contain it — the probe runs on subscribe — but
    // if the probe task was mid-sleep at bind time, one refresh-cycle wait
    // (PORTS_REFRESH_INTERVAL = 2s) is the worst case. Budget = 6s covers
    // that plus slow-CI slack.
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("did not observe port {want_port} in PortList within 6s budget");
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");

        let Payload::Event(EventFrame {
            subscription_id,
            event: Event::PortList { ports, source },
        }) = env.payload
        else {
            continue;
        };
        assert_eq!(subscription_id, PORTS_SUB_ID);

        if let Some(found) = ports.iter().find(|p| p.local_port == want_port) {
            assert_eq!(found.protocol, "tcp");
            assert_eq!(
                found.pid, test_pid,
                "probe must attribute the listener to the test process's pid \
                 (source={source}, row={found:?})"
            );
            assert!(
                !found.process_name.is_empty(),
                "process_name must resolve for an owned pid (row={found:?})"
            );
            eprintln!(
                "probe confirmed port {want_port} pid={} name={:?} source={source}",
                found.pid, found.process_name
            );
            break;
        }
    }

    drop(listener);
    daemon_handle.abort();
}

async fn read_first_ports_event(r: &mut tokio::net::unix::OwnedReadHalf) -> Event {
    let env = tokio::time::timeout(FIRST_EVENT_TIMEOUT, read_envelope(r))
        .await
        .expect("ports subscription must produce an event within timeout")
        .expect("read envelope");
    match env.payload {
        Payload::Event(EventFrame {
            subscription_id,
            event,
        }) => {
            assert_eq!(
                subscription_id, PORTS_SUB_ID,
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
            client_name: "ports-acceptance-test".into(),
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
