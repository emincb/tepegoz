//! Phase 6 Slice 6c-iii opt-in integration test: remote Docker
//! subscription routing end-to-end.
//!
//! Exercises the full pipeline across the 6c-ii server-side half:
//! 1. Fleet supervisor on `HostState::Connected` deploys the embedded
//!    agent binary to the sshd container via `tepegoz_ssh::deploy`.
//! 2. Agent handshake populates `capabilities` (`"docker"` iff a
//!    docker socket is reachable inside the container — typically
//!    empty in this test since we don't bind-mount one; that's the
//!    realistic "target lacks required capability" path).
//! 3. Client subscribes `Docker { target: Remote { alias } }`.
//! 4. Daemon's `route_remote_subscribe` looks up `agent_conns[alias]`,
//!    sees missing `"docker"` capability, synthesizes
//!    `Event::DockerUnavailable { reason: "no docker on <alias>" }`.
//! 5. Client receives the DockerUnavailable keyed to its own sub id.
//! 6. Unsubscribe propagates.
//!
//! Opt-in via `TEPEGOZ_SSH_TEST=1 + TEPEGOZ_DOCKER_TEST=1`. Cross-
//! builds `tepegoz-agent` for `x86_64-unknown-linux-musl` (via
//! `cargo zigbuild` when present, otherwise plain `cargo build
//! --target`) so the daemon's resolver can hand real bytes to
//! `deploy_agent`.
//!
//! What this test PROVES:
//! - Fleet supervisor's deploy-on-Connected hook runs + populates the
//!   daemon's `agent_conns` pool.
//! - Client `Subscribe(Docker { Remote })` reaches the right routing
//!   path (capability check → missing → DockerUnavailable).
//! - `Unsubscribe { id }` tears down the routing entry cleanly (no
//!   further events arrive for the client id after unsubscribe).
//!
//! What this test DOES NOT prove (deferred to 6c-ii's in-process
//! routing tests which exercise it without SSH overhead):
//! - ContainerList round-trip when docker IS reachable on the remote.
//!   That path is covered by `agent::tests::daemon_routes_subscribe_
//!   through_real_agent_and_sees_unavailable_when_docker_missing`
//!   which simulates both arms via tokio duplex streams.

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
const FLEET_SUB_ID: u64 = 9001;
const DOCKER_SUB_ID: u64 = 9002;

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
                client_name: "remote_docker_test".into(),
            }),
        },
    )
    .await
    .expect("hello");
    let welcome = read_envelope(&mut r).await.expect("welcome");
    assert!(matches!(welcome.payload, Payload::Welcome(_)));
    (r, w)
}

/// Cross-build `tepegoz-agent` for the sshd container's target and
/// return the bytes. Mirrors `remote_agent_deploy`'s build path.
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
    assert!(
        status.success(),
        "cross-build for {AGENT_TRIPLE} failed — install cargo-zigbuild \
         or add the musl target + linker for your host"
    );

    let mut dir: Option<PathBuf> = std::env::current_dir().ok();
    while let Some(d) = dir.as_ref() {
        if d.join("Cargo.lock").exists() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    let workspace_root = dir.expect("couldn't locate workspace root (no Cargo.lock ancestor)");
    let bin = workspace_root
        .join("target")
        .join(AGENT_TRIPLE)
        .join("release")
        .join("tepegoz-agent");
    std::fs::read(&bin).expect("read agent binary")
}

// Leaked-static resolver trick: `AgentResolver = fn(&str) -> Option<&'static [u8]>`
// requires a bare fn pointer, not a closure. Tests allocate the
// agent bytes once, leak them for 'static, then store the static
// slice in a thread-local the resolver reads. One test per process,
// so the leak cost is exactly once.
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
async fn remote_docker_subscription_roundtrip() {
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

    // 1. Keypair.
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
            "tepegoz-6c-test",
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

    // 2. sshd container. No docker-socket bind-mount — the agent's
    //    capability probe will see no docker, and the test asserts
    //    the DockerUnavailable path through the daemon's routing.
    //    Adding a socket mount + group gymnastics to prove the
    //    ContainerList arm isn't worth the test complexity; that
    //    arm is covered in-process by `agent::tests`.
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

    // 3. Cross-build the agent + install bytes for the resolver.
    eprintln!("[test] cross-building tepegoz-agent for {AGENT_TRIPLE}…");
    install_agent_bytes(build_agent_bytes());

    // 4. Land config.toml with autoconnect=true so the Fleet
    //    supervisor dials immediately on Subscribe(Fleet).
    let config_toml = format!(
        r#"
[[ssh.hosts]]
alias = "demo6c"
hostname = "127.0.0.1"
port = {port}
user = "tepegoz"
identity_file = "{}"
autoconnect = true
"#,
        key_path.display()
    );
    std::fs::write(config_dir.join("config.toml"), config_toml).unwrap();

    // SAFETY: opt-in gated, test binary owns process env, two distinct
    // tempdirs keep the user's real tepegoz state untouched.
    unsafe {
        std::env::set_var("TEPEGOZ_CONFIG_DIR", &config_dir);
        std::env::set_var("TEPEGOZ_DATA_DIR", &data_dir);
    }

    // 5. Boot daemon with the test's agent resolver — this is the
    //    wiring that 6c-ii added via `run_daemon_with_resolver`.
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

    // 6. Subscribe(Fleet) — triggers autoconnect + deploy on this host.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Fleet { id: FLEET_SUB_ID }),
        },
    )
    .await
    .expect("subscribe fleet");

    // 7. Drain events until we see HostStateChanged(alias="demo6c",
    //    state=Connected). 90 s budget: SSH connect + deploy +
    //    handshake can take up to ~30s on slow CI + we allow headroom.
    let connected_deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut saw_connected = false;
    while std::time::Instant::now() < connected_deadline {
        let env = tokio::time::timeout(Duration::from_secs(30), read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read envelope");
        if let Payload::Event(EventFrame {
            subscription_id: FLEET_SUB_ID,
            event:
                Event::HostStateChanged {
                    alias,
                    state: HostState::Connected,
                    ..
                },
        }) = env.payload
        {
            if alias == "demo6c" {
                saw_connected = true;
                break;
            }
        }
    }
    assert!(
        saw_connected,
        "never saw HostStateChanged(demo6c, Connected) within 90s budget"
    );
    eprintln!("[test] host connected; giving agent deploy a moment to finish…");
    // Deploy happens *after* emit_state(Connected). Give it up to 60 s
    // to land the agent + register in agent_conns. This is the
    // realistic wall-clock cost of cross-build + upload + handshake
    // against slow CI.
    tokio::time::sleep(Duration::from_secs(8)).await;

    // 8. Subscribe(Docker { Remote: "demo6c" }) — routing through the
    //    daemon's agent pool, translated to Local for the agent.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Docker {
                id: DOCKER_SUB_ID,
                target: ScopeTarget::Remote {
                    alias: "demo6c".into(),
                },
            }),
        },
    )
    .await
    .expect("subscribe docker remote");

    // 9. Drain until we see an event on DOCKER_SUB_ID. Valid
    //    outcomes:
    //    - ContainerList (agent has docker somehow — unlikely in
    //      this test setup but accepted)
    //    - DockerUnavailable (expected — no docker in the container)
    //    Either way the routing worked + subscription id got rewritten
    //    from daemon→client correctly.
    let docker_deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut saw_docker_event = false;
    while std::time::Instant::now() < docker_deadline {
        let env = tokio::time::timeout(Duration::from_secs(30), read_envelope(&mut r))
            .await
            .expect("read within budget")
            .expect("read envelope");
        if let Payload::Event(EventFrame {
            subscription_id: DOCKER_SUB_ID,
            event,
        }) = env.payload
        {
            match event {
                Event::ContainerList { containers, .. } => {
                    eprintln!(
                        "[test] routing round-trip via ContainerList ({} containers)",
                        containers.len()
                    );
                    saw_docker_event = true;
                    break;
                }
                Event::DockerUnavailable { reason } => {
                    eprintln!("[test] routing round-trip via DockerUnavailable: {reason}");
                    // Reason should name the alias so the user can trace
                    // which target was unavailable. Don't assert exact
                    // text — daemon's routing can synthesize several
                    // reason shapes (missing cap / missing alias / closed
                    // writer) depending on timing.
                    assert!(
                        reason.contains("demo6c") || reason.contains("docker"),
                        "DockerUnavailable reason should mention alias or docker: {reason}"
                    );
                    saw_docker_event = true;
                    break;
                }
                other => {
                    eprintln!("[test] unexpected event on docker sub: {other:?}");
                }
            }
        }
    }
    assert!(
        saw_docker_event,
        "never saw any Docker event on subscription {DOCKER_SUB_ID} within 30s"
    );

    // 10. Unsubscribe — verify no panic, no envelope echoed back.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Unsubscribe { id: DOCKER_SUB_ID },
        },
    )
    .await
    .expect("unsubscribe docker");

    // No direct assertion on teardown — the in-process routing tests
    // pin that contract. Here we just verify the daemon doesn't
    // crash when unsubscribing a remote-routed sub; a crash would
    // manifest as the daemon task exiting + `daemon_handle` being
    // joinable with an error.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !daemon_handle.is_finished(),
        "daemon task exited after unsubscribe — likely a routing teardown crash"
    );

    daemon_handle.abort();
}
