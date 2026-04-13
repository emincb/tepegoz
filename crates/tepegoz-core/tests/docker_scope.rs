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
    DockerActionKind, DockerActionOutcome, DockerActionRequest, Envelope, Event, EventFrame, Hello,
    PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

const DOCKER_SUB_ID: u64 = 7;
const LOGS_SUB_ID: u64 = 8;
const STATS_SUB_ID: u64 = 9;
const ACTION_REQ_ID: u64 = 100;

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

/// `DockerAction` against an unreachable engine must come back as
/// `DockerActionResult { outcome: Failure }` carrying a useful reason —
/// not silently dropped, not panicking, not the wrong request_id.
///
/// This is the failure-path companion to the opt-in
/// docker_action_restart_against_known_container Available-path test below.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_action_against_unreachable_engine_returns_failure_with_reason() {
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

    // Skip when docker IS reachable (this test asserts the Failure path).
    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() == Some("1") {
        eprintln!(
            "skipping: TEPEGOZ_DOCKER_TEST=1 set — this test asserts the unreachable-engine path"
        );
        daemon_handle.abort();
        return;
    }

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerAction(DockerActionRequest {
                request_id: ACTION_REQ_ID,
                container_id: "no-such-container".into(),
                kind: DockerActionKind::Restart,
            }),
        },
    )
    .await
    .expect("send action");

    let env = tokio::time::timeout(FIRST_EVENT_TIMEOUT, read_envelope(&mut r))
        .await
        .expect("must produce a result within the connect-probe budget")
        .expect("read");

    match env.payload {
        Payload::DockerActionResult(res) => {
            assert_eq!(res.request_id, ACTION_REQ_ID);
            assert_eq!(res.container_id, "no-such-container");
            assert_eq!(res.kind, DockerActionKind::Restart);
            match res.outcome {
                DockerActionOutcome::Failure { reason } => {
                    assert!(
                        !reason.is_empty(),
                        "Failure must carry a non-empty reason — clients render it directly"
                    );
                }
                DockerActionOutcome::Success => {
                    panic!("expected Failure (no engine reachable), got Success");
                }
            }
        }
        other => panic!("expected DockerActionResult, got {other:?}"),
    }

    daemon_handle.abort();
}

/// `Subscribe(DockerLogs)` against an unreachable engine must terminate
/// cleanly with `Event::DockerStreamEnded` — without that signal a UI
/// would spin forever waiting for log chunks that won't come.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_logs_against_unreachable_engine_emits_stream_ended() {
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

    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() == Some("1") {
        eprintln!("skipping: this test asserts the unreachable-engine path");
        daemon_handle.abort();
        return;
    }

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::DockerLogs {
                id: LOGS_SUB_ID,
                container_id: "no-such-container".into(),
                follow: true,
                tail_lines: 0,
            }),
        },
    )
    .await
    .expect("send subscribe");

    let event = read_event_for(&mut r, LOGS_SUB_ID).await;
    match event {
        Event::DockerStreamEnded { reason } => {
            assert!(!reason.is_empty());
            assert!(
                reason.to_lowercase().contains("engine") || reason.contains("docker"),
                "reason should mention the engine being unavailable, got: {reason:?}"
            );
        }
        other => panic!("expected DockerStreamEnded, got {other:?}"),
    }

    daemon_handle.abort();
}

/// Same shape as the logs failure path: stats subscription against an
/// unreachable engine must reach a terminal `DockerStreamEnded` event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_stats_against_unreachable_engine_emits_stream_ended() {
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

    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() == Some("1") {
        eprintln!("skipping: this test asserts the unreachable-engine path");
        daemon_handle.abort();
        return;
    }

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::DockerStats {
                id: STATS_SUB_ID,
                container_id: "no-such-container".into(),
            }),
        },
    )
    .await
    .expect("send subscribe");

    let event = read_event_for(&mut r, STATS_SUB_ID).await;
    match event {
        Event::DockerStreamEnded { reason } => {
            assert!(!reason.is_empty());
        }
        other => panic!("expected DockerStreamEnded, got {other:?}"),
    }

    daemon_handle.abort();
}

/// Available-path acceptance: provision a short-lived alpine container,
/// restart it, subscribe to its logs, send a marker via SendInput-equivalent
/// (we use a container that prints a marker on start), and verify each
/// surface area produces the expected event.
///
/// Opt-in via `TEPEGOZ_DOCKER_TEST=1` (requires running docker + ability to
/// pull `alpine:latest` if not already cached).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_action_logs_stats_end_to_end_against_real_engine() {
    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_DOCKER_TEST=1 to enable (requires running docker)");
        return;
    }

    // Provision a container that lives long enough to subscribe + restart +
    // observe stats, and produces deterministic stdout for the logs check.
    // Use a 60s sleep so the container outlives the test by far.
    let container_name = format!("tepegoz-test-{}", std::process::id());
    let run = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &container_name,
            "alpine:latest",
            "sh",
            "-c",
            "echo TEPEGOZ_LOG_MARKER; sleep 60",
        ])
        .output()
        .expect("docker run");
    assert!(
        run.status.success(),
        "docker run failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let container_id = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert!(!container_id.is_empty(), "docker run returned no id");

    // Always teardown, even on test panic.
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }
    let _cleanup = Cleanup(container_name.clone());

    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");
    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });
    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    // ---- Logs: marker must appear ----
    let (mut r, mut w) = connect(&sock_path).await;
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::DockerLogs {
                id: LOGS_SUB_ID,
                container_id: container_id.clone(),
                follow: true,
                tail_lines: 0,
            }),
        },
    )
    .await
    .expect("subscribe logs");

    let mut found_marker = false;
    let logs_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < logs_deadline {
        match tokio::time::timeout(Duration::from_secs(2), read_envelope(&mut r)).await {
            Ok(Ok(env)) => {
                if let Payload::Event(EventFrame {
                    subscription_id: LOGS_SUB_ID,
                    event: Event::ContainerLog { data, .. },
                }) = env.payload
                {
                    if data
                        .windows(b"TEPEGOZ_LOG_MARKER".len())
                        .any(|w| w == b"TEPEGOZ_LOG_MARKER")
                    {
                        found_marker = true;
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    assert!(found_marker, "log marker TEPEGOZ_LOG_MARKER never arrived");

    // ---- Stats: at least one sample with sane mem_bytes ----
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::DockerStats {
                id: STATS_SUB_ID,
                container_id: container_id.clone(),
            }),
        },
    )
    .await
    .expect("subscribe stats");

    let mut got_stats = false;
    let stats_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < stats_deadline {
        match tokio::time::timeout(Duration::from_secs(3), read_envelope(&mut r)).await {
            Ok(Ok(env)) => {
                if let Payload::Event(EventFrame {
                    subscription_id: STATS_SUB_ID,
                    event: Event::ContainerStats(s),
                }) = env.payload
                {
                    assert!(
                        s.mem_bytes > 0,
                        "alpine sleep should have nonzero RSS, got {}",
                        s.mem_bytes
                    );
                    got_stats = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(got_stats, "ContainerStats event never arrived");

    // ---- Action: restart succeeds ----
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerAction(DockerActionRequest {
                request_id: ACTION_REQ_ID,
                container_id: container_id.clone(),
                kind: DockerActionKind::Restart,
            }),
        },
    )
    .await
    .expect("send action");

    let mut got_action_result = false;
    let action_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < action_deadline {
        match tokio::time::timeout(Duration::from_secs(3), read_envelope(&mut r)).await {
            Ok(Ok(env)) => {
                if let Payload::DockerActionResult(res) = env.payload {
                    assert_eq!(res.request_id, ACTION_REQ_ID);
                    assert_eq!(res.kind, DockerActionKind::Restart);
                    match res.outcome {
                        DockerActionOutcome::Success => {
                            got_action_result = true;
                            break;
                        }
                        DockerActionOutcome::Failure { reason } => {
                            panic!("restart failed: {reason}");
                        }
                    }
                }
            }
            _ => break,
        }
    }
    assert!(got_action_result, "DockerActionResult never arrived");

    daemon_handle.abort();
}

/// Slice C2c3 end-to-end: Subscribe(Docker) must deliver a `ContainerList`
/// *containing a specific provisioned container* within 2 seconds when
/// the engine is reachable. This is the signature assertion from the
/// CTO's C2c3 ask — distinct from the Slice A acceptance test (which
/// just checks that *some* event arrives) in that it pins both:
///
/// 1. Timing. The daemon's refresh interval is 2 s; subscribe→first-list
///    should be faster than that because `Engine::connect` + first
///    `list_containers` run inline on subscribe before the interval tick.
///    If this deadline slips, mode-switching Ctrl-b s into scope view
///    would render a stale "Connecting…" for too long and feel broken.
/// 2. Content. The ContainerList must actually contain the container we
///    provisioned. A list that's technically empty or wrong would pass
///    the Slice A test but fail the user's eyeball.
///
/// Opt-in via `TEPEGOZ_DOCKER_TEST=1` (requires running docker + ability
/// to pull `alpine:latest` if not already cached). Provisions a unique-
/// per-PID container so concurrent runs don't collide; force-removes on
/// Drop so panics don't leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_scope_lists_provisioned_container_within_2s() {
    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_DOCKER_TEST=1 to enable (requires running docker)");
        return;
    }

    let container_name = format!("tepegoz-c2c3-{}", std::process::id());
    let run = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &container_name,
            "alpine:latest",
            "sleep",
            "120",
        ])
        .output()
        .expect("docker run");
    assert!(
        run.status.success(),
        "docker run failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }
    let _cleanup = Cleanup(container_name.clone());

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
    let started = std::time::Instant::now();
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Docker { id: DOCKER_SUB_ID }),
        },
    )
    .await
    .expect("subscribe");

    // The first ContainerList for a reachable engine should arrive well
    // under 2 s. We allow a slightly higher bound for the automated
    // timeout so CI variance doesn't turn transient slowness into a
    // false positive — but we ALSO assert the measured elapsed time is
    // under 2 s so the "feels broken" threshold is pinned.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut list_elapsed = None;
    let mut list_contained_our_container = false;
    loop {
        if list_elapsed.is_some() {
            break;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("first event within 5 s")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            if subscription_id == DOCKER_SUB_ID {
                match event {
                    Event::ContainerList { containers, .. } => {
                        list_elapsed = Some(started.elapsed());
                        list_contained_our_container = containers.iter().any(|c| {
                            c.names
                                .iter()
                                .any(|n| n.trim_start_matches('/') == container_name)
                        });
                    }
                    Event::DockerUnavailable { reason } => {
                        panic!("TEPEGOZ_DOCKER_TEST=1 requires reachable docker, got: {reason}");
                    }
                    _ => {}
                }
            }
        }
    }

    let elapsed = list_elapsed.expect("ContainerList must arrive within the deadline");
    assert!(
        elapsed < Duration::from_secs(2),
        "ContainerList took {elapsed:?}; must be < 2s for the scope view to feel responsive on Ctrl-b s"
    );
    assert!(
        list_contained_our_container,
        "ContainerList must contain the provisioned container {container_name:?}; \
         otherwise the user sees the list populate but the container they just started is missing"
    );

    daemon_handle.abort();
}

/// Slice C3c acceptance: a client-initiated `Restart` must produce
/// `DockerActionResult::Success` with matching `request_id` AND the
/// daemon's next `ContainerList` must reflect the restart — the
/// container's `state` / `status` shift from the pre-restart
/// snapshot. This pins the full round-trip: TUI → DockerAction →
/// daemon → engine → Success → next refresh → visible state change.
/// If the daemon's `Subscribe(Docker)` poller didn't repoll after an
/// action completed, the list would go stale and this would fail;
/// if `DockerAction` dispatch didn't correlate `request_id`, the
/// client would never know its action succeeded. Both failure modes
/// pass the unit tests and fail here.
///
/// We allow either a `status` string change (the common case —
/// Docker's "Up N seconds" resets to "Up Less than a second" on
/// Restart, which is deterministically a different string after the
/// pre-restart sleep) or a `state` change (caught mid-restart:
/// "restarting" / "created" briefly) — both prove the daemon
/// refreshed after the action and the list is not stale.
///
/// Opt-in via `TEPEGOZ_DOCKER_TEST=1`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_propagates_to_follow_up_container_list() {
    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_DOCKER_TEST=1 to enable (requires running docker)");
        return;
    }

    let container_name = format!("tepegoz-c3c-{}", std::process::id());
    let run = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &container_name,
            "alpine:latest",
            "sleep",
            "120",
        ])
        .output()
        .expect("docker run");
    assert!(
        run.status.success(),
        "docker run failed: stderr={:?}",
        String::from_utf8_lossy(&run.stderr)
    );
    let container_id = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert!(!container_id.is_empty(), "docker run returned no id");

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }
    let _cleanup = Cleanup(container_name.clone());

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

    // Snapshot the pre-restart state + status of our container from the
    // first ContainerList that mentions it.
    let (pre_state, pre_status) = loop {
        let env = tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut r))
            .await
            .expect("initial ContainerList must arrive within 5 s")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id: DOCKER_SUB_ID,
            event: Event::ContainerList { containers, .. },
        }) = env.payload
        {
            if let Some(c) = containers.iter().find(|c| {
                c.names
                    .iter()
                    .any(|n| n.trim_start_matches('/') == container_name)
            }) {
                assert_eq!(
                    c.state, "running",
                    "container must be running pre-restart, got {:?}",
                    c.state
                );
                break (c.state.clone(), c.status.clone());
            }
        }
    };

    // Let the "Up N seconds" counter tick up so it visibly differs from
    // the post-restart reset (pre-restart ~Up 2s, post-restart ~Up
    // Less than a second).
    tokio::time::sleep(Duration::from_secs(2)).await;

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerAction(DockerActionRequest {
                request_id: ACTION_REQ_ID,
                container_id: container_id.clone(),
                kind: DockerActionKind::Restart,
            }),
        },
    )
    .await
    .expect("send action");

    // Read until DockerActionResult::Success for our request_id, then
    // keep reading until a fresh ContainerList shows a shifted
    // state/status for our container. 30 s budget covers (1) the
    // restart itself (usually <1 s for alpine sleep), (2) the daemon's
    // next Docker poll (every 2 s), (3) CI variance.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut got_success = false;
    let mut post_reflected = false;
    while tokio::time::Instant::now() < deadline && (!got_success || !post_reflected) {
        let Ok(Ok(env)) = tokio::time::timeout(Duration::from_secs(3), read_envelope(&mut r)).await
        else {
            continue;
        };
        match env.payload {
            Payload::DockerActionResult(res) if res.request_id == ACTION_REQ_ID => {
                assert_eq!(res.container_id, container_id);
                assert_eq!(res.kind, DockerActionKind::Restart);
                match res.outcome {
                    DockerActionOutcome::Success => got_success = true,
                    DockerActionOutcome::Failure { reason } => {
                        panic!("Restart of {container_name} failed: {reason}")
                    }
                }
            }
            Payload::Event(EventFrame {
                subscription_id: DOCKER_SUB_ID,
                event: Event::ContainerList { containers, .. },
            }) => {
                // Only count ContainerLists that come AFTER the Success
                // result. Before Success, the list is still "pre" —
                // any shift there would be spurious (e.g. the daemon's
                // 2 s refresh tick firing between our subscribe and
                // the action).
                if !got_success {
                    continue;
                }
                if let Some(c) = containers.iter().find(|c| {
                    c.names
                        .iter()
                        .any(|n| n.trim_start_matches('/') == container_name)
                }) {
                    if c.state != pre_state || c.status != pre_status {
                        post_reflected = true;
                    }
                }
            }
            _ => {}
        }
    }
    assert!(
        got_success,
        "DockerActionResult::Success for request_id {ACTION_REQ_ID} never arrived"
    );
    assert!(
        post_reflected,
        "follow-up ContainerList never reflected the Restart: \
         pre=(state={pre_state:?}, status={pre_status:?}) but no subsequent \
         list showed a shift for {container_name:?}. Either the daemon's \
         poller didn't repoll after the action, or the action didn't hit \
         the engine at all."
    );

    daemon_handle.abort();
}

// ---- helpers ----

/// Read events on the connection until one references `target_sub_id`,
/// dropping any other-subscription events that interleave.
async fn read_event_for(r: &mut tokio::net::unix::OwnedReadHalf, target_sub_id: u64) -> Event {
    let deadline = tokio::time::Instant::now() + FIRST_EVENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let env = tokio::time::timeout(remaining, read_envelope(r))
            .await
            .expect("event must arrive within timeout")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            if subscription_id == target_sub_id {
                return event;
            }
        }
    }
}

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
