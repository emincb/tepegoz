//! `cargo xtask build-agents` — cross-compile `tepegoz-agent` for the
//! four target triples Decision #3 pins (linux x86_64/aarch64 musl +
//! macOS x86_64/aarch64) and lay the binaries out at
//! `target/agents/<triple>/tepegoz-agent` alongside a `manifest.json`
//! sidecar.
//!
//! The controller's `build.rs` (`crates/tepegoz/build.rs`) picks up
//! each populated target via `include_bytes!` and asserts the
//! manifest's `protocol_version` matches the live
//! `tepegoz-proto::PROTOCOL_VERSION` at compile time. That's the
//! load-bearing drift defence the Phase 6 brief flagged.
//!
//! We invoke `cargo zigbuild` rather than plain `cargo build
//! --target` because the latter doesn't cross-link a Darwin-target
//! SDK from a Linux host (or vice versa). zig's embedded
//! cross-toolchain handles both directions cleanly; same tooling the
//! CI `cross-build` matrix already uses.
//!
//! Non-scope (deferred per Slice 6b brief):
//! - Universal macOS `lipo` of the two darwin binaries into one.
//! - SHA256 checksums in the manifest (adds at release-signing time).
//! - Agent minisign signatures (Phase 10 release pipeline).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

/// Target triples Decision #3 pins. Keep in lockstep with the
/// `TARGETS` array in `crates/tepegoz/build.rs` — they're the two
/// halves of the same "which arches can we deploy to" contract.
const TARGETS: &[&str] = &[
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// Release profile declared in the workspace Cargo.toml — inherits
/// from `release` but with `opt-level = "z"` + `lto = "fat"` for a
/// minimal agent binary (the 4 arches are embedded into every
/// controller binary; every byte counts).
const PROFILE: &str = "release-agent";

pub(crate) fn run() -> Result<()> {
    preflight()?;

    let workspace_root = locate_workspace_root()?;
    let version_file = workspace_root
        .join("crates")
        .join("tepegoz-proto")
        .join("PROTOCOL_VERSION");
    let protocol_version = parse_version(&version_file).with_context(|| {
        format!(
            "reading PROTOCOL_VERSION source of truth at {}",
            version_file.display()
        )
    })?;
    println!(
        "[build-agents] source of truth: protocol v{protocol_version} (from {})",
        version_file.display()
    );

    let agents_root = workspace_root.join("target").join("agents");

    for triple in TARGETS {
        build_one(&workspace_root, &agents_root, triple, protocol_version)?;
    }

    println!("[build-agents] all four targets built + manifests written.");
    println!("[build-agents] layout: {}", agents_root.display());
    Ok(())
}

fn build_one(
    workspace_root: &Path,
    agents_root: &Path,
    triple: &str,
    protocol_version: u32,
) -> Result<()> {
    println!("[build-agents] cross-compiling {triple}…");
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .arg("zigbuild")
        .args(["--package", "tepegoz-agent"])
        .args(["--bin", "tepegoz-agent"])
        .args(["--profile", PROFILE])
        .args(["--target", triple])
        .status()
        .with_context(|| format!("spawn cargo zigbuild for {triple}"))?;
    if !status.success() {
        bail!("cargo zigbuild failed for {triple} (exit: {status})");
    }

    // Cargo + zigbuild write release-agent outputs to
    // `target/<triple>/release-agent/tepegoz-agent` on every platform.
    let source = workspace_root
        .join("target")
        .join(triple)
        .join(PROFILE)
        .join("tepegoz-agent");
    if !source.exists() {
        bail!(
            "expected cargo zigbuild output at {} — did the profile or bin name change?",
            source.display()
        );
    }

    let triple_dir = agents_root.join(triple);
    fs::create_dir_all(&triple_dir)
        .with_context(|| format!("creating agent output dir at {}", triple_dir.display()))?;
    let bin_dest = triple_dir.join("tepegoz-agent");
    fs::copy(&source, &bin_dest)
        .with_context(|| format!("copying {} → {}", source.display(), bin_dest.display()))?;

    // Manifest sidecar. JSON chosen over TOML / rkyv because the
    // controller's build.rs reads it via serde_json — no other
    // consumer exists today, so we pick the reader's preferred
    // format. Fields kept minimal: the controller only checks
    // `protocol_version`; `target_triple` + `built_at_unix_secs`
    // are diagnostic hints for the operator.
    let manifest_path = triple_dir.join("manifest.json");
    let built_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let manifest = format!(
        "{{\n  \"protocol_version\": {protocol_version},\n  \"target_triple\": \"{triple}\",\n  \"built_at_unix_secs\": {built_at}\n}}\n"
    );
    fs::write(&manifest_path, manifest)
        .with_context(|| format!("writing manifest {}", manifest_path.display()))?;

    let bin_size = fs::metadata(&bin_dest).map(|m| m.len()).unwrap_or(0);
    println!(
        "[build-agents] {triple}: {} ({} bytes)",
        bin_dest.display(),
        bin_size
    );

    Ok(())
}

fn preflight() -> Result<()> {
    ensure_on_path(
        "cargo",
        "cargo is required — install Rust via https://rustup.rs and retry",
    )?;
    ensure_on_path(
        "zig",
        "zig is required for cross-compilation — install via https://ziglang.org/download/ (or `brew install zig`)",
    )?;
    // cargo-zigbuild is invoked through cargo, so `cargo zigbuild`
    // succeeds iff the subcommand plugin is on PATH. A standalone
    // `cargo-zigbuild --version` spawnability check sidesteps the
    // "cargo swallows unknown subcommands with a generic message"
    // UX problem.
    ensure_on_path(
        "cargo-zigbuild",
        "cargo-zigbuild is required — install via `cargo install cargo-zigbuild` (https://github.com/rust-cross/cargo-zigbuild)",
    )?;
    Ok(())
}

fn ensure_on_path(binary: &str, hint: &str) -> Result<()> {
    // Same pattern as demo::ensure_on_path — spawnability check, not
    // exit-code check. See `crates/xtask/src/demo.rs` for the full
    // rationale (macOS OpenBSD ssh-keygen case).
    let result = Command::new(binary)
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

fn locate_workspace_root() -> Result<PathBuf> {
    // `cargo xtask` runs with CWD set to the workspace root by
    // convention (cargo treats xtask as a regular workspace member).
    // Verify defensively so a user invoking `./target/debug/xtask`
    // from elsewhere gets a legible error instead of mysteriously-
    // missing file paths.
    let cwd = std::env::current_dir().context("read CWD")?;
    let marker = cwd.join("Cargo.toml");
    if !marker.exists() {
        bail!(
            "`cargo xtask build-agents` must be invoked from the workspace root (no Cargo.toml at {})",
            cwd.display()
        );
    }
    Ok(cwd)
}

fn parse_version(path: &Path) -> Result<u32> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    raw.trim()
        .parse::<u32>()
        .with_context(|| format!("{} must contain a u32 decimal literal", path.display()))
}
