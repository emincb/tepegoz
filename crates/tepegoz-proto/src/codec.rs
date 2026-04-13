//! Length-prefixed rkyv framing over any `AsyncRead` / `AsyncWrite`.
//!
//! Frame layout: `[4-byte big-endian u32 length] [rkyv bytes]`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Envelope;

/// Maximum frame size. Defends against malformed or hostile length prefixes.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Encode an [`Envelope`] to rkyv and write it with a length prefix.
pub async fn write_envelope<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> anyhow::Result<()> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(envelope)
        .map_err(|e| anyhow::anyhow!("rkyv serialize: {e}"))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| anyhow::anyhow!("envelope too large: {} bytes", bytes.len()))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length prefix and decode the following rkyv payload into an [`Envelope`].
///
/// Validates with bytecheck on every call — the proto crate does not assume
/// a trust boundary. Callers on the trusted local Unix socket can later
/// switch to an unchecked fast path once profiling justifies it.
pub async fn read_envelope<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Envelope> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_SIZE {
        anyhow::bail!("frame size {len} exceeds max {MAX_FRAME_SIZE}");
    }

    // rkyv's access path requires aligned storage. Read into a plain Vec,
    // then copy into an AlignedVec. The copy is negligible at our rates.
    let mut raw = vec![0u8; len];
    reader.read_exact(&mut raw).await?;

    let mut aligned = rkyv::util::AlignedVec::<16>::with_capacity(len);
    aligned.extend_from_slice(&raw);

    rkyv::from_bytes::<Envelope, rkyv::rancor::Error>(&aligned)
        .map_err(|e| anyhow::anyhow!("rkyv deserialize: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, EventFrame, Hello, PROTOCOL_VERSION, Payload, StatusSnapshot};

    #[tokio::test]
    async fn hello_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(4096);

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Hello(Hello {
                client_version: PROTOCOL_VERSION,
                client_name: "test-client".into(),
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        assert_eq!(decoded.version, PROTOCOL_VERSION);
        match decoded.payload {
            Payload::Hello(h) => {
                assert_eq!(h.client_version, PROTOCOL_VERSION);
                assert_eq!(h.client_name, "test-client");
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_event_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(4096);

        let snap = StatusSnapshot {
            daemon_pid: 12345,
            daemon_version: "0.0.1".into(),
            started_at_unix_millis: 1_700_000_000_000,
            uptime_seconds: 42,
            clients_now: 1,
            clients_total: 3,
            events_sent: 99,
            socket_path: "/tmp/tepegoz-501/daemon.sock".into(),
            panes_open: 0,
        };

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 7,
                event: Event::Status(snap.clone()),
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::Status(s),
            }) => {
                assert_eq!(subscription_id, 7);
                assert_eq!(s.daemon_pid, snap.daemon_pid);
                assert_eq!(s.uptime_seconds, snap.uptime_seconds);
                assert_eq!(s.clients_total, snap.clients_total);
                assert_eq!(s.events_sent, snap.events_sent);
                assert_eq!(s.socket_path, snap.socket_path);
            }
            other => panic!("expected Event(Status), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn frame_too_large_errors_before_allocation() {
        // Write a len prefix that exceeds MAX_FRAME_SIZE, then zero bytes.
        let (mut a, mut b) = tokio::io::duplex(1024);
        let huge: u32 = (MAX_FRAME_SIZE + 1) as u32;
        use tokio::io::AsyncWriteExt;
        a.write_all(&huge.to_be_bytes()).await.unwrap();
        drop(a);
        let err = read_envelope(&mut b).await.unwrap_err();
        assert!(err.to_string().contains("exceeds max"), "got: {err}");
    }
}
