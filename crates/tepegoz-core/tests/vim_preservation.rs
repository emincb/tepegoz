//! Phase 3 Slice C1.5: vt100 reconstruction of a vim-style pty stream.
//!
//! The TUI's pty tile (per `docs/DECISIONS.md#7`) renders by feeding
//! pane bytes through a `vt100::Parser` and projecting the parser's
//! screen buffer into a ratatui widget within the tile's `Rect`. This
//! integration test exercises the byte-level path end-to-end:
//!
//!   - spawn the daemon
//!   - open a `/bin/sh` pane
//!   - have the shell emit vim-style escape sequences (alt-screen entry,
//!     clear, cursor positioning, marker text) via `printf`
//!   - collect every `PaneSnapshot` / `PaneOutput` chunk on our sub
//!   - feed all accumulated bytes into a 24×80 `vt100::Parser`
//!   - assert the parser's alt-screen cell at the marker's `(row, col)`
//!     contains the marker's first character
//!
//! The automated proxy for "a real terminal redraws vim correctly"
//! has shifted from the C1 synthetic re-attach (where we could only
//! check that bytes survived the replay) to "the vt100 parser reads
//! the same stream and lands the marker at the cell we asked it to."
//! The C1.5b pty tile renderer reads exactly that Screen buffer, so a
//! passing test here is strong evidence vim will render correctly
//! inside the pty tile. The remaining risk lives in terminal-emulator
//! color/font rendering (not a byte-level concern) and is covered by
//! the C1.5c manual demo.
//!
//! `pane_unsubscribe.rs` keeps the daemon-side `Unsubscribe`
//! regression coverage; that's a separate invariant, unaffected by
//! this file's repurpose.

use std::path::Path;
use std::time::Duration;

use tokio::net::UnixStream;

use tepegoz_proto::{
    Envelope, Event, EventFrame, Hello, OpenPaneSpec, PROTOCOL_VERSION, PaneInfo, Payload,
    codec::{read_envelope, write_envelope},
};

const PANE_SUB: u64 = 200;
const MARKER: &str = "TEPEGOZ_VIM_TEST_MARKER";

/// 1-indexed row / column targeted by the CSI `ESC [ R ; C H` sequence.
/// vt100's `Screen::cell(row, col)` is 0-indexed, so we assert at
/// `(MARKER_ROW - 1, MARKER_COL - 1)`.
const MARKER_ROW: u16 = 5;
const MARKER_COL: u16 = 10;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vt100_parser_reconstructs_vim_style_screen_from_pty_bytes() {
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

    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AttachPane {
                pane_id: pane.id,
                subscription_id: PANE_SUB,
            },
        },
    )
    .await
    .expect("attach");

    // Disable echo so the shell prompt + the printf invocation itself
    // don't contaminate the alt-screen assertion. Use OCTAL `\NNN`
    // escapes (POSIX); hex `\xNN` is a GNU extension that dash on
    // Ubuntu doesn't accept — see docs/OPERATIONS.md "POSIX printf
    // portability".
    let input = format!(
        "stty -echo; printf '\\033[?1049h\\033[2J\\033[{};{}H{}'\n",
        MARKER_ROW, MARKER_COL, MARKER
    );
    write_envelope(
        &mut w,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::SendInput {
                pane_id: pane.id,
                data: input.into_bytes(),
            },
        },
    )
    .await
    .expect("input");

    // Drain PaneSnapshot + PaneOutput chunks until our accumulated byte
    // stream, when fed to a vt100 parser, has the marker rendered at
    // the expected cell — or we time out.
    let assertion = tokio::time::timeout(
        Duration::from_secs(5),
        drain_until_vt100_has_marker(&mut r, PANE_SUB, MARKER_ROW - 1, MARKER_COL - 1),
    )
    .await
    .expect(
        "timed out waiting for vt100 to reconstruct the marker. Either the shell's \
         printf didn't emit the expected bytes (check POSIX portability) or the \
         daemon's pty broadcast path dropped chunks.",
    );
    assertion.expect("vt100 reconstruction");

    daemon_handle.abort();
}

/// Feed accumulated pane bytes into a `vt100::Parser` until the
/// marker's first character appears at `(row, col)` on the parser's
/// screen. Errors only on stream end / protocol error; caller wraps
/// with `tokio::time::timeout` for the deadline.
async fn drain_until_vt100_has_marker(
    r: &mut tokio::net::unix::OwnedReadHalf,
    target_sub_id: u64,
    row: u16,
    col: u16,
) -> Result<(), String> {
    let mut parser = vt100::Parser::new(24, 80, 1000);
    let marker_first = MARKER
        .chars()
        .next()
        .expect("MARKER is non-empty")
        .to_string();
    let mut total_bytes: usize = 0;

    loop {
        // Short per-read timeout so we loop and check the assertion
        // condition frequently rather than blocking forever on the
        // socket after the marker has landed. The outer wrapper
        // enforces the overall deadline.
        let env = match tokio::time::timeout(Duration::from_millis(250), read_envelope(r)).await {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => return Err(format!("read error: {e}")),
            Err(_) => {
                // No envelope in 250 ms. Re-check the assertion in
                // case our last chunk was already enough; otherwise
                // keep waiting.
                if vt100_cell_matches(&parser, row, col, &marker_first) {
                    return Ok(());
                }
                continue;
            }
        };
        if let Payload::Event(EventFrame {
            subscription_id,
            event,
        }) = env.payload
        {
            if subscription_id == target_sub_id {
                match event {
                    Event::PaneSnapshot { scrollback, .. } => {
                        total_bytes += scrollback.len();
                        parser.process(&scrollback);
                    }
                    Event::PaneOutput { data } => {
                        total_bytes += data.len();
                        parser.process(&data);
                    }
                    _ => {}
                }
            }
        }
        if vt100_cell_matches(&parser, row, col, &marker_first) {
            // Extra assertion: the full marker string should appear
            // starting at `col` on the target row — gives a better
            // diagnostic than just checking the first char.
            let reconstructed = vt100_row_substring(&parser, row, col, MARKER.len() as u16);
            if reconstructed != MARKER {
                return Err(format!(
                    "first char of marker landed at ({row}, {col}) but full row reads {reconstructed:?} \
                     (expected {MARKER:?}); {total_bytes} pty bytes processed so far"
                ));
            }
            return Ok(());
        }
    }
}

fn vt100_cell_matches(parser: &vt100::Parser, row: u16, col: u16, expected: &str) -> bool {
    parser
        .screen()
        .cell(row, col)
        .map(|c| c.contents() == expected)
        .unwrap_or(false)
}

fn vt100_row_substring(parser: &vt100::Parser, row: u16, start_col: u16, len: u16) -> String {
    let screen = parser.screen();
    (0..len)
        .filter_map(|offset| screen.cell(row, start_col + offset))
        .map(|c| c.contents())
        .collect()
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
                target: tepegoz_proto::PaneTarget::Local,
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
