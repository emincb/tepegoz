//! Length-prefixed rkyv framing over any `AsyncRead` / `AsyncWrite`.
//!
//! Frame layout: `[4-byte big-endian u32 length] [rkyv bytes]`.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::debug;

use crate::{Envelope, Event, Payload};

/// Maximum frame size. Defends against malformed or hostile length prefixes.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Stable discriminant string for a [`Payload`] — added during the Phase 4
/// 4d wire-desync investigation and kept as a permanent diagnostic helper.
/// If a future desync surfaces, per-envelope call sites can log this to
/// attribute the payload kind without reaching for `{:?}` (which allocates
/// and unfolds the whole payload tree).
///
/// Currently unused in the hot path; reinstate at a future diagnostic site
/// rather than recreating it from scratch.
#[allow(dead_code)]
fn payload_variant(p: &Payload) -> &'static str {
    match p {
        Payload::Hello(_) => "Hello",
        Payload::Ping => "Ping",
        Payload::Subscribe(_) => "Subscribe",
        Payload::Unsubscribe { .. } => "Unsubscribe",
        Payload::OpenPane(_) => "OpenPane",
        Payload::AttachPane { .. } => "AttachPane",
        Payload::ClosePane { .. } => "ClosePane",
        Payload::ListPanes => "ListPanes",
        Payload::SendInput { .. } => "SendInput",
        Payload::ResizePane { .. } => "ResizePane",
        Payload::DockerAction(_) => "DockerAction",
        Payload::Welcome(_) => "Welcome",
        Payload::Pong => "Pong",
        Payload::Event(frame) => match &frame.event {
            Event::Status(_) => "Event::Status",
            Event::PaneSnapshot { .. } => "Event::PaneSnapshot",
            Event::PaneOutput { .. } => "Event::PaneOutput",
            Event::PaneExit { .. } => "Event::PaneExit",
            Event::PaneLagged { .. } => "Event::PaneLagged",
            Event::ContainerList { .. } => "Event::ContainerList",
            Event::DockerUnavailable { .. } => "Event::DockerUnavailable",
            Event::ContainerLog { .. } => "Event::ContainerLog",
            Event::ContainerStats(_) => "Event::ContainerStats",
            Event::DockerStreamEnded { .. } => "Event::DockerStreamEnded",
            Event::PortList { .. } => "Event::PortList",
            Event::PortsUnavailable { .. } => "Event::PortsUnavailable",
            Event::ProcessList { .. } => "Event::ProcessList",
            Event::ProcessesUnavailable { .. } => "Event::ProcessesUnavailable",
            Event::HostList { .. } => "Event::HostList",
            Event::HostStateChanged { .. } => "Event::HostStateChanged",
        },
        Payload::PaneOpened(_) => "PaneOpened",
        Payload::PaneList { .. } => "PaneList",
        Payload::DockerActionResult(_) => "DockerActionResult",
        Payload::Error(_) => "Error",
    }
}

/// Encode an [`Envelope`] to rkyv and write it with a length prefix.
pub async fn write_envelope<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> anyhow::Result<()> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(envelope)
        .map_err(|e| anyhow::anyhow!("rkyv serialize: {e}"))?;
    let len = u32::try_from(bytes.len())
        .map_err(|_| anyhow::anyhow!("envelope too large: {} bytes", bytes.len()))?;

    // Defend against AlignedVec's Deref returning bytes past the logical
    // length — that would make `write_all` write more than the length
    // prefix promises and desync the client. The assertion falsified the
    // AlignedVec-padding hypothesis during the Phase 4 4d wire-desync
    // investigation; keeping it as a cheap dev-build invariant check.
    let slice: &[u8] = &bytes;
    debug_assert_eq!(
        slice.len(),
        bytes.len(),
        "AlignedVec deref len must match bytes.len()"
    );

    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(slice).await?;
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
        // DIAGNOSTIC (Phase 4 4d desync investigation): dump the raw
        // length-prefix bytes + try to read and dump the next 32 bytes
        // of the stream so we can see what valid payload we're
        // mid-stream of when the desync fires. Connection is about
        // to error out anyway — free to consume.
        let mut peek_buf = [0u8; 32];
        let peek_result = reader.read_exact(&mut peek_buf).await;
        let peek_hex = match peek_result {
            Ok(_) => hex_dump(&peek_buf),
            Err(e) => format!("(failed to read next 32 bytes: {e})"),
        };
        debug!(
            len_buf_hex = %hex_dump(&len_buf),
            len_u32_be = len,
            next_32_bytes_hex = %peek_hex,
            "read_envelope: length prefix exceeds MAX_FRAME_SIZE — wire desync"
        );
        anyhow::bail!(
            "frame size {len} exceeds max {MAX_FRAME_SIZE}. \
             len_buf=[{len_buf_hex}] next32=[{next}]",
            len_buf_hex = hex_dump(&len_buf),
            next = peek_hex,
        );
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

/// Hex-dump a byte slice as lowercase space-separated pairs. Diagnostic
/// only; not on any hot path.
fn hex_dump(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    async fn docker_container_list_event_roundtrip() {
        use crate::{DockerContainer, DockerPort, KeyValue};

        let (mut a, mut b) = tokio::io::duplex(8192);

        let containers = vec![DockerContainer {
            id: "abc123".into(),
            names: vec!["/webapp".into()],
            image: "nginx:latest".into(),
            image_id: "sha256:deadbeef".into(),
            command: "nginx -g daemon off;".into(),
            created_unix_secs: 1_700_000_000,
            state: "running".into(),
            status: "Up 5 minutes".into(),
            ports: vec![DockerPort {
                ip: "0.0.0.0".into(),
                private_port: 80,
                public_port: 8080,
                protocol: "tcp".into(),
            }],
            labels: vec![KeyValue {
                key: "com.docker.compose.project".into(),
                value: "myapp".into(),
            }],
        }];

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 9,
                event: Event::ContainerList {
                    containers: containers.clone(),
                    engine_source: "Docker Desktop (/Users/me/.docker/run/docker.sock)".into(),
                },
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event:
                    Event::ContainerList {
                        containers: c,
                        engine_source,
                    },
            }) => {
                assert_eq!(subscription_id, 9);
                assert_eq!(c, containers, "container list survived rkyv roundtrip");
                assert!(engine_source.contains("Docker Desktop"));
            }
            other => panic!("expected Event(ContainerList), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn docker_unavailable_event_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(4096);

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 11,
                event: Event::DockerUnavailable {
                    reason: "docker engine unreachable. Tried:\n  - Docker Desktop: socket file not found".into(),
                },
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::DockerUnavailable { reason },
            }) => {
                assert_eq!(subscription_id, 11);
                assert!(reason.contains("Docker Desktop"));
            }
            other => panic!("expected Event(DockerUnavailable), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_docker_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(crate::Subscription::Docker { id: 42 }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Subscribe(crate::Subscription::Docker { id }) => assert_eq!(id, 42),
            other => panic!("expected Subscribe(Docker), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn docker_action_roundtrip_preserves_request_id_and_kind() {
        use crate::{DockerActionKind, DockerActionRequest};
        let (mut a, mut b) = tokio::io::duplex(1024);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerAction(DockerActionRequest {
                request_id: 17,
                container_id: "abc123".into(),
                kind: DockerActionKind::Restart,
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::DockerAction(req) => {
                assert_eq!(req.request_id, 17);
                assert_eq!(req.container_id, "abc123");
                assert_eq!(req.kind, DockerActionKind::Restart);
            }
            other => panic!("expected DockerAction, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn docker_action_result_roundtrip_failure_reason() {
        use crate::{DockerActionKind, DockerActionOutcome, DockerActionResult};
        let (mut a, mut b) = tokio::io::duplex(2048);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::DockerActionResult(DockerActionResult {
                request_id: 17,
                container_id: "abc123".into(),
                kind: DockerActionKind::Stop,
                outcome: DockerActionOutcome::Failure {
                    reason: "container is not running".into(),
                },
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::DockerActionResult(res) => {
                assert_eq!(res.request_id, 17);
                assert_eq!(res.container_id, "abc123");
                assert_eq!(res.kind, DockerActionKind::Stop);
                match res.outcome {
                    DockerActionOutcome::Failure { reason } => {
                        assert_eq!(reason, "container is not running");
                    }
                    other => panic!("expected Failure, got {other:?}"),
                }
            }
            other => panic!("expected DockerActionResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_docker_logs_roundtrip() {
        use crate::Subscription;
        let (mut a, mut b) = tokio::io::duplex(2048);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::DockerLogs {
                id: 99,
                container_id: "abc".into(),
                follow: true,
                tail_lines: 200,
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Subscribe(Subscription::DockerLogs {
                id,
                container_id,
                follow,
                tail_lines,
            }) => {
                assert_eq!(id, 99);
                assert_eq!(container_id, "abc");
                assert!(follow);
                assert_eq!(tail_lines, 200);
            }
            other => panic!("expected Subscribe(DockerLogs), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn container_log_event_roundtrip() {
        use crate::LogStream;
        let (mut a, mut b) = tokio::io::duplex(2048);
        let payload_bytes = b"hello stderr line\n".to_vec();
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 5,
                event: Event::ContainerLog {
                    stream: LogStream::Stderr,
                    data: payload_bytes.clone(),
                },
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::ContainerLog { stream, data },
            }) => {
                assert_eq!(subscription_id, 5);
                assert_eq!(stream, LogStream::Stderr);
                assert_eq!(data, payload_bytes);
            }
            other => panic!("expected Event(ContainerLog), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn container_stats_event_roundtrip() {
        use crate::DockerStats;
        let (mut a, mut b) = tokio::io::duplex(1024);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 6,
                event: Event::ContainerStats(DockerStats {
                    cpu_percent: 12.5,
                    mem_bytes: 1_073_741_824,
                    mem_limit_bytes: 8_589_934_592,
                }),
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::ContainerStats(s),
            }) => {
                assert_eq!(subscription_id, 6);
                assert!((s.cpu_percent - 12.5).abs() < f32::EPSILON);
                assert_eq!(s.mem_bytes, 1_073_741_824);
                assert_eq!(s.mem_limit_bytes, 8_589_934_592);
            }
            other => panic!("expected Event(ContainerStats), got {other:?}"),
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

    #[tokio::test]
    async fn subscribe_ports_roundtrip() {
        use crate::Subscription;
        let (mut a, mut b) = tokio::io::duplex(1024);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Ports { id: 71 }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Subscribe(Subscription::Ports { id }) => assert_eq!(id, 71),
            other => panic!("expected Subscribe(Ports), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn port_list_event_roundtrip() {
        use crate::ProbePort;
        let (mut a, mut b) = tokio::io::duplex(8192);

        let ports = vec![
            ProbePort {
                local_ip: "0.0.0.0".into(),
                local_port: 8080,
                protocol: "tcp".into(),
                pid: 4242,
                process_name: "nginx".into(),
                container_id: Some("abc123def456".into()),
                partial: false,
            },
            ProbePort {
                local_ip: "127.0.0.1".into(),
                local_port: 3000,
                protocol: "tcp".into(),
                pid: 0,
                process_name: String::new(),
                container_id: None,
                partial: true,
            },
        ];

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 71,
                event: Event::PortList {
                    ports: ports.clone(),
                    source: "linux-procfs".into(),
                },
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::PortList { ports: p, source },
            }) => {
                assert_eq!(subscription_id, 71);
                assert_eq!(p, ports, "port list survived rkyv roundtrip");
                assert_eq!(source, "linux-procfs");
            }
            other => panic!("expected Event(PortList), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ports_unavailable_event_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(2048);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 71,
                event: Event::PortsUnavailable {
                    reason: "procfs: permission denied (/proc/net/tcp requires root)".into(),
                },
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::PortsUnavailable { reason },
            }) => {
                assert_eq!(subscription_id, 71);
                assert!(reason.contains("procfs"));
            }
            other => panic!("expected Event(PortsUnavailable), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_processes_roundtrip() {
        use crate::Subscription;
        let (mut a, mut b) = tokio::io::duplex(1024);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Processes { id: 83 }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Subscribe(Subscription::Processes { id }) => assert_eq!(id, 83),
            other => panic!("expected Subscribe(Processes), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn process_list_event_roundtrip_preserves_first_sample_cpu_none() {
        use crate::ProbeProcess;
        let (mut a, mut b) = tokio::io::duplex(8192);

        // Mixed rows: first-sample row (cpu_percent = None), a subsequent-
        // sample row (Some(12.5)), and a partial row (command empty).
        // All three must round-trip intact — `None` must not collapse to
        // `Some(0.0)` or the TUI can't distinguish "not yet measured" from
        // "idle" per CTO's 4b note.
        let rows = vec![
            ProbeProcess {
                pid: 4242,
                parent_pid: 1,
                start_time_unix_secs: 1_700_000_000,
                command: "nginx: worker process".into(),
                cpu_percent: None,
                mem_bytes: 8_388_608,
                partial: false,
            },
            ProbeProcess {
                pid: 4243,
                parent_pid: 4242,
                start_time_unix_secs: 1_700_000_100,
                command: "bun run dev".into(),
                cpu_percent: Some(12.5),
                mem_bytes: 134_217_728,
                partial: false,
            },
            ProbeProcess {
                pid: 99999,
                parent_pid: 0,
                start_time_unix_secs: 0,
                command: String::new(),
                cpu_percent: None,
                mem_bytes: 0,
                partial: true,
            },
        ];

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 83,
                event: Event::ProcessList {
                    rows: rows.clone(),
                    source: "sysinfo".into(),
                },
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::ProcessList { rows: r, source },
            }) => {
                assert_eq!(subscription_id, 83);
                assert_eq!(r, rows, "process list survived rkyv roundtrip");
                assert_eq!(source, "sysinfo");
                assert!(
                    r[0].cpu_percent.is_none(),
                    "first-sample None must not collapse"
                );
                assert_eq!(r[1].cpu_percent, Some(12.5));
                assert!(r[2].partial);
            }
            other => panic!("expected Event(ProcessList), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn processes_unavailable_event_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(2048);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 83,
                event: Event::ProcessesUnavailable {
                    reason: "sysinfo refresh panicked: OS error".into(),
                },
            }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::ProcessesUnavailable { reason },
            }) => {
                assert_eq!(subscription_id, 83);
                assert!(reason.contains("sysinfo"));
            }
            other => panic!("expected Event(ProcessesUnavailable), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_fleet_roundtrip() {
        use crate::Subscription;
        let (mut a, mut b) = tokio::io::duplex(1024);
        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Subscribe(Subscription::Fleet { id: 97 }),
        };
        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();
        match decoded.payload {
            Payload::Subscribe(Subscription::Fleet { id }) => assert_eq!(id, 97),
            other => panic!("expected Subscribe(Fleet), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn host_list_event_roundtrip_preserves_all_fields() {
        use crate::HostEntry;
        let (mut a, mut b) = tokio::io::duplex(8192);

        let hosts = vec![
            HostEntry {
                alias: "staging".into(),
                hostname: "staging.internal".into(),
                user: "alice".into(),
                port: 22,
                identity_files: vec!["/home/alice/.ssh/id_ed25519_staging".into()],
                proxy_jump: None,
            },
            HostEntry {
                alias: "bench-01".into(),
                hostname: "10.0.2.5".into(),
                user: "bob".into(),
                port: 2222,
                identity_files: vec![],
                proxy_jump: Some("bastion".into()),
            },
        ];

        let original = Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::Event(EventFrame {
                subscription_id: 97,
                event: Event::HostList {
                    hosts: hosts.clone(),
                    source: "ssh_config (/home/alice/.ssh/config)".into(),
                },
            }),
        };

        write_envelope(&mut a, &original).await.unwrap();
        let decoded = read_envelope(&mut b).await.unwrap();

        match decoded.payload {
            Payload::Event(EventFrame {
                subscription_id,
                event: Event::HostList { hosts: h, source },
            }) => {
                assert_eq!(subscription_id, 97);
                assert_eq!(h, hosts, "host list survived rkyv roundtrip");
                assert!(source.contains("ssh_config"));
            }
            other => panic!("expected Event(HostList), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn host_state_changed_event_roundtrip_covers_all_variants() {
        use crate::HostState;
        for state in [
            HostState::Disconnected,
            HostState::Connecting,
            HostState::Connected,
            HostState::Degraded,
            HostState::AuthFailed,
            HostState::HostKeyMismatch,
            HostState::AgentNotDeployed,
            HostState::AgentVersionMismatch,
        ] {
            let (mut a, mut b) = tokio::io::duplex(1024);
            let original = Envelope {
                version: PROTOCOL_VERSION,
                payload: Payload::Event(EventFrame {
                    subscription_id: 97,
                    event: Event::HostStateChanged {
                        alias: "staging".into(),
                        state,
                    },
                }),
            };
            write_envelope(&mut a, &original).await.unwrap();
            let decoded = read_envelope(&mut b).await.unwrap();
            match decoded.payload {
                Payload::Event(EventFrame {
                    subscription_id,
                    event:
                        Event::HostStateChanged {
                            alias,
                            state: decoded_state,
                        },
                }) => {
                    assert_eq!(subscription_id, 97);
                    assert_eq!(alias, "staging");
                    assert_eq!(decoded_state, state);
                }
                other => panic!("expected Event(HostStateChanged), got {other:?}"),
            }
        }
    }
}
