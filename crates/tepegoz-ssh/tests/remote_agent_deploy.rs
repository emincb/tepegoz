//! Phase 6 Slice 6b opt-in integration test.
//!
//! Opt-in via `TEPEGOZ_SSH_TEST=1` + `TEPEGOZ_DOCKER_TEST=1` (both
//! required). Provisions an `lscr.io/linuxserver/openssh-server`
//! container with a throwaway ed25519 keypair, builds
//! `tepegoz-agent` for `x86_64-unknown-linux-musl`, exercises the
//! full Slice 6b pipeline (connect → detect_target → deploy_agent
//! → spawn_agent_channel → handshake_agent), and asserts the
//! expected wire-v10 response. Second-run path verifies idempotence:
//! the cache-hit branch short-circuits the upload.
//!
//! Skipped cleanly on default `cargo test` so CI stays green without
//! Docker + ssh-keygen. The agent cross-build step uses plain
//! `cargo build --target x86_64-unknown-linux-musl` — works natively
//! on Linux hosts with the musl target installed; on macOS requires
//! `cargo-zigbuild` on PATH (we use it when present).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use tepegoz_ssh::{
    HostEntry, HostList, HostSource, KnownHostsStore, connect_host, deploy_agent, handshake_agent,
    remote_agent_path, spawn_agent_channel,
};

const AGENT_TRIPLE: &str = "x86_64-unknown-linux-musl";
const PROTOCOL_VERSION: u32 = tepegoz_proto::PROTOCOL_VERSION;

// --------------------------------------------------------------------
// Guards + helpers (mirrored from ssh_smoke.rs; opt-in tests stay
// self-contained so shared refactors can't silently break one)
// --------------------------------------------------------------------

struct ContainerGuard(String);

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
    }
}

fn opt_in() -> bool {
    std::env::var("TEPEGOZ_SSH_TEST").ok().as_deref() == Some("1")
        && std::env::var("TEPEGOZ_DOCKER_TEST").ok().as_deref() == Some("1")
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
            "tepegoz-6b-test",
        ])
        .status()
        .expect("ssh-keygen not found on PATH");
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
            "SUDO_ACCESS=false",
            "-e",
            "PASSWORD_ACCESS=false",
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
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("sshd never bound 127.0.0.1:{port} within {budget:?}");
}

/// Build `tepegoz-agent` for `x86_64-unknown-linux-musl` and return
/// the binary path. Tries `cargo zigbuild` first if on PATH; falls
/// back to plain `cargo build --target` (works on Linux with the
/// musl target installed, fails on macOS without cross-toolchain —
/// an explicit hint fires in that case).
fn build_agent_for_musl() -> PathBuf {
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
         (cross-platform) or add the musl target + linker for your host"
    );

    // Find the workspace root so we can locate target/<triple>/... —
    // when cargo runs the test harness CWD is the package dir.
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
    assert!(
        bin.exists(),
        "expected cross-built agent at {}",
        bin.display()
    );
    bin
}

// --------------------------------------------------------------------
// The test
// --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_agent_deploy_and_handshake() {
    if !opt_in() {
        eprintln!(
            "skipping remote_agent_deploy_and_handshake — set \
             TEPEGOZ_SSH_TEST=1 TEPEGOZ_DOCKER_TEST=1 to enable"
        );
        return;
    }

    // 1. Keypair + sshd container + readiness wait.
    let tmp = TempDir::new().expect("tempdir");
    let (key_path, public_key) = generate_keypair(&tmp);
    let (_guard, port) = start_openssh_container(&public_key);
    wait_for_ssh_ready(port, Duration::from_secs(30)).await;

    // 2. Build the agent binary for the container's target.
    eprintln!("[test] cross-building tepegoz-agent for {AGENT_TRIPLE}…");
    let agent_bin = build_agent_for_musl();
    let agent_bytes = std::fs::read(&agent_bin).expect("read agent binary");
    eprintln!(
        "[test]   → {} ({} bytes)",
        agent_bin.display(),
        agent_bytes.len()
    );

    // 3. Construct a HostList inline + isolated known_hosts so the
    //    user's tepegoz TOFU database is never touched.
    let host = HostEntry {
        alias: "demo6b".into(),
        hostname: "127.0.0.1".into(),
        port,
        user: "tepegoz".into(),
        identity_files: vec![key_path.to_string_lossy().into_owned()],
        proxy_jump: None,
    };
    let list = HostList {
        source: HostSource::None,
        hosts: vec![host],
        autoconnect: std::collections::HashSet::new(),
    };
    let known_hosts_path = tmp.path().join("known_hosts");
    let known_hosts = KnownHostsStore::open_at(&known_hosts_path);

    // 4. First deploy — remote is fresh; deployed_now must be true.
    let session = connect_host("demo6b", &list, &known_hosts)
        .await
        .expect("connect_host");
    let outcome = deploy_agent(&session, &agent_bytes, PROTOCOL_VERSION)
        .await
        .expect("deploy_agent (first attempt)");
    assert!(
        outcome.deployed_now,
        "first deploy must actually upload, not cache-hit"
    );
    assert_eq!(outcome.target.target_triple, AGENT_TRIPLE);
    assert_eq!(outcome.target.os, "linux");
    assert_eq!(outcome.target.arch, "x86_64");
    assert_eq!(
        outcome.remote_path,
        remote_agent_path(&session, PROTOCOL_VERSION).await.unwrap(),
        "outcome's remote_path must match what remote_agent_path reports",
    );

    // 5. Second deploy against the same session — must hit the
    //    cache-hit branch. This is the Slice 6b idempotence contract.
    let second = deploy_agent(&session, &agent_bytes, PROTOCOL_VERSION)
        .await
        .expect("deploy_agent (cache-hit)");
    assert!(
        !second.deployed_now,
        "second deploy must skip upload on byte-identical cache hit"
    );
    assert_eq!(second.sha256_hex, outcome.sha256_hex);

    // 6. Spawn the agent + handshake over the exec channel.
    let mut channel = spawn_agent_channel(&session, &outcome.remote_path)
        .await
        .expect("spawn_agent_channel");
    let info = handshake_agent(&mut channel, PROTOCOL_VERSION)
        .await
        .expect("handshake_agent");

    assert_eq!(info.version, PROTOCOL_VERSION);
    assert_eq!(info.os, "linux", "agent reports linux OS");
    assert_eq!(info.arch, "x86_64", "agent reports x86_64 arch");
    assert!(
        info.capabilities.is_empty(),
        "Slice 6a/6b ship empty capabilities; 6c/d populate. got {:?}",
        info.capabilities
    );

    session.disconnect().await.ok();
}
