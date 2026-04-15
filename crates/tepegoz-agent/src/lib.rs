//! Tepegöz remote agent — speaks the wire protocol to the controller
//! over stdio.
//!
//! Phase 6 Slice 6a ships only the handshake path:
//!
//! 1. Controller spawns the agent binary on a remote host (`tepegoz-
//!    agent` standalone, or the controller's `tepegoz agent --stdio`
//!    alias for same-arch local invocation).
//! 2. Controller sends `Payload::AgentHandshake { request_id }` as a
//!    length-prefix framed rkyv envelope on the agent's stdin.
//! 3. Agent responds with `Payload::AgentHandshakeResponse { request_id,
//!    version, os, arch, capabilities }` on its stdout.
//! 4. Either side closes the channel.
//!
//! Later slices extend `handle_envelope` with real subscriptions
//! (docker / ports / processes / pty). The handshake dispatch is kept
//! as its own match arm so adding subscriptions doesn't regress it.
//!
//! Version drift: the agent's compiled-in [`tepegoz_proto::PROTOCOL_VERSION`]
//! is the source of truth it reports in the response. The controller's
//! `build.rs` asserts at compile time that every embedded agent's
//! manifest matches the controller's own `PROTOCOL_VERSION`; the
//! runtime handshake is the second line of defense for user-deployed
//! (non-embedded) agents.

use tepegoz_proto::{
    Envelope, ErrorInfo, ErrorKind, PROTOCOL_VERSION, Payload,
    codec::{read_envelope, write_envelope},
};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

/// Run the agent over the process's stdio. Drives the event loop
/// until stdin closes or a fatal read/write error is hit.
pub async fn run_stdio() -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve(stdin, stdout).await
}

/// Core event loop, generic over reader / writer. `run_stdio` wraps
/// this with real stdin / stdout; unit + integration tests drive it
/// with in-memory duplex streams.
pub async fn serve<R, W>(mut reader: R, mut writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let envelope = match read_envelope(&mut reader).await {
            Ok(env) => env,
            Err(e) => {
                // Plain EOF is graceful shutdown. Anything else is a
                // real read error worth surfacing.
                let msg = format!("{e}");
                if msg.contains("early eof") || msg.contains("UnexpectedEof") {
                    debug!("stdin closed — agent exiting");
                    return Ok(());
                }
                return Err(e);
            }
        };

        // Version-drift guard mirrors the daemon↔client handshake
        // shape — reject with a structured Error and close. Do this
        // before payload dispatch so a malformed-but-well-framed
        // envelope can't get past us with the wrong version number
        // attached.
        if envelope.version != PROTOCOL_VERSION {
            let err = Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Error(ErrorInfo {
                    kind: ErrorKind::VersionMismatch,
                    message: format!(
                        "agent built for wire v{}, controller sent v{}",
                        PROTOCOL_VERSION, envelope.version
                    ),
                }),
            };
            // Best-effort send of the mismatch reason; then close.
            let _ = write_envelope(&mut writer, &err).await;
            return Ok(());
        }

        if let Some(response) = handle_envelope(envelope) {
            write_envelope(&mut writer, &response).await?;
        }
    }
}

/// Pure-function dispatch: `envelope in → Option<envelope out>`.
/// `None` means "no response needed" (e.g., a hypothetical
/// fire-and-forget event in a future slice); today every variant we
/// handle produces a response.
///
/// Factored out of `serve` so unit tests can exercise the dispatch
/// table without touching async I/O.
pub(crate) fn handle_envelope(envelope: Envelope) -> Option<Envelope> {
    match envelope.payload {
        Payload::AgentHandshake { request_id } => Some(Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AgentHandshakeResponse {
                request_id,
                version: PROTOCOL_VERSION,
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                // Empty for Slice 6a. Populated as 6c/d add real
                // probes ("docker" / "ports" / "processes" / "pty").
                capabilities: Vec::new(),
            },
        }),
        Payload::Ping => Some(Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Pong,
        }),
        other => {
            // Slice 6a only handles AgentHandshake + Ping. Anything
            // else is a future slice's responsibility; surface as
            // InvalidRequest so the controller sees a legible error
            // rather than the channel going silent.
            warn!(?other, "agent received unhandled payload");
            Some(Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Error(ErrorInfo {
                    kind: ErrorKind::InvalidRequest,
                    message: format!(
                        "agent v{PROTOCOL_VERSION} (Phase 6 Slice 6a) does not yet handle this payload"
                    ),
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_dispatch_echoes_request_id_and_reports_self() {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AgentHandshake { request_id: 7 },
        };
        let response = handle_envelope(env).expect("handshake must produce a response");
        match response.payload {
            Payload::AgentHandshakeResponse {
                request_id,
                version,
                os,
                arch,
                capabilities,
            } => {
                assert_eq!(request_id, 7, "request_id must echo");
                assert_eq!(version, PROTOCOL_VERSION);
                assert_eq!(os, std::env::consts::OS);
                assert_eq!(arch, std::env::consts::ARCH);
                assert!(
                    capabilities.is_empty(),
                    "Slice 6a capabilities are empty; 6c/d will add entries"
                );
            }
            other => panic!("expected AgentHandshakeResponse, got {other:?}"),
        }
    }

    #[test]
    fn ping_dispatch_returns_pong() {
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Ping,
        };
        let response = handle_envelope(env).expect("ping must produce a response");
        assert!(matches!(response.payload, Payload::Pong));
    }

    #[test]
    fn unhandled_payload_surfaces_invalid_request_not_silence() {
        // E.g. the agent receives a `Subscribe(Docker)` — we haven't
        // wired that yet in 6a. The controller must see a legible
        // error rather than the channel appearing to hang.
        let env = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::ListPanes,
        };
        let response = handle_envelope(env).expect("must respond, not silence");
        match response.payload {
            Payload::Error(info) => {
                assert!(matches!(info.kind, ErrorKind::InvalidRequest));
                assert!(
                    info.message.contains("Phase 6 Slice 6a"),
                    "diagnostic must name the slice so the controller can log a useful line"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serve_handles_handshake_and_exits_on_eof() {
        // Two independent duplex channels: `a` acts as the agent's
        // stdin (we write controller→agent envelopes into `a_tx`,
        // server reads from `a_rx`); `b` is the agent's stdout
        // (server writes into `b_tx`, we read from `b_rx`).
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, mut b_rx) = tokio::io::duplex(512);

        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION,
                    payload: Payload::AgentHandshake { request_id: 99 },
                },
            )
            .await
            .unwrap();
            // Dropping `w` closes the channel → agent sees EOF.
        }

        let response = read_envelope(&mut b_rx).await.unwrap();
        match response.payload {
            Payload::AgentHandshakeResponse { request_id, .. } => {
                assert_eq!(request_id, 99);
            }
            other => panic!("expected AgentHandshakeResponse, got {other:?}"),
        }

        let outcome = server.await.unwrap();
        assert!(outcome.is_ok(), "clean EOF is Ok(()), got {outcome:?}");
    }

    #[tokio::test]
    async fn serve_rejects_version_mismatch_and_closes() {
        let (a_tx, a_rx) = tokio::io::duplex(512);
        let (b_tx, mut b_rx) = tokio::io::duplex(512);
        let server = tokio::spawn(async move { serve(a_rx, b_tx).await });

        {
            let mut w = a_tx;
            // Hand-craft an envelope with a wrong version. The
            // controller sending this shape is the post-deploy
            // scenario the build.rs drift check can't catch (agent
            // was installed manually at an older version).
            write_envelope(
                &mut w,
                &Envelope {
                    version: PROTOCOL_VERSION + 1,
                    payload: Payload::Ping,
                },
            )
            .await
            .unwrap();
        }

        let response = read_envelope(&mut b_rx).await.unwrap();
        match response.payload {
            Payload::Error(info) => {
                assert!(matches!(info.kind, ErrorKind::VersionMismatch));
                assert!(info.message.contains("agent built for wire v"));
            }
            other => panic!("expected Error(VersionMismatch), got {other:?}"),
        }

        let outcome = server.await.unwrap();
        assert!(
            outcome.is_ok(),
            "version mismatch closes cleanly, not with an Err"
        );
    }
}
