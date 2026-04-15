//! Phase 4 4d desync reproduction harness. **Opt-in diagnostic.** Gated
//! on `TEPEGOZ_DESYNC_REPRO=1` so CI / default `cargo test` never runs
//! it. The named regression test pinning the actual fix lives in
//! `crates/tepegoz-tui/src/session.rs::tests` and runs on every build.
//!
//! This harness mimics the TUI's pre-fix `session.rs` main-loop shape —
//! `read_envelope` called directly inside a `tokio::select!` alongside
//! tick / stdin / winch branches. With `AsyncReadExt::read_exact`
//! cancellation-unsafe, a firing tick or stdin branch could drop an
//! in-flight read mid-payload, advancing the kernel socket position
//! past bytes the next `read_envelope` would need as a length prefix.
//! The 4d production desync manifested exactly that way. Kept here
//! for exploratory timing scenarios; not a substitute for the
//! default-suite regression test.
//!
//! Run with:
//!   `TEPEGOZ_DESYNC_REPRO=1 RUST_LOG=tepegoz_proto=debug,tepegoz_core=debug
//!   cargo test -p tepegoz-core --test desync_repro -- --nocapture`

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::MissedTickBehavior;

use tepegoz_proto::{
    Envelope, Hello, PROTOCOL_VERSION, Payload, Subscription,
    codec::{read_envelope, write_envelope},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn desync_repro_with_select_tick_interrupting_read_envelope() {
    if std::env::var("TEPEGOZ_DESYNC_REPRO").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TEPEGOZ_DESYNC_REPRO=1 to enable");
        return;
    }

    let tmp = tempfile::TempDir::new().unwrap();
    let sock_path = tmp.path().join("daemon.sock");
    let sock_path_for_daemon = sock_path.clone();
    let daemon = tokio::spawn(async move {
        tepegoz_core::run_daemon(tepegoz_core::DaemonConfig {
            socket_path: Some(sock_path_for_daemon),
        })
        .await
        .expect("daemon ran");
    });
    wait_for_socket(&sock_path, Duration::from_secs(5)).await;

    // Connect + handshake.
    let stream = UnixStream::connect(&sock_path).await.expect("connect");
    let (mut reader, mut writer) = stream.into_split();
    write_envelope(
        &mut writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Hello(Hello {
                client_version: PROTOCOL_VERSION,
                client_name: "desync-repro".into(),
            }),
        },
    )
    .await
    .expect("hello");
    let welcome = read_envelope(&mut reader).await.expect("welcome");
    assert!(matches!(welcome.payload, Payload::Welcome(_)));

    // Open + attach a pane so the daemon produces a live PaneOutput
    // stream (variable-size payloads — largest envelope kind the
    // TUI routinely sees).
    write_envelope(
        &mut writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(tepegoz_proto::OpenPaneSpec {
                target: tepegoz_proto::PaneTarget::Local,
                shell: Some("/bin/sh".into()),
                cwd: None,
                env: vec![],
                rows: 40,
                cols: 120,
            }),
        },
    )
    .await
    .expect("open pane");
    // Drain until we get a PaneOpened.
    let pane_id = loop {
        let env = read_envelope(&mut reader).await.expect("pane opened");
        if let Payload::PaneOpened(info) = env.payload {
            break info.id;
        }
    };
    write_envelope(
        &mut writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id,
                subscription_id: 100,
            },
        },
    )
    .await
    .expect("attach pane");
    // Drive some pty output by typing a command into the shell.
    write_envelope(
        &mut writer,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id,
                data: b"for i in 1 2 3 4 5; do echo line $i; done\n".to_vec(),
            },
        },
    )
    .await
    .expect("send input");

    // Subscribe to the three Phase 4 scopes (plus Docker) concurrently
    // — matches what the TUI does at startup.
    for (id, sub) in [
        (
            2u64,
            Subscription::Docker {
                id: 2,
                target: tepegoz_proto::ScopeTarget::Local,
            },
        ),
        (
            3u64,
            Subscription::Ports {
                id: 3,
                target: tepegoz_proto::ScopeTarget::Local,
            },
        ),
        (
            4u64,
            Subscription::Processes {
                id: 4,
                target: tepegoz_proto::ScopeTarget::Local,
            },
        ),
    ] {
        write_envelope(
            &mut writer,
            &Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Subscribe(sub),
            },
        )
        .await
        .expect("subscribe");
        let _ = id;
    }

    // Reproduce the TUI's select! pattern. 30 Hz tick + read_envelope.
    // Send periodic input to create more write traffic.
    let mut tick = tokio::time::interval(Duration::from_millis(30));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut envelopes_read: u64 = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut input_drip = tokio::time::interval(Duration::from_millis(200));
    input_drip.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let outcome: Result<&'static str, String> = loop {
        if std::time::Instant::now() >= deadline {
            break Ok("timeout without desync — hypothesis not confirmed");
        }
        tokio::select! {
            env = read_envelope(&mut reader) => {
                match env {
                    Ok(_) => {
                        envelopes_read += 1;
                    }
                    Err(e) => {
                        break Err(format!("read_envelope failed after {envelopes_read} envelopes: {e}"));
                    }
                }
            }
            _ = tick.tick() => {
                // do nothing — purely to cancel the read_envelope
                // branch at 30 Hz, matching the TUI's Tick cadence.
            }
            _ = input_drip.tick() => {
                let res = write_envelope(
                    &mut writer,
                    &Envelope {
                        version: PROTOCOL_VERSION,
                        payload: Payload::SendInput {
                            pane_id,
                            data: b"echo hi\n".to_vec(),
                        },
                    },
                ).await;
                if let Err(e) = res {
                    break Err(format!("write_envelope failed after {envelopes_read} envelopes: {e}"));
                }
            }
        }
    };

    eprintln!("desync repro outcome: {outcome:?} (envelopes_read = {envelopes_read})");
    daemon.abort();

    // Don't assert on the outcome here — the point is the eprintln
    // output + the RUST_LOG=debug traces, not test pass/fail.
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
