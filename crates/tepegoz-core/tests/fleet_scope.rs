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
            event: Event::HostStateChanged { alias, state },
        }) = env.payload
            && subscription_id == FLEET_SUB_ID
        {
            // Phase 5 Slice 5b emits only Disconnected; 5c will drive
            // real transitions.
            assert_eq!(
                state,
                HostState::Disconnected,
                "Slice 5b emits `Disconnected` for every host; 5c adds real transitions"
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
