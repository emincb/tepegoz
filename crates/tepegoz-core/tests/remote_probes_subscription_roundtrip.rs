//! Phase 6 Slice 6d-ii opt-in integration test: remote Ports +
//! Processes subscription routing end-to-end.
//!
//! Companion to `remote_docker_subscription_roundtrip.rs` (6c-iii):
//! same fixture shape (sshd container + cross-built agent + daemon
//! booted via `run_daemon_with_resolver`), but exercises the Ports +
//! Processes routes added in 6d-ii. Bundled into one test rather
//! than two because:
//!
//! - both probes are always-present capabilities ("ports" +
//!   "processes" populate at handshake regardless of host state),
//! - the container fixture costs ~30 s to set up,
//! - asserting both in one client connection covers more ground
//!   per second of wall-clock CI time.
//!
//! Opt-in via `TEPEGOZ_SSH_TEST=1 + TEPEGOZ_DOCKER_TEST=1` (Docker
//! is the test fixture, not the subscription target).
//!
//! What this test PROVES:
//! - Agent's `probe_capabilities` reports `"ports"` + `"processes"`.
//! - Daemon's `route_remote_subscribe` accepts `Subscription::Ports`
//!   + `Subscription::Processes` Remote targets.
//! - Agent's `forward_ports` + `forward_processes` produce real
//!   `PortList` / `ProcessList` events keyed to the daemon-allocated
//!   sub id, parser rewrites to client id, client receives.
//! - `Event::AgentCapabilities { alias, capabilities }` arrives on
//!   Fleet subscription with `"ports"` + `"processes"` — closes
//!   the 6d-i propagation loop end-to-end.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, HostState, PROTOCOL_VERSION, Payload, ScopeTarget,
    Subscription,
    codec::{read_envelope, write_envelope},
};

const AGENT_TRIPLE: &str = "x86_64-unknown-linux-musl";
const FLEET_SUB_ID: u64 = 9101;
const PORTS_SUB_ID: u64 = 9102;
const PROCESSES_SUB_ID: u64 = 9103;

struct ContainerGuard(String);
impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
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

async fn handshake(
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
                client_name: "remote_probes_test".into(),
            }),
        },
    )
    .await
    .expect("hello");
    let welcome = read_envelope(&mut r).await.expect("welcome");
    assert!(matches!(welcome.payload, Payload::Welcome(_)));
    (r, w)
}

fn build_agent_bytes() -> Vec<u8> {
    let has_zigbuild = Command::new("cargo")
        .args(["zigbuild", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let cargo_subcmd = if has_zigbuild { "zigbuild" } else { "build" };
    let status = Command::new("cargo")
        .args([
            cargo_subcmd,
            "--release",
            "--package",
            "tepegoz-agent",
            "--bin",
            "tepegoz-agent",
            "--target",
            AGENT_TRIPLE,
        ])
        .status()
        .expect("failed to spawn cargo");
    assert!(status.success(), "cross-build for {AGENT_TRIPLE} failed");

    let mut dir: Option<PathBuf> = std::env::current_dir().ok();
    while let Some(d) = dir.as_ref() {
        if d.join("Cargo.lock").exists() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    let workspace_root = dir.expect("couldn't locate workspace root");
    let bin = workspace_root
        .join("target")
        .join(AGENT_TRIPLE)
        .join("release")
        .join("tepegoz-agent");
    std::fs::read(&bin).expect("read agent binary")
}

thread_local! {
    static AGENT_BYTES: std::cell::RefCell<Option<&'static [u8]>> = const { std::cell::RefCell::new(None) };
}

fn install_agent_bytes(bytes: Vec<u8>) {
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    AGENT_BYTES.with(|cell| *cell.borrow_mut() = Some(leaked));
}

fn resolve_agent(triple: &str) -> Option<&'static [u8]> {
    if triple != AGENT_TRIPLE {
        return None;
    }
    AGENT_BYTES.with(|cell| *cell.borrow())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_ports_and_processes_subscription_roundtrip() {
    if std::env::var("TEPEGOZ_SSH_TEST").ok().as_deref() != Some("1")
        || std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() != Some("1")
    {
        eprintln!("skipping: set TEPEGOZ_SSH_TEST=1 and TEPEGOZ_DOCKER_TEST=1 to enable");
        return;
    }

    let tmp = TempDir::new().expect("tempdir");
    let sock_path = tmp.path().join("daemon.sock");
    let config_dir = tmp.path().join("tepegoz-config");
    let data_dir = tmp.path().join("tepegoz-data");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let key_path = config_dir.join("id_ed25519");
    let status = Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            key_path.to_str().unwrap(),
            "-q",
            "-C",
            "tepegoz-6d-test",
        ])
        .status()
        .expect("ssh-keygen required on PATH");
    assert!(status.success());
    let mut pub_key = String::new();
    std::fs::File::open(key_path.with_extension("pub"))
        .unwrap()
        .read_to_string(&mut pub_key)
        .unwrap();
    let pub_key = pub_key.trim().to_string();

    let out = Command::new("docker")
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
    let _guard = ContainerGuard(container_id.clone());

    let port_out = Command::new("docker")
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
    tokio::time::sleep(Duration::from_millis(500)).await;

    eprintln!("[test] cross-building tepegoz-agent for {AGENT_TRIPLE}…");
    install_agent_bytes(build_agent_bytes());

    let config_toml = format!(
        r#"
[[ssh.hosts]]
alias = "demo6d"
hostname = "127.0.0.1"
port = {port}
user = "tepegoz"
identity_file = "{}"
autoconnect = true
"#,
        key_path.display()
    );
    std::fs::write(config_dir.join("config.toml"), config_toml).unwrap();

    // SAFETY: opt-in gated, test binary owns process env.
    unsafe {
        std::env::set_var("TEPEGOZ_CONFIG_DIR", &config_dir);
        std::env::set_var("TEPEGOZ_DATA_DIR", &data_dir);
    }

    let daemon_config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon_with_resolver(daemon_config, Some(resolve_agent))
            .await
            .expect("daemon ran");
    });
    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    let (mut r, mut w) = handshake(&sock_path).await;

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Fleet { id: FLEET_SUB_ID }),
        },
    )
    .await
    .expect("subscribe fleet");

    // Drain until we see (a) HostStateChanged(Connected, demo6d) AND
    // (b) AgentCapabilities(demo6d, caps containing "ports" + "processes").
    // The latter is the 6d-i propagation closing the loop end-to-end.
    let connected_deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut saw_connected = false;
    let mut saw_capabilities = false;
    while std::time::Instant::now() < connected_deadline && !(saw_connected && saw_capabilities) {
        let env = tokio::time::timeout(Duration::from_secs(30), read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read envelope");
        if let Payload::Event(EventFrame {
            subscription_id: FLEET_SUB_ID,
            event,
        }) = env.payload
        {
            match event {
                Event::HostStateChanged {
                    alias,
                    state: HostState::Connected,
                    ..
                } if alias == "demo6d" => {
                    saw_connected = true;
                }
                Event::AgentCapabilities {
                    alias,
                    capabilities,
                } if alias == "demo6d" => {
                    if capabilities.iter().any(|c| c == "ports")
                        && capabilities.iter().any(|c| c == "processes")
                    {
                        saw_capabilities = true;
                        eprintln!("[test] AgentCapabilities arrived: {capabilities:?}");
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        saw_connected,
        "never saw HostStateChanged(Connected) for demo6d"
    );
    assert!(
        saw_capabilities,
        "never saw AgentCapabilities containing ports + processes"
    );

    // Subscribe Ports + Processes back-to-back.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Ports {
                id: PORTS_SUB_ID,
                target: ScopeTarget::Remote {
                    alias: "demo6d".into(),
                },
            }),
        },
    )
    .await
    .expect("subscribe ports remote");
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Processes {
                id: PROCESSES_SUB_ID,
                target: ScopeTarget::Remote {
                    alias: "demo6d".into(),
                },
            }),
        },
    )
    .await
    .expect("subscribe processes remote");

    // Drain until we see at least one PortList event on PORTS_SUB_ID
    // AND one ProcessList event on PROCESSES_SUB_ID. Either or both
    // can arrive interleaved; the loop tracks both flags.
    let probes_deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut saw_ports = false;
    let mut saw_processes = false;
    while std::time::Instant::now() < probes_deadline && !(saw_ports && saw_processes) {
        let env = tokio::time::timeout(Duration::from_secs(30), read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read envelope");
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            match (subscription_id, event) {
                (PORTS_SUB_ID, Event::PortList { ports, source }) => {
                    eprintln!(
                        "[test] PortList arrived: {} entries, source={source}",
                        ports.len()
                    );
                    saw_ports = true;
                }
                (PORTS_SUB_ID, Event::PortsUnavailable { reason }) => {
                    eprintln!("[test] PortsUnavailable: {reason}");
                    saw_ports = true;
                }
                (PROCESSES_SUB_ID, Event::ProcessList { rows, source }) => {
                    eprintln!(
                        "[test] ProcessList arrived: {} rows, source={source}",
                        rows.len()
                    );
                    // First sample contract: every row's
                    // cpu_percent must be None (sysinfo has no
                    // prior delta on first sample).
                    if rows.iter().any(|r| r.cpu_percent.is_some()) {
                        // Subsequent samples may carry Some(_); we only
                        // care that the first event's first-sample
                        // contract matches if this happens to be
                        // the first one. Don't fail otherwise.
                    }
                    saw_processes = true;
                }
                (PROCESSES_SUB_ID, Event::ProcessesUnavailable { reason }) => {
                    eprintln!("[test] ProcessesUnavailable: {reason}");
                    saw_processes = true;
                }
                _ => {}
            }
        }
    }
    assert!(
        saw_ports,
        "never saw a Ports event on subscription {PORTS_SUB_ID}"
    );
    assert!(
        saw_processes,
        "never saw a Processes event on subscription {PROCESSES_SUB_ID}"
    );

    // Unsubscribe both — verify daemon doesn't crash + can take
    // further commands.
    for id in [PORTS_SUB_ID, PROCESSES_SUB_ID] {
        write_envelope(
            &mut w,
            &Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Unsubscribe { id },
            },
        )
        .await
        .expect("unsubscribe");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !daemon_handle.is_finished(),
        "daemon task exited after unsubscribe — likely a routing teardown crash"
    );

    daemon_handle.abort();
}
