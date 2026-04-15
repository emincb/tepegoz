//! Phase 4 Slice 4d E2E acceptance: bind a known TCP port from a known
//! process, observe it simultaneously in both the Ports and Processes
//! subscriptions, kill the process, and confirm it disappears from
//! both. Exercises the full daemon pipeline (probe → wire → event
//! routing) in a way the per-probe integration tests in 4a/4b don't
//! cover together.
//!
//! Plus a bonus test gated on `TEPEGOZ_PROBE_TEST=1 TEPEGOZ_DOCKER_TEST=1`
//! that pins the README-mockup feature (`:3000 web (docker)`): a port
//! bound INSIDE a docker container surfaces on the Ports subscription
//! with `container: Some(<id>)`.
//!
//! Both tests use `kill_on_drop(true)` on `tokio::process::Command`
//! (the tokio equivalent of the 4b `ChildGuard` pattern) to prevent
//! leaked subprocesses if the test panics mid-way.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

const PORTS_SUB_ID: u64 = 71;
const PROCESSES_SUB_ID: u64 = 83;

/// Opt-in: drive the combined Ports + Processes feature end-to-end
/// through the daemon's wire. Spawn a python3 child that binds a
/// loopback TCP port and sleeps, wait for the child to announce it's
/// ready (so we don't race the probe against the bind), subscribe to
/// both `Ports` and `Processes`, assert the child appears in both
/// within a 6 s budget (2 s cadence + slack), then kill the child and
/// assert it disappears from both within 6 s.
///
/// Requires `python3` on PATH. Both GitHub CI runners (ubuntu-latest,
/// macos-latest) preinstall it; local devs without it will see a
/// clear spawn error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ports_and_processes_see_spawned_child_and_see_it_disappear() {
    if std::env::var("TEPEGOZ_PROBE_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_PROBE_TEST=1 to enable");
        return;
    }

    // Spawn a python child that binds an ephemeral loopback port,
    // prints the assigned port + "ready", and sleeps. `-u` unbuffers
    // stdout so the readiness line arrives synchronously.
    let script = r#"
import socket, sys, time
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen(1)
print(s.getsockname()[1], flush=True)
time.sleep(60)
"#;
    let mut child = tokio::process::Command::new("python3")
        .arg("-u")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect(
            "spawn python3 child — Phase 4 Slice 4d E2E test needs python3 on PATH \
             (preinstalled on GitHub CI ubuntu-latest + macos-latest)",
        );
    let child_pid = child.id().expect("child pid");

    // Read the port the child bound. Wait with a short budget so a
    // broken python (missing socket module, etc.) surfaces fast.
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout).lines();
    let line = tokio::time::timeout(Duration::from_secs(5), reader.next_line())
        .await
        .expect("child must print port within 5 s")
        .expect("read line")
        .expect("child produced a line");
    let child_port: u16 = line
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("child's port line not a u16: {line:?} ({e})"));
    eprintln!("child pid={child_pid} bound TCP 127.0.0.1:{child_port}");

    // Start the daemon.
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

    // Subscribe to BOTH Ports and Processes.
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
    .expect("subscribe ports");
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
    .expect("subscribe processes");

    // Drain events until we've seen the child in BOTH Ports and
    // Processes. Budget: 6 s covers one refresh boundary on each.
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let mut saw_port = false;
    let mut saw_process = false;
    while !(saw_port && saw_process) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "within 6 s: saw_port={saw_port} saw_process={saw_process} \
                 (expected both)"
            );
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");

        let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        else {
            continue;
        };
        if subscription_id == PORTS_SUB_ID {
            if let Event::PortList { ports, .. } = event {
                if ports
                    .iter()
                    .any(|p| p.local_port == child_port && p.pid == child_pid)
                {
                    saw_port = true;
                }
            }
        } else if subscription_id == PROCESSES_SUB_ID {
            if let Event::ProcessList { rows, .. } = event {
                if let Some(row) = rows.iter().find(|p| p.pid == child_pid) {
                    assert!(
                        row.command.to_lowercase().contains("python"),
                        "child process command must identify as python; got {:?}",
                        row.command
                    );
                    saw_process = true;
                }
            }
        }
    }
    eprintln!("confirmed child in Ports AND Processes");

    // Kill the child. `kill_on_drop(true)` drops handle immediately
    // above when we call `child.start_kill()`; we await the wait() so
    // the zombie reaps cleanly and the kernel reissues the pid freeing.
    child.start_kill().expect("start_kill");
    child.wait().await.expect("wait");
    eprintln!("killed child pid={child_pid}");

    // Drain events until the child has disappeared from BOTH Ports
    // and Processes. Budget: 6 s.
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let mut gone_from_ports = false;
    let mut gone_from_processes = false;
    while !(gone_from_ports && gone_from_processes) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "within 6 s after kill: gone_from_ports={gone_from_ports} \
                 gone_from_processes={gone_from_processes} (expected both)"
            );
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");
        let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        else {
            continue;
        };
        if subscription_id == PORTS_SUB_ID {
            if let Event::PortList { ports, .. } = event {
                let still_there = ports.iter().any(|p| p.local_port == child_port);
                if !still_there {
                    gone_from_ports = true;
                }
            }
        } else if subscription_id == PROCESSES_SUB_ID {
            if let Event::ProcessList { rows, .. } = event {
                let still_there = rows.iter().any(|p| p.pid == child_pid);
                if !still_there {
                    gone_from_processes = true;
                }
            }
        }
    }
    eprintln!("confirmed child disappeared from Ports AND Processes");

    daemon_handle.abort();
}

/// Opt-in on BOTH `TEPEGOZ_PROBE_TEST=1` AND `TEPEGOZ_DOCKER_TEST=1`:
/// container correlation is THE distinguishing feature per the README
/// mockup's `:3000 web (docker)`, so it deserves an E2E pin. Start an
/// alpine container publishing a port; subscribe to Ports; assert the
/// row for that port carries `container: Some(<id>)` within a 6 s
/// budget. Force-remove the container on Drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_bound_port_surfaces_with_container_correlation() {
    if std::env::var("TEPEGOZ_PROBE_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_PROBE_TEST=1 + TEPEGOZ_DOCKER_TEST=1 to enable");
        return;
    }
    if std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_DOCKER_TEST=1 (in addition to PROBE) to enable");
        return;
    }

    // Pick a high port we don't expect to be in use. PID-derived so
    // parallel test runs don't collide.
    let host_port: u16 = 40000u16 + (std::process::id() as u16 & 0x1fff);
    let container_name = format!("tepegoz-4d-e2e-{}", std::process::id());

    // Start the container: alpine publishing a port, just sleeping.
    // We don't actually need anything listening INSIDE the container
    // for this test; bollard's container.Ports will still report the
    // host-side public_port binding, which is what the daemon's
    // correlation logic matches against.
    let status = tokio::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &container_name,
            "-p",
            &format!("{host_port}:80"),
            "alpine",
            "sleep",
            "120",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .expect("spawn docker run");
    assert!(
        status.success(),
        "docker run must succeed; check dockerd is available for TEPEGOZ_DOCKER_TEST=1"
    );
    let _guard = DockerContainerGuard(container_name.clone());

    // Start the daemon.
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
    .expect("subscribe ports");

    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("within 6 s: did not see port {host_port} carrying container correlation");
        }
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read");
        let Payload::Event(EventFrame {
            subscription_id: PORTS_SUB_ID,
            event: Event::PortList { ports, .. },
        }) = env.payload
        else {
            continue;
        };
        if let Some(row) = ports.iter().find(|p| p.local_port == host_port) {
            assert!(
                row.container_id.is_some(),
                "port row for docker-bound :{host_port} must carry \
                 container: Some(...); got {:?}",
                row
            );
            eprintln!(
                "confirmed port {host_port} correlated to container {:?}",
                row.container_id
            );
            break;
        }
    }

    daemon_handle.abort();
}

/// Force-remove the docker container on Drop so a panic mid-test
/// doesn't leak it. Synchronous + best-effort — Drop can't await.
struct DockerContainerGuard(String);

impl Drop for DockerContainerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

// ---- helpers (duplicated per integration-test-file convention) ----

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
            client_name: "ports-processes-e2e-test".into(),
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
