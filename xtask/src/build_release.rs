//! `cargo xtask build-release` — cross-compile the controller binary
//! for all four Decision #3 target triples, produce a universal macOS
//! binary via `lipo` when available, and emit a `SHA256SUMS` index
//! across every artifact. Layout feeds v1.0 Slice R2's GitHub Actions
//! release workflow (upload glob) and Slice R3's install script
//! (checksum verification).
//!
//! ## Artifact layout
//!
//! ```text
//! target/release-bundles/
//!   x86_64-apple-darwin/tepegoz
//!   x86_64-apple-darwin/manifest.json
//!   aarch64-apple-darwin/tepegoz
//!   aarch64-apple-darwin/manifest.json
//!   x86_64-unknown-linux-musl/tepegoz
//!   x86_64-unknown-linux-musl/manifest.json
//!   aarch64-unknown-linux-musl/tepegoz
//!   aarch64-unknown-linux-musl/manifest.json
//!   universal-apple-darwin/tepegoz          # if lipo / llvm-lipo on PATH
//!   universal-apple-darwin/manifest.json
//!   SHA256SUMS
//! ```
//!
//! `manifest.json` — `{ version, target_triple, built_at_unix_secs }`.
//! `version` is `env!("CARGO_PKG_VERSION")` resolved at xtask compile
//! time; xtask + tepegoz both inherit `version.workspace = true`, so
//! the two strings always match. Pre-tag value is 0.0.1; Slice R4
//! bumps to 1.0.0 before the release tag.
//!
//! `SHA256SUMS` — GNU-style `<hex>  <relpath>` (two spaces, text mode)
//! consumable by both `sha256sum -c` (Linux) and `shasum -a 256 -c`
//! (macOS) without flags.
//!
//! ## Step ordering
//!
//! 1. **Preflight toolchain** — two-layer check (rustup targets +
//!    cargo-zigbuild) across all 4 targets via `preflight::check_cross_build_toolchain`.
//!    Zero side effects on failure, mirroring the 6e-prep-3 discipline.
//! 2. **Build agents first** — the controller's `build.rs` drift
//!    check `include_bytes!`'s each populated agent arch at compile
//!    time. Releasing the controller binary without building agents
//!    ships binaries with `None` agent slots, silently breaking
//!    `deploy_agent` on remote hosts. The dependency is wired here
//!    as a function call, not documentation-of-ordering.
//! 3. **Cross-compile tepegoz × 4 triples** via `cargo zigbuild`.
//! 4. **Universal macOS via lipo** — optional locally in R1 (warn +
//!    skip + 4 per-triple binaries still produced); R2's CI workflow
//!    enforces presence on macos-latest and fails red on absence.
//! 5. **SHA256SUMS** covering every produced artifact.
//!
//! ## R1 host stance: macOS only for darwin targets
//!
//! On a Linux host, `preflight::check_cross_build_toolchain` rejects
//! darwin targets up-front because zigbuild's cross-link to Mach-O
//! needs a macOS SDK (SDKROOT) that R1 does not shim (HANDOFF "don't
//! rabbit-hole"). R2's workflow routes darwin targets to macos-latest
//! so the production path never hits this. Running `build-release`
//! locally from a Linux box today means "Linux targets only + explicit
//! preflight bail on the two darwin triples."
//!
//! ## Per-target subcommand: `build` for darwin-on-macOS, `zigbuild` elsewhere
//!
//! On a macOS host, we invoke plain `cargo build --target <triple>` for
//! the two darwin triples — ld64 cross-links both darwin arches natively
//! (macOS aarch64 host → x86_64-apple-darwin works fine). zigbuild's
//! `ar` wrapper has a known interaction with `ring`'s cc-rs-driven
//! assembly that trap-doors through `libring_core_*.a: No such file or
//! directory` on cross-arch darwin builds; falling back to the native
//! toolchain sidesteps the issue without any preprocessing.
//!
//! `cargo zigbuild` is still required for the two musl triples on every
//! host (musl can't be cross-linked via ld64 on macOS, and glibc systems
//! need zigbuild to produce musl-static binaries cleanly). R2's CI
//! workflow inherits this: macos-latest uses plain `cargo build` for
//! darwin + `cargo zigbuild` for musl; ubuntu-latest uses `cargo zigbuild`
//! for musl only and skips darwin targets.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::build_agents;
use crate::preflight::{self, RELEASE_TARGETS};

const PROFILE: &str = "release";
const BIN_NAME: &str = "tepegoz";
const BUNDLE_ROOT_REL: &str = "target/release-bundles";
const UNIVERSAL_DIR: &str = "universal-apple-darwin";

pub(crate) fn run() -> Result<()> {
    let workspace_root = locate_workspace_root()?;

    println!("[build-release] step 1/5: preflight cross-build toolchain (4 targets × 2 layers)");
    preflight::check_cross_build_toolchain(RELEASE_TARGETS)?;
    // Ring cross-build gate — zig 0.16 / llvm-ar 21 regressed archive
    // creation. Separate from the two-layer check because `build-agents`
    // / `demo-phase-6` don't need it (agent has no ring dep).
    preflight::check_zig_ring_compatibility()?;

    println!("[build-release] step 2/5: build embedded agents (prereq for controller build.rs)");
    build_agents::run().context("build-agents (prerequisite) failed")?;

    let bundles_root = workspace_root.join(BUNDLE_ROOT_REL);
    fs::create_dir_all(&bundles_root)
        .with_context(|| format!("creating bundles root at {}", bundles_root.display()))?;

    let version = env!("CARGO_PKG_VERSION");

    println!("[build-release] step 3/5: cross-compile {BIN_NAME} × 4 triples");
    for triple in RELEASE_TARGETS {
        build_one(&workspace_root, &bundles_root, triple, version)?;
    }

    println!("[build-release] step 4/5: universal macOS binary (lipo)");
    let produced_universal = match preflight::find_lipo() {
        Some(tool) => {
            fuse_universal_macos(&bundles_root, tool, version)?;
            true
        }
        None => {
            println!(
                "[build-release]   lipo / llvm-lipo not on PATH — skipping universal macOS binary.\n\
                 [build-release]   4 per-triple binaries still produced. R2's CI workflow treats this as a hard failure on macos-latest."
            );
            false
        }
    };

    println!("[build-release] step 5/5: write SHA256SUMS");
    write_sha256sums(&bundles_root, produced_universal)?;

    let count = RELEASE_TARGETS.len() + usize::from(produced_universal);
    println!("[build-release] done: {count} artifact(s) + SHA256SUMS");
    println!("[build-release] layout: {}", bundles_root.display());
    Ok(())
}

fn build_one(
    workspace_root: &Path,
    bundles_root: &Path,
    triple: &str,
    version: &str,
) -> Result<()> {
    // Pick subcommand per target+host. See module docs for the
    // rationale — in short: zigbuild's ar wrapper trips ring's
    // cc-rs-built asm on cross-arch darwin, so on macOS we use the
    // native ld64 for darwin targets and zigbuild only for musl.
    let is_darwin_target = triple.ends_with("-apple-darwin");
    let subcmd = if cfg!(target_os = "macos") && is_darwin_target {
        "build"
    } else {
        "zigbuild"
    };
    println!("[build-release]   cross-compiling {triple} (cargo {subcmd})…");
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .arg(subcmd)
        .args(["--package", BIN_NAME])
        .args(["--bin", BIN_NAME])
        .args(["--profile", PROFILE])
        .args(["--target", triple])
        .status()
        .with_context(|| format!("spawn cargo {subcmd} for {triple}"))?;
    if !status.success() {
        bail!("cargo {subcmd} failed for {triple} (exit: {status})");
    }

    let source = workspace_root
        .join("target")
        .join(triple)
        .join(PROFILE)
        .join(BIN_NAME);
    if !source.exists() {
        bail!(
            "expected cargo output at {} — did the profile or bin name change?",
            source.display()
        );
    }

    let triple_dir = bundles_root.join(triple);
    fs::create_dir_all(&triple_dir)
        .with_context(|| format!("creating bundle dir {}", triple_dir.display()))?;
    let bin_dest = triple_dir.join(BIN_NAME);
    fs::copy(&source, &bin_dest)
        .with_context(|| format!("copying {} → {}", source.display(), bin_dest.display()))?;

    write_manifest(&triple_dir, triple, version)?;

    let bin_size = fs::metadata(&bin_dest).map(|m| m.len()).unwrap_or(0);
    println!(
        "[build-release]     → {} ({bin_size} bytes)",
        bin_dest.display()
    );
    Ok(())
}

fn fuse_universal_macos(bundles_root: &Path, lipo_tool: &str, version: &str) -> Result<()> {
    let x86_bin = bundles_root.join("x86_64-apple-darwin").join(BIN_NAME);
    let arm64_bin = bundles_root.join("aarch64-apple-darwin").join(BIN_NAME);
    for bin in [&x86_bin, &arm64_bin] {
        if !bin.exists() {
            bail!(
                "universal-apple-darwin requires both darwin binaries — missing {}",
                bin.display()
            );
        }
    }

    let universal_dir = bundles_root.join(UNIVERSAL_DIR);
    fs::create_dir_all(&universal_dir)
        .with_context(|| format!("creating universal dir {}", universal_dir.display()))?;
    let universal_bin = universal_dir.join(BIN_NAME);

    println!("[build-release]   fusing via {lipo_tool}…");
    let status = Command::new(lipo_tool)
        .arg("-create")
        .arg("-output")
        .arg(&universal_bin)
        .arg(&x86_bin)
        .arg(&arm64_bin)
        .status()
        .with_context(|| format!("spawning {lipo_tool}"))?;
    if !status.success() {
        bail!("{lipo_tool} failed to fuse darwin binaries (exit: {status})");
    }

    write_manifest(&universal_dir, UNIVERSAL_DIR, version)?;

    let bin_size = fs::metadata(&universal_bin).map(|m| m.len()).unwrap_or(0);
    println!(
        "[build-release]     → {} ({bin_size} bytes)",
        universal_bin.display()
    );
    Ok(())
}

fn write_manifest(dir: &Path, target_triple: &str, version: &str) -> Result<()> {
    let built_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Match build_agents::build_one's hand-rolled JSON shape: same
    // format, different field set (version + target_triple +
    // built_at_unix_secs vs. protocol_version + target_triple +
    // built_at_unix_secs). No serde to avoid a dep for three fields.
    let manifest = format!(
        "{{\n  \"version\": \"{version}\",\n  \"target_triple\": \"{target_triple}\",\n  \"built_at_unix_secs\": {built_at}\n}}\n"
    );
    let path = dir.join("manifest.json");
    fs::write(&path, manifest).with_context(|| format!("writing manifest {}", path.display()))?;
    Ok(())
}

fn write_sha256sums(bundles_root: &Path, produced_universal: bool) -> Result<()> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for triple in RELEASE_TARGETS {
        let relpath = format!("{triple}/{BIN_NAME}");
        let full = bundles_root.join(triple).join(BIN_NAME);
        let digest = sha256_file(&full)?;
        entries.push((digest, relpath));
    }
    if produced_universal {
        let relpath = format!("{UNIVERSAL_DIR}/{BIN_NAME}");
        let full = bundles_root.join(UNIVERSAL_DIR).join(BIN_NAME);
        let digest = sha256_file(&full)?;
        entries.push((digest, relpath));
    }
    entries.sort_by(|a, b| a.1.cmp(&b.1));

    // GNU sha256sum text-mode format: "<hex>  <relpath>\n" (two spaces
    // between hex and path). `sha256sum -c` on Linux and
    // `shasum -a 256 -c` on macOS both consume this without flags.
    let mut body = String::new();
    for (digest, relpath) in &entries {
        body.push_str(digest);
        body.push_str("  ");
        body.push_str(relpath);
        body.push('\n');
    }
    let path = bundles_root.join("SHA256SUMS");
    fs::write(&path, body).with_context(|| format!("writing SHA256SUMS at {}", path.display()))?;
    println!(
        "[build-release]   → {} ({} entries)",
        path.display(),
        entries.len()
    );
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

fn locate_workspace_root() -> Result<PathBuf> {
    // `cargo xtask` runs with CWD set to the workspace root by
    // convention. Mirrors build_agents::locate_workspace_root — a
    // user invoking `./target/debug/xtask build-release` from
    // elsewhere gets a legible error rather than mysterious "cargo
    // output not found" failures downstream.
    let cwd = std::env::current_dir().context("read CWD")?;
    if !cwd.join("Cargo.toml").exists() {
        bail!(
            "`cargo xtask build-release` must be invoked from the workspace root (no Cargo.toml at {})",
            cwd.display()
        );
    }
    Ok(cwd)
}
