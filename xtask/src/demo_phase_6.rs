//! `cargo xtask demo-phase-6 {up,down}` — one-command runner for the
//! Phase 6 Slice 6a local handshake demo.
//!
//! Scope is deliberately tiny: build `tepegoz-agent` for the host
//! target (plain `cargo build`, no zigbuild), spawn it as a
//! subprocess with stdio piped, send a single `AgentHandshake`
//! envelope, read the response, print a legible summary. Slice 6b's
//! demo will extend this with SSH deploy + remote handshake against
//! the existing Phase 5 sshd container fixture.
//!
//! Per the standing demo-tooling rule (memory:
//! `feedback_demo_tooling.md`): every phase's manual-demo gate ships
//! a `cargo xtask demo-<phase>` runner alongside the first
//! meaningful implementation. 6a meets that obligation even though
//! there's no real remote UX to walk yet — the slice's value is the
//! scaffolding, and this demo proves the scaffolding works end-to-
//! end.

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use tepegoz_proto::{
    Envelope, PROTOCOL_VERSION, Payload,
    codec::{read_envelope, write_envelope},
};
use tokio::process::Command as AsyncCommand;

/// Stable path so `down` can clean even when `up` crashed mid-flight.
/// Mirrors the demo-phase-5 convention.
fn demo_root() -> PathBuf {
    std::env::temp_dir().join("tepegoz-demo-phase-6")
}

pub(crate) fn up() -> Result<()> {
    preflight()?;
    let root = demo_root();
    fs::create_dir_all(&root)
        .with_context(|| format!("creating demo root at {}", root.display()))?;

    println!("[demo-phase-6] building tepegoz-agent for the host target…");
    cargo_build_host_agent()?;
    let bin = PathBuf::from("target/debug/tepegoz-agent");
    if !bin.exists() {
        bail!(
            "expected `{}` after cargo build — invoke `cargo xtask demo-phase-6 up` from the workspace root",
            bin.display()
        );
    }

    // Single-thread tokio runtime — the whole demo is a sequential
    // write-then-read against the subprocess; no reason to bring up
    // a multi-threaded reactor.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(drive_handshake(&bin))
}

async fn drive_handshake(bin: &std::path::Path) -> Result<()> {
    println!("[demo-phase-6] spawning agent subprocess…");
    let mut child = AsyncCommand::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    let mut stdin = child.stdin.take().context("agent stdin piped by spawn()")?;
    let mut stdout = child
        .stdout
        .take()
        .context("agent stdout piped by spawn()")?;

    let request_id = 1u64;
    println!("[demo-phase-6] sending AgentHandshake {{ request_id: {request_id} }}…");
    write_envelope(
        &mut stdin,
        &Envelope {
            version: PROTOCOL_VERSION,
            payload: Payload::AgentHandshake { request_id },
        },
    )
    .await
    .context("write handshake envelope")?;

    println!("[demo-phase-6] awaiting AgentHandshakeResponse…");
    let response = tokio::time::timeout(Duration::from_secs(5), read_envelope(&mut stdout))
        .await
        .context("timeout: agent didn't respond within 5 s")?
        .context("decode response envelope")?;

    match response.payload {
        Payload::AgentHandshakeResponse {
            request_id: echoed,
            version,
            os,
            arch,
            capabilities,
        } => {
            println!();
            println!("  agent handshake ✓");
            println!("    request_id:   {echoed}");
            println!("    version:      {version}");
            println!("    os:           {os}");
            println!("    arch:         {arch}");
            if capabilities.is_empty() {
                println!("    capabilities: (none — 6a ships an empty list; 6c/d populate)");
            } else {
                println!("    capabilities: {}", capabilities.join(", "));
            }
            println!();
        }
        Payload::Error(info) => {
            bail!("agent returned Error({:?}): {}", info.kind, info.message);
        }
        other => bail!("expected AgentHandshakeResponse, got {other:?}"),
    }

    // Close stdin → agent exits on EOF.
    drop(stdin);
    drop(stdout);
    let status = child.wait().await.context("wait on agent subprocess")?;
    if !status.success() {
        bail!("agent exited non-zero after handshake: {status}");
    }
    println!("[demo-phase-6] agent exited cleanly. Done.");
    Ok(())
}

pub(crate) fn down() -> Result<()> {
    // Slice 6a has no persistent fixtures — the agent subprocess
    // exits as part of `up`. `down` stays in the subcommand surface
    // for shape parity with demo-phase-5 + to absorb cleanup of the
    // tempdir if later slices grow one.
    let root = demo_root();
    if root.exists() {
        fs::remove_dir_all(&root)
            .with_context(|| format!("removing demo root {}", root.display()))?;
        println!("[demo-phase-6] removed {}", root.display());
    } else {
        println!("[demo-phase-6] nothing to tear down (no fixture present).");
    }
    Ok(())
}

fn preflight() -> Result<()> {
    ensure_on_path(
        "cargo",
        "cargo is required — install Rust via https://rustup.rs and retry",
    )
}

fn ensure_on_path(binary: &str, hint: &str) -> Result<()> {
    let result = std::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("`{binary}` not found on PATH. {hint}")
        }
        Err(e) => bail!("failed to probe `{binary}`: {e}. {hint}"),
    }
}

fn cargo_build_host_agent() -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--package",
            "tepegoz-agent",
            "--bin",
            "tepegoz-agent",
        ])
        .status()
        .context("spawning cargo build")?;
    if !status.success() {
        bail!("cargo build for tepegoz-agent failed — fix the compile error and retry");
    }
    Ok(())
}
