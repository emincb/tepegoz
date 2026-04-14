//! Phase 5 Slice 5a opt-in integration test.
//!
//! Enabled with `TEPEGOZ_SSH_TEST=1`. Spins up a
//! `lscr.io/linuxserver/openssh-server` container with a freshly
//! generated ed25519 keypair, exercises the full happy path
//! (connect → open_session → exec `echo hello` → read stdout →
//! disconnect), and force-removes the container on drop.
//!
//! Skipped cleanly on default `cargo test` so CI stays green without
//! Docker + ssh-keygen on the CI host.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::AsyncReadExt;

use tepegoz_ssh::{HostEntry, HostList, HostSource, KnownHostsStore, connect_host, open_session};

/// Force-remove the container when the test scope exits.
struct ContainerGuard(String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn opt_in() -> bool {
    std::env::var("TEPEGOZ_SSH_TEST").ok().as_deref() == Some("1")
}

fn generate_keypair(tmp: &TempDir) -> (PathBuf, String) {
    let key_path = tmp.path().join("id_ed25519");
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
            "tepegoz-test",
        ])
        .status()
        .expect("ssh-keygen not found on PATH — required for TEPEGOZ_SSH_TEST");
    assert!(status.success(), "ssh-keygen failed");
    let pub_path = key_path.with_extension("pub");
    let mut pub_buf = String::new();
    std::fs::File::open(&pub_path)
        .unwrap()
        .read_to_string(&mut pub_buf)
        .unwrap();
    (key_path, pub_buf.trim().to_string())
}

fn start_openssh_container(public_key: &str) -> (ContainerGuard, u16) {
    // Image: linuxserver/openssh-server — exposes 2222, accepts
    // `PUBLIC_KEY=...` + `USER_NAME=...` envs to install an authorized
    // key for the given user.
    let output = Command::new("docker")
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
            &format!("PUBLIC_KEY={public_key}"),
            "-p",
            "0:2222",
            "lscr.io/linuxserver/openssh-server:latest",
        ])
        .output()
        .expect("docker run failed — is Docker running?");
    assert!(
        output.status.success(),
        "docker run non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let guard = ContainerGuard(id.clone());

    // Query the mapped host port for 2222/tcp.
    let port_out = Command::new("docker")
        .args(["port", &id, "2222/tcp"])
        .output()
        .unwrap();
    let port_line = String::from_utf8_lossy(&port_out.stdout).trim().to_string();
    let port: u16 = port_line
        .rsplit(':')
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("failed to parse port from `docker port` output: {port_line:?}"));

    (guard, port)
}

async fn wait_for_ssh_ready(port: u16, budget: Duration) {
    // The container needs ~5s to boot sshd + install the key. Poll TCP
    // until the port accepts connections, then give it one more half-
    // second for sshd's banner to be ready.
    let deadline = Instant::now() + budget;
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "openssh-server container did not become reachable on port {port} within {budget:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ssh_connect_exec_roundtrip() {
    if !opt_in() {
        eprintln!("skipping: set TEPEGOZ_SSH_TEST=1 to enable");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let (key_path, public_key) = generate_keypair(&tmp);
    let (_container, port) = start_openssh_container(&public_key);
    wait_for_ssh_ready(port, Duration::from_secs(30)).await;

    // Build a HostList with a single synthetic entry pointing at the
    // container. Don't consult ssh_config or config.toml — this is a
    // pure SSH-layer test.
    let hosts = HostList {
        hosts: vec![HostEntry {
            alias: "integ".to_string(),
            hostname: "127.0.0.1".to_string(),
            user: "tepegoz".to_string(),
            port,
            identity_files: vec![key_path],
            proxy_jump: None,
        }],
        source: HostSource::None,
    };

    let known_hosts_path = tmp.path().join("known_hosts");
    let known_hosts = KnownHostsStore::open_at(&known_hosts_path);

    let session = connect_host("integ", &hosts, &known_hosts)
        .await
        .expect("connect_host should succeed against freshly-provisioned openssh-server");

    let channel = open_session(&session)
        .await
        .expect("open_session should succeed on live connection");
    let mut channel = channel.into_inner();
    channel.exec(true, "echo hello").await.unwrap();

    // Read stdout until EOF or a timeout.
    let mut reader = channel.make_reader();
    let mut out = Vec::new();
    let deadline = Duration::from_secs(5);
    match tokio::time::timeout(deadline, reader.read_to_end(&mut out)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => panic!("channel read error: {e}"),
        Err(_) => panic!("channel read timed out after {deadline:?}"),
    }
    let stdout = String::from_utf8_lossy(&out);
    assert!(
        stdout.contains("hello"),
        "expected 'hello' in stdout, got {stdout:?}"
    );

    session.disconnect().await.ok();

    // TOFU sanity — the known_hosts file should now carry one entry
    // for the container host:port.
    let kh = std::fs::read_to_string(&known_hosts_path).unwrap_or_default();
    assert!(
        kh.contains(&format!(":{port}")) || kh.contains("127.0.0.1"),
        "known_hosts should have persisted the server key; contents: {kh:?}"
    );
}
