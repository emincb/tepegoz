//! Phase 3 Slice C2 gate: vim-style preservation across the synthetic
//! re-attach.
//!
//! The TUI's Scope→Pane mode switch (Slice C1) cancels the previous
//! AttachPane subscription and sends a fresh one; the daemon replays
//! current scrollback as `Event::PaneSnapshot`. The user's terminal then
//! receives those bytes and is expected to redraw the pane (which,
//! crucially, may be in vim's alternate screen with cursor positioning
//! state that vim won't re-emit unless prompted).
//!
//! **Eyeball confirmation gap:** the actual question — "does my real
//! terminal emulator render vim correctly after these bytes hit it?" — is
//! a manual demo per `docs/OPERATIONS.md`. This test is the strongest
//! automated proxy: it drives a real `/bin/sh` pane, emits vim-style
//! escape sequences (alt-screen entry, clear, cursor positioning, marker
//! text) via `printf`, then exercises the C1 synthetic re-attach pattern
//! and verifies the new `PaneSnapshot` contains the bytes the terminal
//! will need to faithfully redraw.
//!
//! If this test passes, the *daemon* is doing the right thing at the byte
//! level. The remaining risk lives in the terminal-emulator-specific
//! interpretation of those bytes — see CTO §3 for the fallback options
//! (Resize-after-attach, Ctrl-L, or keep-AttachPane-alive across mode
//! switches) if the eyeball demo finds problems.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

const PANE_SUB_1: u64 = 200;
const PANE_SUB_2: u64 = 201;

/// Vim-style escape sequences. We don't run vim itself because vim's
/// exact byte output depends on vim version and TERM negotiation —
/// flaky for a CI test. The synthetic stream below covers the same
/// invariants that matter for the snapshot replay:
///   - alt-screen entry (`ESC[?1049h`)
///   - clear screen (`ESC[2J`)
///   - cursor positioning (`ESC[5;10H`)
///   - marker text the user typed
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const CURSOR_POS: &[u8] = b"\x1b[5;10H";
const MARKER: &[u8] = b"TEPEGOZ_VIM_TEST_MARKER";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vim_style_state_survives_synthetic_reattach() {
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
    let pane = open_pane(&mut r, &mut w).await;

    // Attach #1, then ask the shell to emit a vim-style sequence:
    // `printf '\x1b[?1049h\x1b[2J\x1b[5;10HTEPEGOZ_VIM_TEST_MARKER'`.
    // We use printf's `\xNN` shorthand so the actual escape bytes hit
    // the pty via the shell, exactly as vim's own writes would.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id: pane.id,
                subscription_id: PANE_SUB_1,
            },
        },
    )
    .await
    .expect("attach 1");

    // Disable echo so the shell prompt + the printf invocation don't
    // contaminate the snapshot we'll be asserting against. The marker is
    // literal in printf's argument so it WILL appear once in the echoed
    // command and once in the actual output — we'd have to filter that.
    // Easier: `stty -echo` first, then printf.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id: pane.id,
                data:
                    b"stty -echo; printf '\\x1b[?1049h\\x1b[2J\\x1b[5;10HTEPEGOZ_VIM_TEST_MARKER'\n"
                        .to_vec(),
            },
        },
    )
    .await
    .expect("input");

    // Wait until the marker shows up on PANE_SUB_1.
    drain_until_all_markers(
        &mut r,
        PANE_SUB_1,
        &[ALT_SCREEN_ENTER, CURSOR_POS, MARKER],
        Duration::from_secs(5),
    )
    .await
    .expect("vim-style sequence must reach the pane on PANE_SUB_1");

    // ---- Simulate the C1 mode switch ----
    // Unsubscribe(prev) + AttachPane(new) — this is what the TUI does on
    // Scope→Pane. The daemon must replay the full current scrollback
    // (including the alt-screen entry, the clear, the cursor positioning,
    // and the marker) as a fresh PaneSnapshot on the new sub.
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Unsubscribe { id: PANE_SUB_1 },
        },
    )
    .await
    .expect("unsubscribe");

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id: pane.id,
                subscription_id: PANE_SUB_2,
            },
        },
    )
    .await
    .expect("attach 2");

    // The PaneSnapshot for PANE_SUB_2 (and any subsequent PaneOutput)
    // should contain all three vim-style markers — that's the byte-level
    // invariant the terminal needs to redraw vim correctly.
    drain_until_all_markers(
        &mut r,
        PANE_SUB_2,
        &[ALT_SCREEN_ENTER, CURSOR_POS, MARKER],
        Duration::from_secs(5),
    )
    .await
    .expect(
        "vim-preservation FAILED: the new PaneSnapshot must contain alt-screen entry, \
         cursor positioning, AND the marker text. Without all three, a real terminal \
         can't redraw vim's screen state correctly across the mode switch. Per CTO §3, \
         fallback options are: (a) send Resize after re-attach to force vim's redraw, \
         (b) emit Ctrl-L equivalent, (c) keep AttachPane alive across mode switches.",
    );

    daemon_handle.abort();
}

// ---- protocol helpers ----

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
            client_name: "vim-preservation-test".into(),
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

async fn open_pane(
    r: &mut tokio::net::unix::OwnedReadHalf,
    w: &mut tokio::net::unix::OwnedWriteHalf,
) -> PaneInfo {
    write_envelope(
        w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::OpenPane(OpenPaneSpec {
                shell: Some("/bin/sh".into()),
                cwd: None,
                env: Vec::new(),
                rows: 24,
                cols: 80,
            }),
        },
    )
    .await
    .expect("openpane write");
    let rep = read_envelope(r).await.expect("pane response");
    match rep.payload {
        Payload::PaneOpened(info) => info,
        Payload::Error(e) => panic!("open failed: {:?} {}", e.kind, e.message),
        other => panic!("expected PaneOpened, got {other:?}"),
    }
}

/// Drain pane events on `target_sub_id` until every needle in `needles` has
/// been observed in the accumulated byte stream. Each needle must appear at
/// least once. Times out otherwise.
async fn drain_until_all_markers(
    r: &mut tokio::net::unix::OwnedReadHalf,
    target_sub_id: u64,
    needles: &[&[u8]],
    timeout: Duration,
) -> Result<(), String> {
    let mut accumulator: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if needles.iter().all(|n| contains(&accumulator, n)) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let missing: Vec<String> = needles
                .iter()
                .filter(|n| !contains(&accumulator, n))
                .map(|n| format!("{n:?}"))
                .collect();
            return Err(format!(
                "timed out; missing needles: {missing:?} from a stream of {} bytes",
                accumulator.len()
            ));
        }
        let env = match tokio::time::timeout(remaining, read_envelope(r)).await {
            Ok(Ok(e)) => e,
            _ => continue,
        };
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            if subscription_id == target_sub_id {
                match event {
                    Event::PaneSnapshot { scrollback, .. } => {
                        accumulator.extend_from_slice(&scrollback);
                    }
                    Event::PaneOutput { data } => {
                        accumulator.extend_from_slice(&data);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
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
