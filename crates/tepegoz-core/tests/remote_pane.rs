//! Phase 5 Slice 5d-i acceptance: `OpenPane { target: Remote { alias } }`
//! opens an SSH-backed pty through the daemon, and subsequent
//! `AttachPane` + `SendInput` + `PaneOutput` events flow cleanly.
//!
//! Mirrors `ssh_smoke.rs`'s container provisioning + keypair + env-
//! override setup (reused pattern from Slice 5a + 5c-i). Opt-in gated
//! on `TEPEGOZ_SSH_TEST=1 + TEPEGOZ_DOCKER_TEST=1` — tests need Docker
//! + ssh-keygen on PATH, which CI doesn't have.

use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;
use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneTarget, Payload,
    Subscription,
    codec::{read_envelope, write_envelope},
};

const FLEET_SUB_ID: u64 = 1101;
const PANE_SUB_ID: u64 = 1102;

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
                client_name: "remote_pane_test".into(),
            }),
        },
    )
    .await
    .expect("hello");
    let welcome = read_envelope(&mut r).await.expect("welcome");
    match welcome.payload {
        Payload::Welcome(_) => {}
        other => panic!("expected Welcome, got {other:?}"),
    }
    (r, w)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_pane_open_attach_exec_roundtrip() {
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

    // Generate keypair + start openssh-server container.
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
            "tepegoz-5d-test",
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

    // Land a tepegoz config.toml pointing at the container with
    // autoconnect:false — 5d-i doesn't need autoconnect to open a
    // remote pane; the pane open itself triggers the SSH dial.
    let config_toml = format!(
        r#"
[[ssh.hosts]]
alias = "remote-pane-test"
hostname = "127.0.0.1"
port = {port}
user = "tepegoz"
identity_file = "{}"
"#,
        key_path.display()
    );
    std::fs::write(config_dir.join("config.toml"), config_toml).unwrap();

    // SAFETY: Only this test mutates env; it's behind the opt-in gate
    // so CI never runs it, and the test binary has only this test
    // touching env.
    unsafe {
        std::env::set_var("TEPEGOZ_CONFIG_DIR", &config_dir);
        std::env::set_var("TEPEGOZ_DATA_DIR", &data_dir);
    }

    // Boot daemon.
    let config = tepegoz_core::DaemonConfig {
        socket_path: Some(sock_path.clone()),
    };
    let daemon_handle = tokio::spawn(async move {
        tepegoz_core::run_daemon(config).await.expect("daemon ran");
    });
    wait_for_socket(&sock_path, Duration::from_secs(5)).await;
    let (mut r, mut w) = handshake(&sock_path).await;

    // Subscribe to Fleet so the supervisor is aware of the host (its
    // Disconnected state doesn't block OpenPane — 5d-i opens a fresh
    // SSH session per pane, parallel to any supervisor-managed
    // connection. Subscribe exercises the happy path.).
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Fleet { id: FLEET_SUB_ID }),
        },
    )
    .await
    .expect("subscribe fleet");

    // OpenPane { target: Remote { alias } }. Daemon returns PaneOpened.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(OpenPaneSpec {
                shell: None,
                cwd: None,
                env: Vec::new(),
                rows: 24,
                cols: 80,
                target: PaneTarget::Remote {
                    alias: "remote-pane-test".into(),
                },
            }),
        },
    )
    .await
    .expect("open pane");

    // Drain events until we see PaneOpened. Fleet events are noise
    // here.
    let open_deadline = std::time::Instant::now() + Duration::from_secs(20);
    let pane_info = loop {
        let remaining = open_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "did not receive PaneOpened within 20s"
        );
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read")
            .expect("read");
        match env.payload {
            Payload::PaneOpened(info) => break info,
            Payload::Error(e) => panic!("daemon returned Error: {} ({:?})", e.message, e.kind),
            _ => continue,
        }
    };
    assert!(
        pane_info.alive,
        "pane should be alive immediately after open"
    );
    let pane_id = pane_info.id;
    eprintln!("opened remote pane id={pane_id} shell={}", pane_info.shell);

    // Attach — daemon replays a PaneSnapshot + then live PaneOutput.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id,
                subscription_id: PANE_SUB_ID,
            },
        },
    )
    .await
    .expect("attach");

    // Send a command that produces a known string in stdout.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id,
                data: b"echo tepegoz-marker-5d\n".to_vec(),
            },
        },
    )
    .await
    .expect("send input");

    // Drain PaneSnapshot + PaneOutput events until we see the marker.
    let read_deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut buf = Vec::<u8>::new();
    let mut saw_marker = false;
    while !saw_marker {
        let remaining = read_deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "did not observe marker within 15s; buf:\n{}",
            String::from_utf8_lossy(&buf)
        );
        let env = tokio::time::timeout(remaining, read_envelope(&mut r))
            .await
            .expect("read")
            .expect("read");
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
            && subscription_id == PANE_SUB_ID
        {
            match event {
                Event::PaneSnapshot { scrollback, .. } => {
                    buf.extend_from_slice(&scrollback);
                }
                Event::PaneOutput { data } => {
                    buf.extend_from_slice(&data);
                }
                Event::PaneExit { exit_code } => {
                    panic!(
                        "pane exited before marker; exit_code={exit_code:?}; buf:\n{}",
                        String::from_utf8_lossy(&buf)
                    );
                }
                _ => {}
            }
            if String::from_utf8_lossy(&buf).contains("tepegoz-marker-5d") {
                saw_marker = true;
            }
        }
    }

    // Close the pane. Daemon removes it from remote_pty map.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::ClosePane { pane_id },
        },
    )
    .await
    .expect("close pane");

    daemon_handle.abort();
}
