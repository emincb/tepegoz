//! Phase 5 Slice 5b acceptance: `Subscribe(Fleet)` returns a `HostList`
//! plus one `HostStateChanged { state: Disconnected }` per host within
//! a few seconds. Slice 5c replaces the all-Disconnected emit with the
//! real connection-supervisor state machine.
//!
//! The hosts themselves come from whatever the test host has
//! (`~/.ssh/config`, `TEPEGOZ_SSH_HOSTS=` env, or tepegoz config.toml
//! per the Q2 precedence), so we don't assert specific aliases — only
//! structural invariants.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, HostState, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

const FLEET_SUB_ID: u64 = 97;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_subscription_emits_host_list_then_one_state_per_host() {
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
            payload: Payload::Subscribe(Subscription::Fleet { id: FLEET_SUB_ID }),
        },
    )
    .await
    .expect("subscribe");

    // Drain events until we see the initial HostList on our subscription
    // id. Budget generous for slow-CI ssh_config parsing.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut host_list: Option<(Vec<String>, String)> = None;
    while host_list.is_none() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("did not receive HostList within 10s budget");
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event: Event::HostList { hosts, source },
        }) = env.payload
            && subscription_id == FLEET_SUB_ID
        {
            assert!(
                !source.is_empty(),
                "HostList.source must be non-empty (e.g. 'ssh_config (...)' / '(none)')"
            );
            let aliases = hosts.into_iter().map(|h| h.alias).collect();
            host_list = Some((aliases, source));
        }
    }

    let (aliases, _source) = host_list.expect("HostList received");

    if aliases.is_empty() {
        // No hosts in the discovery environment — 5b emits HostList and
        // nothing further. That's a valid terminal state for CI hosts
        // without an ssh_config.
        daemon_handle.abort();
        return;
    }

    // One HostStateChanged { Disconnected } per host. Drain a few extras
    // silently (the subscription is still live, so a slow CI could
    // reorder between HostList and the state emits but within a small
    // window). Collect until we've seen every alias.
    let mut seen: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while seen.len() < aliases.len() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "did not receive HostStateChanged for every alias within 5s budget; \
                 expected {aliases:?}, saw {seen:?}"
            );
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event:
                Event::HostStateChanged {
                    alias,
                    state,
                    reason: _,
                },
        }) = env.payload
            && subscription_id == FLEET_SUB_ID
        {
            // Phase 5 Slice 5c-i: supervisor starts every host with
            // an initial `Disconnected` emit. Hosts with `ProxyJump`
            // set in ssh_config immediately re-transition to
            // `AuthFailed` (terminal — ProxyJump not supported in
            // Phase 5). Autoconnect hosts (config.toml-sourced) may
            // race into `Connecting` before we sample, but the test
            // host's discovery source is whatever the CI/dev machine
            // has — we accept any structurally-valid state as long
            // as we saw an emit for every alias.
            assert!(
                matches!(
                    state,
                    HostState::Disconnected
                        | HostState::Connecting
                        | HostState::Connected
                        | HostState::AuthFailed
                        | HostState::HostKeyMismatch
                ),
                "state must be a structurally-valid HostState variant, got {state:?}"
            );
            if !seen.contains(&alias) {
                seen.push(alias);
            }
        }
    }

    for alias in &aliases {
        assert!(
            seen.contains(alias),
            "expected a HostStateChanged for alias {alias:?}, only saw {seen:?}"
        );
    }

    daemon_handle.abort();
}

/// Phase 5 Slice 5c-i: supervisor integration test.
///
/// Provisions a tepegoz `config.toml` pointing a single host alias at
/// an `lscr.io/linuxserver/openssh-server` container with
/// `autoconnect = true`. Asserts the supervisor drives the full happy-
/// path state machine:
///
/// - initial Disconnected (supervisor seed)
/// - Connecting (autoconnect dial)
/// - Connected (handshake + auth succeeded)
///
/// Then kills the container mid-session and asserts:
///
/// - eventual Disconnected (within a 90 s budget — covers heartbeat
///   miss-threshold × 2 + transient-fail slack)
/// - a subsequent Connecting (reconnect attempt begins)
///
/// Optionally (if Degraded emits before Disconnected, which depends
/// on russh's keepalive timing against a just-killed container)
/// asserts ◐ Degraded appeared in the transition sequence.
///
/// Opt-in gated on `TEPEGOZ_SSH_TEST=1 + TEPEGOZ_DOCKER_TEST=1` — the
/// test needs Docker + ssh-keygen on PATH, which CI doesn't have.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fleet_supervisor_connects_autoconnect_host_and_reconnects_after_container_kill() {
    if std::env::var("TEPEGOZ_SSH_TEST").ok().as_deref() != Some("1")
        || std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1")
    {
        eprintln!("skipping: set TEPEGOZ_SSH_TEST=1 and TEPEGOZ_DOCKER_TEST=1 to enable");
        return;
    }

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let sock_path = tmp.path().join("daemon.sock");
    let config_dir = tmp.path().join("tepegoz-config");
    let data_dir = tmp.path().join("tepegoz-data");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    // Generate a keypair + start the openssh-server container.
    let key_path = config_dir.join("id_ed25519");
    let status = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            key_path.to_str().unwrap(),
            "-q",
            "-C",
            "tepegoz-5c-test",
        ])
        .status()
        .expect("ssh-keygen required on PATH");
    assert!(status.success());
    let pub_key = std::fs::read_to_string(key_path.with_extension("pub")).unwrap();
    let pub_key = pub_key.trim();

    // Start the container.
    let out = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm=false",
            "-e",
            "PUID=1000",
            "-e",
            "PGID=1000",
            "-e",
            "USER_NAME=tepegoz",
            "-e",
            &format!("PUBLIC_KEY={pub_key}"),
            "-p",
            "0:2222",
            "lscr.io/linuxserver/openssh-server:latest",
        ])
        .output()
        .expect("docker run");
    assert!(
        out.status.success(),
        "docker run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let container_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _container_guard = ContainerGuard(container_id.clone());

    // Query mapped port + wait for sshd readiness.
    let port_out = std::process::Command::new("docker")
        .args(["port", &container_id, "2222/tcp"])
        .output()
        .unwrap();
    let port_line = String::from_utf8_lossy(&port_out.stdout).trim().to_string();
    let port: u16 = port_line
        .rsplit(':')
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("failed to parse port from {port_line:?}"));
    wait_for_tcp(port, Duration::from_secs(30)).await;
    tokio::time::sleep(Duration::from_millis(500)).await; // sshd banner settle

    // Land a tepegoz config.toml with autoconnect=true + env overrides.
    let config_toml = format!(
        r#"
[[ssh.hosts]]
alias = "supervisor-test"
hostname = "127.0.0.1"
port = {port}
user = "tepegoz"
identity_file = "{}"
autoconnect = true
"#,
        key_path.display()
    );
    std::fs::write(config_dir.join("config.toml"), config_toml).unwrap();

    // Point tepegoz-ssh at our tempdir via the dev-affordance env vars.
    // Safety: test sets these on the current process which is also the
    // daemon's process (run_daemon runs as a spawned task, same env).
    // SAFETY: tests within the same process ordinarily need `std::sync::Mutex`
    // around env mutation, but cargo test runs each test in its own process
    // per target binary — this fleet_scope binary has exactly two tests and
    // the env setup here doesn't collide with the 5b test's setup.
    unsafe {
        std::env::set_var("TEPEGOZ_CONFIG_DIR", &config_dir);
        std::env::set_var("TEPEGOZ_DATA_DIR", &data_dir);
    }

    // Boot daemon + subscribe to Fleet.
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
            payload: Payload::Subscribe(Subscription::Fleet { id: FLEET_SUB_ID }),
        },
    )
    .await
    .expect("subscribe");

    // Phase 1: wait for HostList to arrive with our alias.
    let list_deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_list = false;
    while !saw_list {
        let remaining = list_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "did not receive HostList within 10s");
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event: Event::HostList { hosts, .. },
        }) = env.payload
            && subscription_id == FLEET_SUB_ID
        {
            let aliases: Vec<_> = hosts.iter().map(|h| h.alias.clone()).collect();
            assert!(
                aliases.contains(&"supervisor-test".to_string()),
                "expected 'supervisor-test' in HostList, got {aliases:?}"
            );
            saw_list = true;
        }
    }

    // Phase 2: observe Disconnected → Connecting → Connected within 15 s.
    //
    // Drain HostStateChanged events for our alias and track the
    // sequence until we reach Connected.
    let connect_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut transitions: Vec<HostState> = Vec::new();
    while !transitions.contains(&HostState::Connected) {
        let remaining = connect_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "did not observe Connected within 15s; saw transitions {transitions:?}"
        );
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event:
                Event::HostStateChanged {
                    alias,
                    state,
                    reason: _,
                },
        }) = env.payload
            && subscription_id == FLEET_SUB_ID
            && alias == "supervisor-test"
        {
            transitions.push(state);
        }
    }
    assert!(
        transitions.contains(&HostState::Disconnected)
            && transitions.contains(&HostState::Connecting)
            && transitions.contains(&HostState::Connected),
        "connect transitions missing a state: {transitions:?}"
    );

    // Phase 3: kill the container. Supervisor should transition
    // Connected → (Degraded) → Disconnected within ~90 s, then begin
    // reconnecting (we expect at least one more Connecting event).
    eprintln!("killing container to trigger heartbeat failure …");
    let _ = std::process::Command::new("docker")
        .args(["kill", &container_id])
        .output();

    let fail_deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut saw_disconnected = false;
    let mut saw_reconnecting = false;
    let mut post_kill_transitions: Vec<HostState> = Vec::new();
    while !(saw_disconnected && saw_reconnecting) {
        let remaining = fail_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "did not observe Disconnected+reconnect within 120s after kill; \
             post-kill transitions: {post_kill_transitions:?}"
        );
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event:
                Event::HostStateChanged {
                    alias,
                    state,
                    reason: _,
                },
        }) = env.payload
            && subscription_id == FLEET_SUB_ID
            && alias == "supervisor-test"
        {
            post_kill_transitions.push(state);
            if state == HostState::Disconnected {
                saw_disconnected = true;
            }
            if saw_disconnected && state == HostState::Connecting {
                saw_reconnecting = true;
            }
        }
    }
    assert!(saw_disconnected, "Disconnected was expected after kill");
    assert!(
        saw_reconnecting,
        "a reconnect attempt (Connecting after Disconnected) was expected"
    );

    // Degraded is a nice-to-have — russh 0.60's keepalive is fire-and-
    // forget, so fast TCP close may jump straight to Disconnected
    // without a Degraded-marker interim. Log either way.
    if post_kill_transitions.contains(&HostState::Degraded) {
        eprintln!("saw Degraded transition (russh keepalive reported partial miss)");
    } else {
        eprintln!(
            "did not see Degraded transition — TCP close was fast enough to \
             skip the miss-1 → Degraded step"
        );
    }

    // Phase 5 Slice 5c-ii extension: exercise the FleetAction wire.
    // The supervisor is currently cycling through backoff + reconnect
    // attempts against the dead container. Send FleetAction::Reconnect
    // with a known request_id; assert FleetActionResult::Success
    // comes back (dispatched). Supervisor's per-host channel receives
    // the action, resets backoff_idx to 0, and continues its loop.
    const RECONNECT_REQUEST_ID: u64 = 9001;
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::FleetAction(tepegoz_proto::FleetActionRequest {
                request_id: RECONNECT_REQUEST_ID,
                alias: "supervisor-test".into(),
                kind: tepegoz_proto::FleetActionKind::Reconnect,
            }),
        },
    )
    .await
    .expect("fleet action");

    let action_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_result = false;
    while !saw_result {
        let remaining = action_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "did not receive FleetActionResult within 5s"
        );
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read")
            .expect("read");
        if let Payload::FleetActionResult(res) = env.payload
            && res.request_id == RECONNECT_REQUEST_ID
        {
            assert_eq!(res.alias, "supervisor-test");
            assert_eq!(res.kind, tepegoz_proto::FleetActionKind::Reconnect);
            assert!(
                matches!(res.outcome, tepegoz_proto::FleetActionOutcome::Success),
                "Reconnect against a known alias should dispatch cleanly; got {:?}",
                res.outcome
            );
            saw_result = true;
        }
    }

    // Also verify an unknown alias returns Failure with a clear reason.
    const UNKNOWN_REQUEST_ID: u64 = 9002;
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::FleetAction(tepegoz_proto::FleetActionRequest {
                request_id: UNKNOWN_REQUEST_ID,
                alias: "does-not-exist".into(),
                kind: tepegoz_proto::FleetActionKind::Reconnect,
            }),
        },
    )
    .await
    .expect("fleet action (unknown alias)");

    let unknown_deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut saw_failure = false;
    while !saw_failure {
        let remaining = unknown_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "did not receive FleetActionResult (unknown alias) within 5s"
        );
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read")
            .expect("read");
        if let Payload::FleetActionResult(res) = env.payload
            && res.request_id == UNKNOWN_REQUEST_ID
        {
            match res.outcome {
                tepegoz_proto::FleetActionOutcome::Failure { reason } => {
                    assert!(
                        reason.contains("unknown alias"),
                        "expected 'unknown alias' in failure reason, got {reason:?}"
                    );
                }
                other => panic!("expected Failure for unknown alias, got {other:?}"),
            }
            saw_failure = true;
        }
    }

    daemon_handle.abort();
}

struct ContainerGuard(String);
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.0])
            .output();
    }
}

async fn wait_for_tcp(port: u16, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("tcp on 127.0.0.1:{port} did not come up in {budget:?}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_socket(path: &Path, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    while !path.exists() {
        if std::time::Instant::now() >= deadline {
            panic!("socket did not appear at {path:?} within {budget:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
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
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Hello(Hello {
                client_version: PROTOCOL_VERSION,
                client_name: "fleet_scope_test".into(),
            }),
        },
    )
    .await
    .expect("hello");
    // Drain the Welcome.
    let welcome = read_envelope(&mut r).await.expect("welcome");
    match welcome.payload {
        Payload::Welcome(_) => {}
        other => panic!("expected Welcome, got {other:?}"),
    }
    (r, w)
}
