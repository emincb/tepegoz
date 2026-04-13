//! Phase 3 Slice A acceptance: a `Subscribe(Docker)` returns either a
//! `ContainerList` (when docker is reachable) or a `DockerUnavailable` (when
//! it's not). Both are valid; what matters is that the daemon doesn't panic,
//! doesn't hang, and reaches *some* terminal state within a few seconds.
//!
//! A second test gated on `TEPEGOZ_DOCKER_TEST=1` insists on the Available
//! path — it requires a running engine and is meant for local + CI runs that
//! provision docker beforehand.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

const DOCKER_SUB_ID: u64 = 7;

/// Wait up to this long for the daemon to deliver a docker event after we
/// subscribe. Generous because `Engine::connect` walks every candidate
/// socket — each probe times out at 5s if a socket exists but doesn't
/// answer (e.g., a Docker Desktop install that's not running).
const FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_subscription_emits_either_container_list_or_unavailable() {
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
            payload: Payload::Subscribe(Subscription::Docker { id: DOCKER_SUB_ID }),
        },
    )
    .await
    .expect("subscribe");

    let event = read_first_docker_event(&mut r).await;

    match event {
        Event::ContainerList {
            containers,
            engine_source,
        } => {
            // Engine is reachable. Containers may legitimately be empty (no
            // containers running). Just verify the source label is non-empty.
            assert!(
                !engine_source.is_empty(),
                "engine_source must identify which docker we connected to"
            );
            eprintln!(
                "docker available via {engine_source}; {} container(s)",
                containers.len()
            );
        }
        Event::DockerUnavailable { reason } => {
            assert!(
                !reason.is_empty(),
                "DockerUnavailable must carry a non-empty reason"
            );
            assert!(
                reason.to_lowercase().contains("docker") || reason.contains("socket"),
                "reason should mention docker or sockets so the user can act on it: {reason:?}"
            );
            eprintln!("docker unavailable: {reason}");
        }
        other => panic!("expected ContainerList or DockerUnavailable, got {other:?}"),
    }

    daemon_handle.abort();
}

/// Opt-in test: insists on the available path. Run via:
/// `TEPEGOZ_DOCKER_TEST=1 cargo test -p tepegoz-core --test docker_scope`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_subscription_returns_container_list_when_engine_is_running() {
    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_DOCKER_TEST=1 to enable (requires running docker)");
        return;
    }

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
            payload: Payload::Subscribe(Subscription::Docker { id: DOCKER_SUB_ID }),
        },
    )
    .await
    .expect("subscribe");

    let event = read_first_docker_event(&mut r).await;
    match event {
        Event::ContainerList { engine_source, .. } => {
            assert!(!engine_source.is_empty());
        }
        Event::DockerUnavailable { reason } => {
            panic!("TEPEGOZ_DOCKER_TEST=1 requires a reachable docker engine, but: {reason}");
        }
        other => panic!("expected ContainerList, got {other:?}"),
    }

    daemon_handle.abort();
}

// ---- helpers ----

async fn read_first_docker_event(r: &mut tokio::net::unix::OwnedReadHalf) -> Event {
    let env = tokio::time::timeout(FIRST_EVENT_TIMEOUT, read_envelope(r))
        .await
        .expect("docker subscription must produce an event within timeout")
        .expect("read envelope");
    match env.payload {
        Payload::Event(EventFrame {
            subscription_id,
            event,
        }) => {
            assert_eq!(
                subscription_id, DOCKER_SUB_ID,
                "event must reference our subscription id, not someone else's"
            );
            event
        }
        Payload::Welcome(_) | Payload::Pong => panic!("unexpected non-event payload"),
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
            client_name: "docker-acceptance-test".into(),
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon socket never appeared at {}", path.display());
}
