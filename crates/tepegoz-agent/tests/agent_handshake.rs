//! Phase 6 Slice 6a integration test (extended in Slice 6c-proper for
//! capability probing).
//!
//! Spawns the real `tepegoz-agent` binary as a subprocess with piped
//! stdio, drives a handshake through the wire codec, and asserts the
//! response carries the controller's expected shape. Capability list
//! is environment-dependent — "docker" appears iff a local engine
//! answers within the probe timeout — so we only assert that every
//! entry is a known capability string.
//!
//! Cargo exposes the binary's built path via `CARGO_BIN_EXE_tepegoz-
//! agent` during integration-test builds, so we don't need to hard-
//! code a target-dir location or re-invoke cargo.

use std::process::Stdio;
use std::time::Duration;

use tepegoz_proto::{
    Envelope, PROTOCOL_VERSION, Payload,
    codec::{read_envelope, write_envelope},
};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_tepegoz-agent");

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_handshake_roundtrip() {
    // Spawn the agent with piped stdio. Stderr inherits so any
    // tracing output lands in the test runner's stderr on failure.
    let mut child = Command::new(BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn tepegoz-agent subprocess");

    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = child.stdout.take().expect("stdout piped");

    // Send the handshake. `request_id` is arbitrary — the assertion
    // below verifies the agent echoes it.
    let request_id = 0xfeedface_u64;
    write_envelope(
        &mut stdin,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AgentHandshake { request_id },
        },
    )
    .await
    .expect("write handshake envelope");

    // Read the response with a generous-but-bounded timeout so a
    // hung agent doesn't wedge CI.
    let response = tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut stdout))
        .await
        .expect("agent responded within 5 s")
        .expect("decode response envelope");

    assert_eq!(
        response.version, PROTOCOL_VERSION,
        "response envelope must carry the same protocol version"
    );
    match response.payload {
        Payload::AgentHandshakeResponse {
            request_id: echoed,
            version,
            os,
            arch,
            capabilities,
        } => {
            assert_eq!(echoed, request_id, "request_id must round-trip");
            assert_eq!(version, PROTOCOL_VERSION);
            assert_eq!(
                os,
                std::env::consts::OS,
                "agent's reported os must match the test-process host"
            );
            assert_eq!(
                arch,
                std::env::consts::ARCH,
                "agent's reported arch must match the test-process host"
            );
            // 6d-ii: ports + processes always present on supported
            // platforms; docker is env-dependent.
            assert!(
                capabilities.contains(&"ports".to_string()),
                "ports capability must always be present (got {capabilities:?})"
            );
            assert!(
                capabilities.contains(&"processes".to_string()),
                "processes capability must always be present (got {capabilities:?})"
            );
            for cap in &capabilities {
                assert!(
                    matches!(cap.as_str(), "docker" | "ports" | "processes"),
                    "unexpected capability {cap:?} — known set is {{docker, ports, processes}}"
                );
            }
        }
        other => panic!("expected AgentHandshakeResponse, got {other:?}"),
    }

    // Close stdin — the agent should exit cleanly on EOF.
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("agent exited within 5 s of EOF")
        .expect("wait on agent subprocess");
    assert!(
        status.success(),
        "agent must exit 0 on clean EOF; got {status:?}"
    );
}
